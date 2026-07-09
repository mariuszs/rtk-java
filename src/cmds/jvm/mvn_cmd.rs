//! Filters Maven (`mvn`) command output — test results, build errors.
//!
//! State machine parser for `mvn test` output with states:
//! Preamble -> Testing -> Summary -> Done.
//! Strips thousands of noise lines to compact failure reports (99%+ savings).

use crate::cmds::jvm::surefire_reports::{self, FailureKind, SurefireResult, TestFailure, TestSummary};
use crate::core::runner;
use crate::core::tracking;
use crate::core::utils::{exit_code_from_status, resolved_command, strip_ansi, truncate};
use anyhow::{Context, Result};
use lazy_static::lazy_static;
use regex::Regex;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

const INFO_TAG: &str = "[INFO]";
const ERROR_TAG: &str = "[ERROR]";
const WARNING_TAG: &str = "[WARNING]";

const MAX_FAILURES_PER_SOURCE: usize = 10;

lazy_static! {
    static ref TESTS_RUN_RE: Regex =
        Regex::new(r"Tests run:\s*(\d+),\s*Failures:\s*(\d+),\s*Errors:\s*(\d+),\s*Skipped:\s*(\d+)")
            .unwrap();
    static ref FAILURE_HEADER_RE: Regex =
        Regex::new(r"^\[ERROR\]\s+(\S+\.\S+)\s+--\s+Time elapsed:.*<<<\s+(FAILURE|ERROR)!")
            .unwrap();
    static ref TOTAL_TIME_RE: Regex =
        Regex::new(r"Total time:\s+(.+)")
            .unwrap();
}

/// Parse `Total time: <value>` from a Maven line already passed through
/// `strip_maven_prefix`. Returns the trimmed value borrowed from the input.
fn parse_total_time(stripped: &str) -> Option<&str> {
    TOTAL_TIME_RE
        .captures(stripped)
        .and_then(|caps| caps.get(1).map(|m| m.as_str().trim()))
}

lazy_static! {
    static ref VERSION_MANAGED_RE: Regex =
        Regex::new(r"\s*\(version managed from [^)]+\)")
            .unwrap();
    /// Code generator config params: `dialect                : POSTGRES_15`
    /// Also matches parens/hyphens in keys: `interfaces (immutable) : false`
    static ref CODEGEN_CONFIG_RE: Regex =
        Regex::new(r"^[\w][\w\s()\-]*\s{2,}:(\s|$)")
            .unwrap();
    /// Frontend bundle size lines: `257.55 kB  build/static/js/main.js`
    static ref BUNDLE_SIZE_RE: Regex =
        Regex::new(r"^\d[\d.]*\s+[kKMG]?B\s")
            .unwrap();
    /// Reactor Build Order line, two accepted formats:
    ///   - `<module name>   [pom|jar|war|ear]` (classic, verbose mode)
    ///   - `<module name>   <version>`           (mvn 3.9.x default, where
    ///                                            `<version>` starts with a digit)
    /// Expects input already passed through `strip_maven_prefix`.
    static ref REACTOR_BUILD_ORDER_RE: Regex =
        Regex::new(r"^\S.*\s+(?:\[(?:pom|jar|war|ear)\]|\d\S*)\s*$")
            .unwrap();
    /// Reactor Summary per-module line:
    /// `<module> ...... SUCCESS [  0.234 s]` (also FAILURE, SKIPPED).
    /// Expects input already passed through `strip_maven_prefix`. Capture
    /// groups: 1=name, 2=status. The trailing `[time]` segment is required
    /// to match but not captured — we don't use per-module timing.
    static ref REACTOR_SUMMARY_LINE_RE: Regex =
        Regex::new(r"^(\S.*?)\s*\.{2,}\s*(SUCCESS|FAILURE|SKIPPED)\s*\[[^\]]*\]\s*$")
            .unwrap();
    /// Javac error location: `[ERROR] /path/File.java:[line,col] message`
    /// Capture groups: 1=path, 2=line, 3=col. Used for error dedup.
    static ref COMPILE_ERROR_LOCATION_RE: Regex =
        Regex::new(r"^\[ERROR\]\s+(\S+?):\[(\d+),(\d+)\]")
            .unwrap();
    /// Javac context line attached to a previous error:
    /// `[ERROR]   symbol:   ...`, `[ERROR]   location: ...`, required/found/reason.
    static ref COMPILE_ERROR_CONTEXT_RE: Regex =
        Regex::new(r"^\[ERROR\]\s+(?:symbol|location|required|found|reason):")
            .unwrap();
    /// Checkstyle violation lines:
    /// `[ERROR] <path>:[<line>[,<col>]] (<category>) <Rule>: <msg>`
    /// (also matches `[WARN]` severity for plugins configured with warn level).
    static ref CHECKSTYLE_VIOLATION_RE: Regex =
        Regex::new(r"^\[(?:ERROR|WARN)\] (.+?):\[(\d+)(?:,(\d+))?\] \(\w+\) (\w+): (.+)$")
            .unwrap();
    /// mvnd / maven 3.9+ extension-loader noise:
    /// `[INFO] Loaded 22539 auto-discovered prefixes for remote repository central (...)`
    static ref PREFIX_LOAD_RE: Regex =
        Regex::new(r"Loaded\s+\d+\s+auto-discovered prefixes").unwrap();
    /// maven-enforcer per-rule `passed` notification — one line per rule on
    /// every successful build. Format: `Rule <n>: <fqcn> passed`. Expects
    /// input already passed through `strip_maven_prefix`.
    static ref ENFORCER_RULE_PASSED_RE: Regex =
        Regex::new(r"^Rule \d+: \S+ passed").unwrap();
}

/// JVM warning lines emitted by Java 24+ (restricted methods, native access,
/// terminally-deprecated Unsafe). These have NO `[INFO]/[ERROR]/[WARNING]`
/// prefix — Maven wrappers surface them as bare text. They are always noise
/// for our purposes.
const JVM_WARNING_PREFIXES: &[&str] = &[
    "WARNING: A restricted method",
    "WARNING: java.lang.System::",
    "WARNING: sun.misc.Unsafe",
    "WARNING: Use --enable-native-access",
    "WARNING: Restricted methods will be blocked",
    "WARNING: A terminally deprecated",
    "WARNING: Please consider reporting",
];

/// Bare-text banner emitted by `mvn --version` / `mvn -V` before the build
/// starts. No `[INFO]/[ERROR]` prefix. Matched by prefix on the already
/// `trim_start`-ed line.
const MVN_ENV_BANNER_PREFIXES: &[&str] = &[
    "Apache Maven ",
    "Maven home:",
    "Java version:",
    "Default locale:",
    "OS name:",
];

lazy_static! {
    /// java.util.logging header emitted by GCP libraries near end of build:
    ///   `Apr 18, 2026 12:19:27 AM com.google.auth.oauth2.X warnY`
    static ref JUL_LOG_HEADER_RE: Regex =
        Regex::new(r"^\w{3} \d{1,2}, \d{4} \d{1,2}:\d{2}:\d{2} [AP]M ")
            .unwrap();
}

/// Bare-text WARNING lines emitted by non-JVM libraries (artifactregistry-
/// maven-wagon, google-auth-library, etc.) without any `[INFO]/[ERROR]`
/// Maven tag. Always non-actionable compared to real compile errors.
const BARE_PLUGIN_WARNING_PREFIXES: &[&str] = &[
    "WARNING: Your application has authenticated",
];

/// Returns true for mvn startup / JVM / os-detection noise that is not
/// command-specific (applies to compile, checkstyle, and most goals).
/// Expects a raw (non-trimmed) line or a trimmed line — both work.
fn is_mvn_startup_noise(line: &str) -> bool {
    let t = line.trim_start();

    // mvnd / maven 3.9+ extension-loader progress
    if PREFIX_LOAD_RE.is_match(t) {
        return true;
    }

    // JVM restricted-method / native-access warnings (no Maven prefix)
    for p in JVM_WARNING_PREFIXES {
        if t.starts_with(p) {
            return true;
        }
    }

    // `mvn -V` environment banner
    for p in MVN_ENV_BANNER_PREFIXES {
        if t.starts_with(p) {
            return true;
        }
    }

    // SLF4J static-binder complaints on startup (`SLF4J: Failed to load …`).
    if t.starts_with("SLF4J:") {
        return true;
    }

    // java.util.logging header line from GCP auth libraries
    if JUL_LOG_HEADER_RE.is_match(t) {
        return true;
    }

    // Bare-text plugin WARNING lines that carry no Maven tag
    for p in BARE_PLUGIN_WARNING_PREFIXES {
        if t.starts_with(p) {
            return true;
        }
    }

    // os-maven-plugin detection output: `[INFO] os.detected.name: linux` etc.
    if t.starts_with("[INFO] os.detected") {
        return true;
    }

    false
}

/// Which Maven binary to invoke. `Mvn` auto-detects the `mvnw` wrapper and
/// falls back to system `mvn`; `Mvnd` always uses the Maven Daemon (`mvnd`),
/// which is incompatible with the wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MvnBinary {
    Mvn,
    Mvnd,
}

impl MvnBinary {
    fn as_str(self) -> &'static str {
        match self {
            MvnBinary::Mvn => "mvn",
            MvnBinary::Mvnd => "mvnd",
        }
    }
}

impl std::fmt::Display for MvnBinary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Goals that share the test-output state machine (surefire + failsafe + XML
/// enrichment). `Test`/`Verify` are the canonical test goals; the lifecycle
/// goals (`integration-test`, `package`, `install`, `deploy`) run through the
/// full test phase too, so they reuse the same filter while running their OWN
/// goal name (not "verify"). The filter is goal-agnostic — `goal` is only a
/// display label (`mvn <goal>: N passed`) — so new variants format correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestLikeGoal {
    Test,
    Verify,
    IntegrationTest,
    Package,
    Install,
    Deploy,
    /// Direct `failsafe:integration-test` plugin goal — runs failsafe without
    /// the surrounding lifecycle, must be invoked verbatim.
    FailsafeIntegrationTest,
    /// Direct `failsafe:verify` plugin goal.
    FailsafeVerify,
    /// Direct `surefire:test` plugin goal.
    SurefireTest,
}

impl TestLikeGoal {
    fn as_str(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Verify => "verify",
            Self::IntegrationTest => "integration-test",
            Self::Package => "package",
            Self::Install => "install",
            Self::Deploy => "deploy",
            Self::FailsafeIntegrationTest => "failsafe:integration-test",
            Self::FailsafeVerify => "failsafe:verify",
            Self::SurefireTest => "surefire:test",
        }
    }

    /// Filesystem-safe slug for tee labels — plugin goals contain ':'.
    fn tee_slug(self) -> &'static str {
        match self {
            Self::FailsafeIntegrationTest => "failsafe_integration-test",
            Self::FailsafeVerify => "failsafe_verify",
            Self::SurefireTest => "surefire_test",
            _ => self.as_str(),
        }
    }
}

/// Build the `(tool_name, tee_label)` pair used for tracking a run of
/// `<binary> <goal>`. Tee labels use `_` separators (filesystem-safe); tool
/// names use a space (human-readable in `rtk gain`). Kept as a single helper
/// so the `{binary}`/`_` convention stays consistent across all mvn/mvnd runs.
fn mvn_labels(binary: MvnBinary, goal: &str, tee_slug: &str) -> (String, String) {
    (format!("{binary} {goal}"), format!("{binary}_{tee_slug}"))
}

/// Build the base command for the selected binary. For `Mvn`, auto-detects the
/// `mvnw` wrapper and falls back to system `mvn`. For `Mvnd`, always invokes
/// `mvnd` directly (the daemon does not use wrapper scripts).
fn mvn_command(binary: MvnBinary) -> std::process::Command {
    match binary {
        MvnBinary::Mvn => {
            if Path::new("mvnw").exists() {
                resolved_command("./mvnw")
            } else {
                resolved_command("mvn")
            }
        }
        MvnBinary::Mvnd => resolved_command("mvnd"),
    }
}


fn run_tests_like(
    binary: MvnBinary,
    goal: TestLikeGoal,
    args: &[String],
    verbose: u8,
) -> Result<i32> {
    let goal_str = goal.as_str();

    let mut cmd = mvn_command(binary);
    cmd.arg(goal_str);

    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: {binary} {goal_str} {}", args.join(" "));
    }

    let started_at = std::time::SystemTime::now();
    let cwd = std::env::current_dir().unwrap_or_else(|e| {
        eprintln!("rtk {binary}: could not determine cwd: {e}");
        std::path::PathBuf::from(".")
    });
    let app_pkgs = crate::cmds::jvm::pom_groupid::detect(&cwd);

    let cwd_for_filter = cwd.clone();

    let (tool_name, tee_label) = mvn_labels(binary, goal_str, goal.tee_slug());
    let tee_label_for_filter = tee_label.clone();
    runner::run_filtered(
        cmd,
        &tool_name,
        &args.join(" "),
        move |raw: &str| {
            // Thread `app_packages` into the stdout parser so its framework
            // frame filtering matches the XML enrichment's behavior — keeps
            // the fallback (no XML reports) format consistent with XML output.
            let filtered = filter_mvn_tests_with_goal(raw, goal_str, &app_pkgs);
            let enriched =
                enrich_with_reports(&filtered, &cwd_for_filter, started_at, &app_pkgs, goal_str);
            finalize_enriched(enriched, &tee_label_for_filter)
        },
        runner::RunOptions::with_tee(&tee_label),
    )
}

/// Shared implementation for compile-phase-like goals: runs `<binary> <goal> <args>`
/// through `filter_mvn_compile`. Used by `dispatch` to route `compile`,
/// `process-classes`, and `test-compile` through the same filter while
/// preserving the original goal name in the invocation and in the tracking
/// label.
fn run_compile_like(binary: MvnBinary, goal: &str, args: &[String], verbose: u8) -> Result<i32> {
    let tee_slug = COMPILE_LIKE_GOALS
        .iter()
        .find_map(|&(g, slug)| (g == goal).then_some(slug))
        .expect("goal must be in COMPILE_LIKE_GOALS — gated by route_goal");
    run_simple_goal(binary, goal, tee_slug, filter_mvn_compile, args, verbose)
}

pub fn run_checkstyle(binary: MvnBinary, args: &[String], verbose: u8) -> Result<i32> {
    run_simple_goal(
        binary,
        "checkstyle:check",
        "checkstyle",
        filter_mvn_checkstyle,
        args,
        verbose,
    )
}

pub fn run_clean(binary: MvnBinary, args: &[String], verbose: u8) -> Result<i32> {
    run_simple_goal(binary, "clean", "clean", filter_mvn_clean, args, verbose)
}

pub fn run_dep_tree(binary: MvnBinary, args: &[String], verbose: u8) -> Result<i32> {
    run_simple_goal(
        binary,
        "dependency:tree",
        "dep_tree",
        filter_mvn_dep_tree,
        args,
        verbose,
    )
}

pub fn run_dep_list(binary: MvnBinary, args: &[String], verbose: u8) -> Result<i32> {
    run_simple_goal(
        binary,
        "dependency:list",
        "dep_list",
        filter_mvn_dep_list,
        args,
        verbose,
    )
}

/// Shared runner for single-filter goals: spawns `<binary> <goal> <args>`,
/// pipes stdout through `filter`, tees raw output under `tee_slug`. Only used
/// by goals with no XML enrichment — `run_tests_like` handles test/verify.
fn run_simple_goal(
    binary: MvnBinary,
    goal: &str,
    tee_slug: &str,
    filter: fn(&str) -> String,
    args: &[String],
    verbose: u8,
) -> Result<i32> {
    let mut cmd = mvn_command(binary);
    cmd.arg(goal);
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: {binary} {goal} {}", args.join(" "));
    }

    let (tool_name, tee_label) = mvn_labels(binary, goal, tee_slug);
    runner::run_filtered(
        cmd,
        &tool_name,
        &args.join(" "),
        filter,
        runner::RunOptions::with_tee(&tee_label),
    )
}

/// Goals whose output looks like `mvn compile` (same noise profile: plugin
/// codegen, npm lifecycle, Liquibase, Docker). Tuples are `(goal, tee_slug)`
/// — tool names are prefixed with the active binary at runtime to keep mvn
/// and mvnd metrics separate in `rtk gain`.
const COMPILE_LIKE_GOALS: &[(&str, &str)] = &[
    ("compile", "compile"),
    ("process-classes", "process_classes"),
    ("test-compile", "test_compile"),
];

/// Routing decision for a raw mvn goal token. Pure function, easy to unit-test.
#[derive(Debug, PartialEq, Eq)]
enum GoalRouting {
    /// Test-output goals (test/verify + lifecycle goals that run the test
    /// phase), filtered by the shared surefire/failsafe state machine.
    TestsLike(TestLikeGoal),
    Clean,
    Compile,
    Checkstyle,
    DepTree,
    DepList,
    /// Stream unchanged via `status()`; tracked for metrics only.
    Passthrough,
}

/// Maven lifecycle phases (clean + default + site lifecycles). A bare token
/// matching one of these is a goal even without a `:`.
const MAVEN_PHASES: &[&str] = &[
    "pre-clean", "clean", "post-clean",
    "validate", "initialize",
    "generate-sources", "process-sources", "generate-resources", "process-resources",
    "compile", "process-classes",
    "generate-test-sources", "process-test-sources", "generate-test-resources",
    "process-test-resources", "test-compile", "process-test-classes",
    "test", "prepare-package", "package",
    "pre-integration-test", "integration-test", "post-integration-test",
    "verify", "install", "deploy",
    "pre-site", "site", "post-site", "site-deploy",
];

/// Maven options that consume the FOLLOWING token as their value, so that
/// token must never be treated as a goal (`-pl core`, `-rf :module`).
const VALUE_TAKING_OPTS: &[&str] = &[
    "-pl", "--projects", "-P", "--activate-profiles", "-f", "--file",
    "-T", "--threads", "-rf", "--resume-from", "-s", "--settings",
    "-gs", "--global-settings", "-l", "--log-file", "-b", "--builder",
    "-t", "--toolchains",
];

/// Extract the goal/phase tokens (in order) from a raw mvn arg vector.
/// A token is a goal iff it (1) is not a flag, (2) is not the value of a
/// preceding value-taking option, and (3) is a known lifecycle phase or has
/// the `plugin:goal` form (contains ':').
fn parse_goals(args: &[String]) -> Vec<String> {
    let mut goals = Vec::new();
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg.starts_with('-') {
            // `-pl core` style: the value is a separate token. `-pl=core` and
            // `-Dk=v` keep the value attached, so only bare opts skip-next.
            if VALUE_TAKING_OPTS.contains(&arg.as_str()) {
                skip_next = true;
            }
            continue;
        }
        if MAVEN_PHASES.contains(&arg.as_str()) || arg.contains(':') {
            goals.push(arg.clone());
        }
    }
    goals
}

/// Phases that actually execute surefire/failsafe (everything from `test`
/// onward in the default lifecycle).
const TEST_RUNNING_PHASES: &[&str] = &[
    "test", "prepare-package", "package",
    "pre-integration-test", "integration-test", "post-integration-test",
    "verify", "install", "deploy",
];

/// True if any goal in the chain runs tests — gates XML enrichment in
/// `run_multi_goal`. Avoids a spurious "no XML reports" note when the chain
/// only compiles / runs checkstyle.
fn chain_runs_tests(goals: &[String]) -> bool {
    goals.iter().any(|g| {
        TEST_RUNNING_PHASES.contains(&g.as_str())
            || g.starts_with("surefire:")
            || g.starts_with("failsafe:")
    })
}

lazy_static! {
    /// `[INFO] --- <plugin>:<version>:<goal> (<exec>) @ <module> ---`
    static ref PLUGIN_MARKER_RE: Regex =
        Regex::new(r"^\[INFO\]\s+-{3,}\s+(\S+?):\S+:(\S+)\s+\(").unwrap();
    /// Start of the trailing reactor/build footer.
    static ref BUILD_FOOTER_RE: Regex =
        Regex::new(r"(BUILD SUCCESS|BUILD FAILURE|Reactor Summary)").unwrap();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentKind {
    Preamble,   // before the first plugin marker
    Clean,
    Compile,    // compile / testCompile
    Surefire,   // unit tests
    Failsafe,   // integration tests
    Checkstyle,
    Other,      // jar, resources, install, ...
}

struct Segment {
    kind: SegmentKind,
    body: String,
}

/// Classify a plugin marker into a SegmentKind by its plugin token. Handles
/// both the full artifact id (`maven-surefire-plugin`) and the short
/// goal-prefix form (`surefire`) that Maven prints depending on version/config.
fn classify_marker(plugin: &str) -> SegmentKind {
    if plugin.contains("clean") {
        SegmentKind::Clean
    } else if plugin.contains("compiler") {
        SegmentKind::Compile
    } else if plugin.contains("surefire") {
        SegmentKind::Surefire
    } else if plugin.contains("failsafe") {
        SegmentKind::Failsafe
    } else if plugin.contains("checkstyle") {
        SegmentKind::Checkstyle
    } else {
        SegmentKind::Other
    }
}

/// Split raw mvn output into segments at plugin-execution markers. The
/// trailing BUILD/Reactor footer is NOT a segment — it is handled separately
/// by `extract_build_block` (later task). Everything before the first marker
/// is `Preamble`.
fn split_segments(raw: &str) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut current_kind = SegmentKind::Preamble;
    let mut current_body = String::new();
    let mut in_footer = false;

    for line in raw.lines() {
        let stripped = strip_ansi(line);
        if !in_footer && BUILD_FOOTER_RE.is_match(&stripped) {
            in_footer = true; // stop accumulating into segments
        }
        if let Some(caps) = PLUGIN_MARKER_RE.captures(&stripped) {
            // flush the previous segment
            if !current_body.is_empty() || current_kind != SegmentKind::Preamble {
                segments.push(Segment { kind: current_kind, body: std::mem::take(&mut current_body) });
            } else {
                current_body.clear();
            }
            let plugin = caps.get(1).map_or("", |m| m.as_str());
            current_kind = classify_marker(plugin);
            in_footer = false;
            continue;
        }
        if !in_footer {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }
    if !current_body.is_empty() {
        segments.push(Segment { kind: current_kind, body: current_body });
    }
    segments
}

// ---------------------------------------------------------------------------
// Multi-goal composition layer (Task 4)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MultiParts {
    compile: String,
    tests: String,      // surefire + failsafe combined (enriched later in run_multi_goal)
    checkstyle: String,
    build: String,      // BUILD SUCCESS/FAILURE + Total time (+ Reactor Summary on failure)
    stray_errors: Vec<String>, // [ERROR] lines from dropped/Other segments
}

/// Pull the trailing build footer: always keep BUILD SUCCESS/FAILURE + Total
/// time; on failure also keep the Reactor Summary block (which module failed).
fn extract_build_block(raw: &str) -> String {
    let failed = raw.contains("BUILD FAILURE");
    let mut out: Vec<String> = Vec::new();
    let mut in_reactor = false;
    for line in raw.lines() {
        let s = strip_ansi(line);
        let st = s.trim();
        if failed && st.contains("Reactor Summary") {
            in_reactor = true;
        }
        if in_reactor {
            if st.contains("BUILD FAILURE") || st.contains("BUILD SUCCESS") {
                in_reactor = false;
            } else if !is_maven_boilerplate(st) && !st.is_empty() {
                out.push(strip_maven_prefix(&s).to_string());
            }
        }
        if st.contains("BUILD SUCCESS") || st.contains("BUILD FAILURE") {
            out.push(if failed { "BUILD FAILURE".to_string() } else { "BUILD SUCCESS".to_string() });
        }
        if let Some(t) = parse_total_time(&s) {
            out.push(format!("Total time: {t}"));
        }
    }
    out.join("\n")
}

/// Run each segment group through its existing sub-filter and collect the
/// signal pieces. Pure: no filesystem access (enrichment happens in
/// run_multi_goal, a later task).
fn filter_segments(raw: &str) -> MultiParts {
    let segments = split_segments(raw);
    let mut parts = MultiParts::default();

    let mut compile_buf = String::new();
    let mut test_buf = String::new();
    let mut has_failsafe = false;
    let mut checkstyle_buf = String::new();

    for seg in &segments {
        match seg.kind {
            SegmentKind::Compile => compile_buf.push_str(&seg.body),
            SegmentKind::Surefire => test_buf.push_str(&seg.body),
            SegmentKind::Failsafe => {
                test_buf.push_str(&seg.body);
                has_failsafe = true;
            }
            SegmentKind::Checkstyle => checkstyle_buf.push_str(&seg.body),
            SegmentKind::Clean | SegmentKind::Preamble => {} // dropped as noise
            SegmentKind::Other => {
                for l in seg.body.lines() {
                    if strip_ansi(l).trim_start().starts_with(ERROR_TAG) {
                        parts.stray_errors.push(strip_maven_prefix(&strip_ansi(l)).to_string());
                    }
                }
            }
        }
    }

    if !compile_buf.trim().is_empty() {
        parts.compile = filter_mvn_compile(&compile_buf);
    }
    if !test_buf.trim().is_empty() {
        let goal = if has_failsafe { "verify" } else { "test" };
        parts.tests = filter_mvn_tests_with_goal(&test_buf, goal, &[]);
    }
    if !checkstyle_buf.trim().is_empty() {
        parts.checkstyle = filter_mvn_checkstyle(&checkstyle_buf);
    }
    parts.build = extract_build_block(raw);
    parts
}

/// Assemble the final multi-goal report from already-filtered (and possibly
/// enriched) parts, in canonical order.
fn compose_multi(parts: &MultiParts, goals_header: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "mvn {goals_header} (multi-goal)");
    for piece in [&parts.compile, &parts.tests, &parts.checkstyle] {
        if piece.trim().is_empty() {
            continue;
        }
        let cleaned: String = piece
            .lines()
            .filter(|l| {
                let t = l.trim();
                t != "BUILD SUCCESS" && t != "BUILD FAILURE"
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !cleaned.trim().is_empty() {
            out.push_str(cleaned.trim_end());
            out.push('\n');
        }
    }
    for e in &parts.stray_errors {
        out.push_str(e);
        out.push('\n');
    }
    if !parts.build.trim().is_empty() {
        out.push_str(parts.build.trim_end());
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Pure multi-goal filter (no XML enrichment) — snapshot-tested directly.
/// run_multi_goal (later task) wraps this and adds enrichment on the test portion.
#[allow(dead_code)]
fn filter_mvn_multi(raw: &str, goals_header: &str) -> String {
    // Degraded-input fallback: no markers AND no build footer → never swallow.
    if !PLUGIN_MARKER_RE.is_match(raw) && !BUILD_FOOTER_RE.is_match(raw) {
        return raw.to_string();
    }
    let parts = filter_segments(raw);
    compose_multi(&parts, goals_header)
}

/// Remove `-q` / `--quiet` so RTK receives full output and does the
/// compression itself (multi-goal "smart quiet").
fn strip_quiet_flags(args: &[String]) -> Vec<String> {
    args.iter()
        .filter(|a| a.as_str() != "-q" && a.as_str() != "--quiet")
        .cloned()
        .collect()
}

/// Run a multi-goal invocation: strip -q, run mvn, filter via filter_segments
/// and compose_multi, then enrich the test portion from surefire/failsafe XML
/// when the chain runs tests. Reuses `runner::run_filtered` so exit code and
/// tee behave like every other goal.
fn run_multi_goal(binary: MvnBinary, args: &[String], verbose: u8) -> Result<i32> {
    let goals = parse_goals(args);
    let header = goals.join(" ");
    let run_args = strip_quiet_flags(args);

    let mut cmd = mvn_command(binary);
    for arg in &run_args {
        cmd.arg(arg);
    }
    if verbose > 0 {
        eprintln!("Running: {binary} {} (multi-goal)", run_args.join(" "));
    }

    let started_at = std::time::SystemTime::now();
    let cwd = std::env::current_dir().unwrap_or_else(|e| {
        eprintln!("rtk {binary}: could not determine cwd: {e}");
        std::path::PathBuf::from(".")
    });
    let app_pkgs = crate::cmds::jvm::pom_groupid::detect(&cwd);
    let enrich = chain_runs_tests(&goals);
    let test_goal = if goals.iter().any(|g| g == "verify" || g == "integration-test") {
        "verify"
    } else {
        "test"
    };

    let (tool_name, tee_label) = mvn_labels(binary, "multi", "multi");
    let tee_label_for_filter = tee_label.clone();
    runner::run_filtered(
        cmd,
        &tool_name,
        &run_args.join(" "),
        move |raw: &str| {
            // Degraded-input fallback: never swallow output.
            if !PLUGIN_MARKER_RE.is_match(raw) && !BUILD_FOOTER_RE.is_match(raw) {
                return raw.to_string();
            }
            let mut parts = filter_segments(raw);
            if enrich && !parts.tests.trim().is_empty() {
                let enriched =
                    enrich_with_reports(&parts.tests, &cwd, started_at, &app_pkgs, test_goal);
                parts.tests = finalize_enriched(enriched, &tee_label_for_filter);
            }
            compose_multi(&parts, &header)
        },
        runner::RunOptions::with_tee(&tee_label),
    )
}

fn route_goal(subcommand: &str) -> GoalRouting {
    if COMPILE_LIKE_GOALS.iter().any(|(g, _)| *g == subcommand) {
        return GoalRouting::Compile;
    }
    match subcommand {
        "test" => GoalRouting::TestsLike(TestLikeGoal::Test),
        "verify" => GoalRouting::TestsLike(TestLikeGoal::Verify),
        "integration-test" => GoalRouting::TestsLike(TestLikeGoal::IntegrationTest),
        "package" => GoalRouting::TestsLike(TestLikeGoal::Package),
        "install" => GoalRouting::TestsLike(TestLikeGoal::Install),
        "deploy" => GoalRouting::TestsLike(TestLikeGoal::Deploy),
        "failsafe:integration-test" => {
            GoalRouting::TestsLike(TestLikeGoal::FailsafeIntegrationTest)
        }
        "failsafe:verify" => GoalRouting::TestsLike(TestLikeGoal::FailsafeVerify),
        "surefire:test" => GoalRouting::TestsLike(TestLikeGoal::SurefireTest),
        "clean" => GoalRouting::Clean,
        "checkstyle:check" | "checkstyle" => GoalRouting::Checkstyle,
        "dependency:tree" => GoalRouting::DepTree,
        "dependency:list" => GoalRouting::DepList,
        _ => GoalRouting::Passthrough,
    }
}

/// Stream an unfiltered mvn invocation (long-running/unsupported goals, or
/// goal-less commands like `mvn -version`). Tracked for metrics only.
fn run_passthrough_all(binary: MvnBinary, args: &[OsString], verbose: u8) -> Result<i32> {
    if verbose > 0 {
        eprintln!("Running: {binary} {} (passthrough)", tracking::args_display(args));
    }
    let timer = tracking::TimedExecution::start();
    let mut cmd = mvn_command(binary);
    for arg in args {
        cmd.arg(arg);
    }
    let status = cmd
        .status()
        .with_context(|| format!("Failed to run {binary}"))?;
    let args_str = tracking::args_display(args);
    timer.track_passthrough(
        &format!("{binary} {args_str}"),
        &format!("rtk {binary} {args_str} (passthrough)"),
    );
    Ok(exit_code_from_status(&status, binary.as_str()))
}

/// Top-level mvn/mvnd entry point. Parses goals from the raw arg vector and
/// routes: 0 goals → passthrough; 1 goal → its single-goal filter; ≥2 →
/// multi-goal aggregating filter.
/// Build the args for a filtered single-goal runner: drop the first occurrence
/// of the matched goal token (the run_* helpers prepend their own canonical goal
/// name) and strip `-q`/`--quiet` so RTK receives full output and does the
/// compression itself — the same "smart quiet" applied in multi-goal mode. The
/// unfiltered Passthrough route does NOT use this (it keeps `-q` and streams raw).
fn filtered_goal_args(str_args: &[String], goal: &str) -> Vec<String> {
    let mut removed = false;
    let without_goal: Vec<String> = str_args
        .iter()
        .filter(|a| {
            if !removed && a.as_str() == goal {
                removed = true;
                false
            } else {
                true
            }
        })
        .cloned()
        .collect();
    strip_quiet_flags(&without_goal)
}

pub fn dispatch(binary: MvnBinary, args: &[OsString], verbose: u8) -> Result<i32> {
    let str_args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let goals = parse_goals(&str_args);

    match goals.len() {
        0 => run_passthrough_all(binary, args, verbose),
        1 => {
            let goal = goals[0].clone();
            let rest = filtered_goal_args(&str_args, &goal);
            match route_goal(&goal) {
                GoalRouting::TestsLike(g) => run_tests_like(binary, g, &rest, verbose),
                GoalRouting::Clean => run_clean(binary, &rest, verbose),
                GoalRouting::Compile => run_compile_like(binary, &goal, &rest, verbose),
                GoalRouting::Checkstyle => run_checkstyle(binary, &rest, verbose),
                GoalRouting::DepTree => run_dep_tree(binary, &rest, verbose),
                GoalRouting::DepList => run_dep_list(binary, &rest, verbose),
                GoalRouting::Passthrough => run_passthrough_all(binary, args, verbose),
            }
        }
        _ => run_multi_goal(binary, &str_args, verbose),
    }
}

// ---------------------------------------------------------------------------
// State machine parser for mvn test output
// ---------------------------------------------------------------------------

const MAX_DETAIL_LINES: usize = 3;
/// `Caused by:` headers kept per failure, on top of MAX_DETAIL_LINES. The
/// root cause is what agents dig for after a failure — usage analysis showed
/// repeated `grep 'Caused by'` follow-ups on tee logs when it was cut off.
const MAX_CAUSE_LINES: usize = 4;
const MAX_FAILURES_SHOWN: usize = 10;
const MAX_LINE_LENGTH: usize = 200;

#[derive(Debug, PartialEq)]
enum TestParseState {
    Preamble,
    Testing,
    Summary,
    Done,
}


struct FailureEntry {
    name: String,
    details: Vec<String>,
    /// How many of `details` are `Caused by:` headers — they are capped
    /// separately from regular detail lines (see `MAX_CAUSE_LINES`).
    cause_lines: usize,
}

/// Parse the four count fields from a `TESTS_RUN_RE` captures. The regex
/// guarantees four numeric groups so defaulting to 0 is only a safety net.
fn parse_counts(caps: &regex::Captures) -> TestSummary {
    TestSummary {
        run: caps.get(1).map_or(0, |m| m.as_str().parse().unwrap_or(0)),
        failures: caps.get(2).map_or(0, |m| m.as_str().parse().unwrap_or(0)),
        errors: caps.get(3).map_or(0, |m| m.as_str().parse().unwrap_or(0)),
        skipped: caps.get(4).map_or(0, |m| m.as_str().parse().unwrap_or(0)),
    }
}

/// Discover surefire/failsafe report directories under `cwd`.
///
/// Maven multi-module reactors write reports under each module's own
/// `target/` directory (`<cwd>/<module>/target/surefire-reports/`).
/// Single-module builds use `<cwd>/target/surefire-reports/`. We probe
/// both and return every directory that exists, so reactor and `-pl`
/// runs from the repo root surface failure details.
///
/// Walk depth is limited to 1 (direct child modules). Nested submodules
/// are not discovered — run `rtk mvn` from the parent module dir for
/// deeper structures.
fn discover_report_dirs(cwd: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut surefire: Vec<PathBuf> = Vec::new();
    let mut failsafe: Vec<PathBuf> = Vec::new();

    let mut probe_target = |target: PathBuf| {
        let sf = target.join("surefire-reports");
        if sf.is_dir() {
            surefire.push(sf);
        }
        let fs = target.join("failsafe-reports");
        if fs.is_dir() {
            failsafe.push(fs);
        }
    };

    // Single-module / direct: cwd/target/...
    probe_target(cwd.join("target"));

    // Reactor modules: cwd/<module>/target/...
    let Ok(entries) = std::fs::read_dir(cwd) else {
        return (surefire, failsafe);
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Skip dot-dirs, the cwd-level target itself (already probed),
        // and well-known non-module dirs to keep the walk cheap.
        if name.starts_with('.')
            || name == "target"
            || name == "src"
            || name == "node_modules"
            || name == "build"
            || name == "out"
        {
            continue;
        }
        probe_target(path.join("target"));
    }

    (surefire, failsafe)
}

/// Module name for a report dir: first path component of `dir` relative to
/// `cwd` ("services/target/surefire-reports" -> "services"); `None` for the
/// root-level `target/` or when the dir is outside `cwd`.
fn module_for_dir(dir: &std::path::Path, cwd: &std::path::Path) -> Option<String> {
    let rel = dir.strip_prefix(cwd).ok()?;
    let first = rel.components().next()?;
    let name = first.as_os_str().to_str()?;
    if name == "target" {
        None
    } else {
        Some(name.to_string())
    }
}

/// Parse every report dir and merge results into one `SurefireResult`.
/// Returns `None` only when no dir produced any output.
fn collect_reports(
    dirs: &[PathBuf],
    since: std::time::SystemTime,
    app_packages: &[String],
    cwd: &std::path::Path,
) -> Option<SurefireResult> {
    let mut merged: Option<SurefireResult> = None;
    for dir in dirs {
        let Some(r) = surefire_reports::parse_dir(dir, Some(since), app_packages) else {
            continue;
        };
        let module = module_for_dir(dir, cwd);
        let mut r = r;
        for s in r.suites.iter_mut() {
            s.module = module.clone();
        }
        match &mut merged {
            Some(acc) => {
                acc.summary.add(&r.summary);
                acc.failures.extend(r.failures);
                acc.suites.extend(r.suites);
                acc.skipped_tests.extend(r.skipped_tests);
                acc.files_read += r.files_read;
                acc.files_skipped_stale += r.files_skipped_stale;
                acc.files_malformed += r.files_malformed;
            }
            None => merged = Some(r),
        }
    }
    merged
}

/// Result of enrichment: the (possibly extended) text summary plus optional
/// digest content to write next to the tee log.
pub(crate) struct Enriched {
    pub(crate) text: String,
    /// Digest file content; `None` -> nothing to write.
    pub(crate) digest: Option<String>,
    /// Append a "classes: <path>" line after writing the digest.
    pub(crate) reference: bool,
}

/// Wrap the text-filter summary with structured failure details sourced from
/// `target/surefire-reports/` and `target/failsafe-reports/` XML files.
/// Discovers per-module report dirs in reactor builds (depth-1 walk).
///
/// Passing runs are also enriched: a per-class breakdown is inlined when it
/// fits (see `MAX_INLINE_CLASSES`/`MAX_INLINE_SKIPPED`), otherwise the
/// summary is left unchanged and the full breakdown goes only into the
/// returned digest, with `reference` signaling a "classes: <path>" pointer
/// line is needed.
pub(crate) fn enrich_with_reports(
    text_summary: &str,
    cwd: &std::path::Path,
    since: std::time::SystemTime,
    app_packages: &[String],
    goal: &str,
) -> Enriched {
    let passthrough = |text: String| Enriched {
        text,
        digest: None,
        reference: false,
    };
    if !text_summary.starts_with("[INFO]")
        && !text_summary.starts_with("[ERROR]")
        && !text_summary.starts_with("[WARNING]")
    {
        return passthrough(text_summary.to_string());
    }

    let zero_tests = text_summary == "[WARNING] No tests were executed!";
    let has_failures = text_summary.contains("BUILD FAILURE");
    // NOTE: replaces the old `looks_clean` substring check ("passed (" missed
    // "N passed, K skipped (t)" summaries) — a run is passing iff it neither
    // failed nor ran zero tests.
    let passing = !zero_tests && !has_failures;

    let (sf_dirs, fs_dirs) = discover_report_dirs(cwd);
    let sf = collect_reports(&sf_dirs, since, app_packages, cwd);
    let fs = collect_reports(&fs_dirs, since, app_packages, cwd);
    let digest = render_classes_digest(goal, sf.as_ref(), fs.as_ref());

    if passing {
        // Fallback invariant: no parsed reports -> summary unchanged, silently.
        if digest.is_none() {
            return passthrough(text_summary.to_string());
        }
        let (text, reference) = render_pass_inline(text_summary, sf.as_ref(), fs.as_ref());
        return Enriched {
            text,
            digest,
            reference,
        };
    }

    match (zero_tests, &sf, &fs) {
        (true, None, None) => passthrough(format!(
            "mvn {goal}: No tests run (0 tests executed — surefire detected \
             no tests). Check pom.xml (surefire plugin configuration)."
        )),
        (false, None, None) => passthrough(format!(
            "{text_summary}\n(no XML reports found — check target/surefire-reports/)"
        )),
        _ => Enriched {
            text: render_enriched(text_summary, sf.as_ref(), fs.as_ref()),
            reference: digest.is_some(),
            digest,
        },
    }
}

/// Write the class digest (if any) through the tee infrastructure and append
/// the `classes: <path>` reference line when the inline output doesn't carry
/// the full breakdown. Falls back to the enriched text unchanged when tee is
/// disabled or the write fails — the summary must never degrade.
fn finalize_enriched(enriched: Enriched, tee_label: &str) -> String {
    let Some(digest) = enriched.digest else {
        return enriched.text;
    };
    let slug = format!("{tee_label}_classes");
    match crate::core::tee::force_tee_display(&digest, &slug) {
        Some(path) if enriched.reference => format!("{}\nclasses: {}", enriched.text, path),
        _ => enriched.text,
    }
}

// ---------------------------------------------------------------------------
// Pure renderers for pass-run enrichment (Task 4)
// ---------------------------------------------------------------------------

const MAX_INLINE_CLASSES: usize = 5;
const MAX_INLINE_SKIPPED: usize = 3;

fn short_class(fqcn: &str) -> &str {
    fqcn.rsplit('.').next().unwrap_or(fqcn)
}

fn all_suites<'a>(
    surefire: Option<&'a SurefireResult>,
    failsafe: Option<&'a SurefireResult>,
) -> Vec<&'a surefire_reports::SuiteStat> {
    surefire
        .into_iter()
        .chain(failsafe)
        .flat_map(|r| r.suites.iter())
        .collect()
}

fn all_skipped<'a>(
    surefire: Option<&'a SurefireResult>,
    failsafe: Option<&'a SurefireResult>,
) -> Vec<&'a surefire_reports::SkippedTest> {
    surefire
        .into_iter()
        .chain(failsafe)
        .flat_map(|r| r.skipped_tests.iter())
        .collect()
}

/// Condensed per-class report written next to the tee log. `None` when no
/// suites were parsed (nothing worth writing).
fn render_classes_digest(
    goal: &str,
    surefire: Option<&SurefireResult>,
    failsafe: Option<&SurefireResult>,
) -> Option<String> {
    let suites = all_suites(surefire, failsafe);
    if suites.is_empty() {
        return None;
    }
    let skipped = all_skipped(surefire, failsafe);
    // Maven-native aggregate line: agents grep tee logs with Maven's own
    // summary pattern, so the header must match it verbatim.
    let mut summary = surefire_reports::TestSummary::default();
    if let Some(sf) = surefire {
        summary.add(&sf.summary);
    }
    if let Some(fs) = failsafe {
        summary.add(&fs.summary);
    }
    let mut out = format!(
        "# mvn {goal} (from XML reports) — Tests run: {}, Failures: {}, Errors: {}, Skipped: {}",
        summary.run, summary.failures, summary.errors, summary.skipped
    );
    out.push('\n');

    // Group by module; BTreeMap for deterministic order, root module last-free
    // "." key sorts first which is fine.
    let mut by_module: std::collections::BTreeMap<&str, Vec<String>> =
        std::collections::BTreeMap::new();
    for s in &suites {
        by_module
            .entry(s.module.as_deref().unwrap_or("."))
            .or_default()
            .push(format!(
                "{} {} ({:.1}s)",
                short_class(&s.class_name),
                s.tests,
                s.time_secs
            ));
    }
    for (module, classes) in &by_module {
        writeln!(out, "{module}: {}", classes.join(", ")).ok();
    }

    if !skipped.is_empty() {
        out.push_str("skipped:\n");
        for st in &skipped {
            match &st.reason {
                Some(reason) => {
                    writeln!(out, "  {}.{} — {}", short_class(&st.class), st.method, reason)
                        .ok();
                }
                None => {
                    writeln!(out, "  {}.{}", short_class(&st.class), st.method).ok();
                }
            }
        }
    }
    Some(out)
}

/// Hybrid inline rendering for passing runs. Returns the (possibly extended)
/// summary and whether the digest reference line is required — true when the
/// class list or skipped names exceed the inline caps.
fn render_pass_inline(
    text_summary: &str,
    surefire: Option<&SurefireResult>,
    failsafe: Option<&SurefireResult>,
) -> (String, bool) {
    let suites = all_suites(surefire, failsafe);
    let skipped = all_skipped(surefire, failsafe);
    let needs_reference =
        suites.len() > MAX_INLINE_CLASSES || skipped.len() > MAX_INLINE_SKIPPED;

    // The pass summary ends with a maven-native "BUILD SUCCESS" footer; the
    // inline breakdown slots in before it so the footer stays the last line.
    let (mut out, build_footer) = match text_summary.strip_suffix("\n[INFO] BUILD SUCCESS") {
        Some(head) => (head.to_string(), true),
        None => (text_summary.to_string(), false),
    };
    if !suites.is_empty() && suites.len() <= MAX_INLINE_CLASSES {
        for s in &suites {
            write!(
                out,
                "\n{}: {} ({:.1}s)",
                short_class(&s.class_name),
                s.tests,
                s.time_secs
            )
            .ok();
        }
    }
    if !skipped.is_empty() && skipped.len() <= MAX_INLINE_SKIPPED {
        for st in &skipped {
            write!(out, "\nskipped: {}.{}", short_class(&st.class), st.method).ok();
            if let Some(reason) = &st.reason {
                write!(out, " — {reason}").ok();
            }
        }
    }
    if build_footer {
        out.push_str("\n[INFO] BUILD SUCCESS");
    }
    (out, needs_reference)
}

fn render_enriched(
    text_summary: &str,
    surefire: Option<&SurefireResult>,
    failsafe: Option<&SurefireResult>,
) -> String {
    let sf_has_failures = surefire.is_some_and(|sf| !sf.failures.is_empty());
    let fs_has_failures = failsafe.is_some_and(|fs| !fs.failures.is_empty());

    // XML enrichment is authoritative (stack trace, captured output) — drop the
    // text filter's `Failures:` block to avoid duplicating the same failures.
    let mut out = if sf_has_failures || fs_has_failures {
        strip_text_failures_block(text_summary)
    } else {
        text_summary.to_string()
    };

    if let Some(sf) = surefire {
        if !sf.failures.is_empty() {
            out.push_str("\n\nFailures (from surefire-reports/):\n");
            render_failure_block(&mut out, &sf.failures);
        }
    }

    if let Some(fs) = failsafe {
        if !fs.failures.is_empty() {
            out.push_str("\n\nIntegration failures (from failsafe-reports/):\n");
            render_failure_block(&mut out, &fs.failures);
        }
    }

    let footer = render_footer(surefire, failsafe);
    if !footer.is_empty() {
        out.push_str("\n\n");
        out.push_str(&footer);
    }

    out
}

/// Truncate the text-filter's `\n[ERROR] Failures:\n...` block so XML
/// enrichment can replace it without duplicating the same test names.
fn strip_text_failures_block(text_summary: &str) -> String {
    match text_summary.find("\n[ERROR] Failures:\n") {
        Some(idx) => text_summary[..idx].trim_end().to_string(),
        None => text_summary.to_string(),
    }
}

fn render_failure_block(out: &mut String, failures: &[TestFailure]) {
    let shown = failures.iter().take(MAX_FAILURES_PER_SOURCE);
    for (i, f) in shown.enumerate() {
        // Maven's per-test marker — agents grep for `<<< FAILURE` / `FAILURE!`
        // to list failing test names.
        writeln!(out, "{}. {}.{} <<< FAILURE!", i + 1, f.test_class, f.test_method).ok();
        if let Some(kind_label) = failure_kind_label(f) {
            writeln!(out, "   {kind_label}").ok();
        }
        if let Some(trace) = &f.stack_trace {
            for line in trace.lines() {
                writeln!(out, "     {line}").ok();
            }
        }
        if let Some(output) = f.test_output.as_deref().filter(|s| !s.is_empty()) {
            writeln!(out, "  captured output:").ok();
            for line in output.lines() {
                writeln!(out, "    {line}").ok();
            }
        }
        out.push('\n');
    }
    if failures.len() > MAX_FAILURES_PER_SOURCE {
        writeln!(
            out,
            "... +{} more failures",
            failures.len() - MAX_FAILURES_PER_SOURCE
        )
        .ok();
    }
}

fn failure_kind_label(f: &TestFailure) -> Option<String> {
    let msg = f.message.as_deref().unwrap_or("").trim();
    let ty = f
        .failure_type
        .as_deref()
        .and_then(|t| t.rsplit('.').next())
        .unwrap_or("");
    match (ty.is_empty(), msg.is_empty()) {
        (true, true) => None,
        (true, false) => Some(msg.to_string()),
        (false, true) => Some(ty.to_string()),
        (false, false) => Some(format!("{ty}: {msg}")),
    }
    .map(|s| match f.kind {
        FailureKind::Error => format!("[error] {s}"),
        FailureKind::Failure => s,
    })
}

fn render_footer(
    surefire: Option<&SurefireResult>,
    failsafe: Option<&SurefireResult>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    let (sf_read, sf_stale, sf_bad) = counts(surefire);
    let (fs_read, fs_stale, fs_bad) = counts(failsafe);

    if sf_read > 0 {
        parts.push(format!("{sf_read} surefire"));
    }
    if fs_read > 0 {
        parts.push(format!("{fs_read} failsafe"));
    }
    let stale = sf_stale + fs_stale;
    if stale > 0 {
        parts.push(format!("{stale} stale files skipped"));
    }
    let malformed = sf_bad + fs_bad;
    if malformed > 0 {
        parts.push(format!("{malformed} malformed"));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("(reports: {})", parts.join(", "))
}

fn counts(r: Option<&SurefireResult>) -> (usize, usize, usize) {
    r.map(|x| (x.files_read, x.files_skipped_stale, x.files_malformed))
        .unwrap_or((0, 0, 0))
}

/// Filter `mvn test` output using a state machine parser.
#[cfg(test)]
pub(crate) fn filter_mvn_test(output: &str) -> String {
    filter_mvn_tests_with_goal(output, "test", &[])
}

#[cfg(test)]
pub(crate) fn filter_mvn_verify(output: &str) -> String {
    filter_mvn_tests_with_goal(output, "verify", &[])
}

/// Shared state machine parser for test-producing goals (`test`, `verify`).
///
/// States: Preamble -> Testing -> Summary -> Done
/// - Preamble: skip everything before "T E S T S" marker
/// - Testing: collect failure details from [ERROR] headers and assertion lines
/// - Summary: parse final "Tests run:" line, BUILD SUCCESS/FAILURE, Total time
/// - Done: stop at Help boilerplate
fn filter_mvn_tests_with_goal(output: &str, goal: &str, app_packages: &[String]) -> String {
    let clean = strip_ansi(output);
    let mut state = TestParseState::Preamble;

    let mut failures: Vec<FailureEntry> = Vec::with_capacity(MAX_FAILURES_SHOWN);
    let mut current_failure: Option<FailureEntry> = None;

    let mut cumulative = TestSummary::default();
    let mut section: Option<TestSummary> = None;
    let mut total_failures_seen: usize = 0;

    for line in clean.lines() {
        let trimmed = line.trim();
        let stripped = strip_maven_prefix(trimmed);

        // Global transition: T E S T S marker resets to Testing from any state
        // (multi-module builds emit this marker per module)
        if stripped.contains("T E S T S") {
            if let Some(s) = section.take() {
                cumulative.add(&s);
            }
            state = TestParseState::Testing;
            continue;
        }

        match state {
            TestParseState::Preamble => {}
            TestParseState::Testing => {
                if stripped == "Results:" {
                    if let Some(f) = current_failure.take() {
                        total_failures_seen += 1;
                        if failures.len() < MAX_FAILURES_SHOWN {
                            failures.push(f);
                        }
                    }
                    state = TestParseState::Summary;
                    continue;
                }

                if let Some(caps) = FAILURE_HEADER_RE.captures(trimmed) {
                    if let Some(f) = current_failure.take() {
                        total_failures_seen += 1;
                        if failures.len() < MAX_FAILURES_SHOWN {
                            failures.push(f);
                        }
                    }
                    let test_name = caps.get(1).map_or("", |m| m.as_str()).to_string();
                    current_failure = Some(FailureEntry {
                        name: test_name,
                        details: Vec::new(),
                        cause_lines: 0,
                    });
                    continue;
                }

                // Per-plugin summary line inside the Testing block:
                // "Tests run: N, Failures: N, Errors: N, Skipped: N" with no
                // "-- in <class>" suffix. Priority over any later Summary-state
                // match so that the reactor aggregate (which appears after the
                // LAST module's Summary block in multi-module builds) does not
                // overwrite the real per-module total.
                if !trimmed.contains("-- in") {
                    if let Some(caps) = TESTS_RUN_RE.captures(stripped) {
                        section = Some(parse_counts(&caps));
                        continue;
                    }
                }

                // The next test class starts — close the current failure so
                // its "Running <class>" marker does not bleed into the stack
                // block.
                if stripped.starts_with("Running ") {
                    if let Some(f) = current_failure.take() {
                        total_failures_seen += 1;
                        if failures.len() < MAX_FAILURES_SHOWN {
                            failures.push(f);
                        }
                    }
                    continue;
                }

                if let Some(ref mut f) = current_failure {
                    // `Caused by:` headers get their own budget on top of the
                    // detail cap — the root cause must never be cut off.
                    let is_cause = stripped.starts_with("Caused by:");
                    if is_cause {
                        if f.cause_lines >= MAX_CAUSE_LINES {
                            continue;
                        }
                    } else if f.details.len().saturating_sub(f.cause_lines)
                        >= MAX_DETAIL_LINES
                    {
                        continue;
                    }
                    if is_framework_frame_ext(stripped, app_packages)
                        || is_maven_boilerplate(trimmed)
                        || stripped.is_empty()
                        || (trimmed.starts_with(ERROR_TAG) && stripped.contains("<<<"))
                    {
                        continue;
                    }
                    f.details.push(stripped.to_string());
                    if is_cause {
                        f.cause_lines += 1;
                    }
                }
            }
            TestParseState::Summary => {
                if is_maven_boilerplate(trimmed) || stripped.starts_with("Failures:") {
                    continue;
                }

                if section.is_none() {
                    if let Some(caps) = TESTS_RUN_RE.captures(stripped) {
                        section = Some(parse_counts(&caps));
                    }
                }

                if parse_total_time(stripped).is_some() {
                    // Total time is no longer surfaced on the test path (the
                    // prefix-preserving frame drops it) — only the state
                    // transition is needed here.
                    state = TestParseState::Done;
                }
            }
            TestParseState::Done => break,
        }
    }

    if let Some(s) = section.take() {
        cumulative.add(&s);
    }

    if state == TestParseState::Preamble {
        // The build never reached the `T E S T S` marker. That means either:
        //   (a) the goal ran something that produced no tests (validate,
        //       a plugin-only phase) — "no tests run" is correct; or
        //   (b) the build failed earlier (typically at the compile phase).
        //       In that case, returning a cheerful "no tests run" line
        //       would hide the actual errors from the user. Fall back to
        //       the compile filter so the error block reaches them.
        if clean.contains("BUILD FAILURE") {
            return filter_mvn_compile(output);
        }
        // Surefire's own native line — no synthetic `mvn <goal>:` prose.
        let _ = goal; // goal no longer interpolated into the no-tests line
        return "[WARNING] No tests were executed!".to_string();
    }

    let counts = cumulative;
    let has_failures = counts.failures > 0 || counts.errors > 0;

    // Guard: BUILD FAILURE while still in `Testing` (no `Results:` block,
    // or one that arrived after the build aborted) means a forked-VM
    // crash, surefire timeout, or plugin abort — the parser has no count
    // of failures, but the build is hard-failed. Without this guard the
    // success branch below would emit "0 passed", silently hiding the
    // failure. Fall back to the compile filter so the actual error block
    // (which the raw output still contains) reaches the user.
    if !has_failures && clean.contains("BUILD FAILURE") {
        return filter_mvn_compile(output);
    }

    // Maven's own summary line, prefixed exactly as surefire prints it:
    // `[INFO]` on a clean pass, `[ERROR]` when there are failures/errors.
    let agg_prefix = if has_failures { "[ERROR]" } else { "[INFO]" };
    let aggregate = format!(
        "{agg_prefix} Tests run: {}, Failures: {}, Errors: {}, Skipped: {}",
        counts.run, counts.failures, counts.errors, counts.skipped
    );

    if !has_failures {
        // Frame only — enrichment (per-class breakdown) is slotted in by
        // render_pass_inline, before the BUILD footer.
        return format!("{aggregate}\n[INFO] BUILD SUCCESS");
    }

    let mut result = format!("{aggregate}\n[INFO] BUILD FAILURE\n");
    if !failures.is_empty() {
        result.push_str("\n[ERROR] Failures:\n");
    }
    for failure in failures.iter() {
        // Maven's per-test coord form: `Class.method <<< FAILURE!`, [ERROR]-prefixed.
        writeln!(result, "[ERROR]   {} <<< FAILURE!", failure.name).ok();
        for (di, detail) in failure.details.iter().enumerate() {
            let rendered = if di == 0 {
                shorten_exception_header(detail)
            } else {
                detail.clone()
            };
            writeln!(result, "[ERROR]     {}", truncate(&rendered, MAX_LINE_LENGTH)).ok();
        }
    }
    if total_failures_seen > MAX_FAILURES_SHOWN {
        writeln!(result, "\n... +{} more failures", total_failures_seen - MAX_FAILURES_SHOWN).ok();
    }
    result.trim().to_string()
}

/// Strip [INFO], [ERROR], [WARNING] prefixes from Maven output lines.
/// Expects pre-trimmed input from callers.
fn strip_maven_prefix(line: &str) -> &str {
    for tag in [INFO_TAG, ERROR_TAG, WARNING_TAG] {
        if let Some(rest) = line.strip_prefix(tag) {
            return rest.trim_start();
        }
    }
    line
}

/// Returns true for Java framework stack frames that should be stripped.
/// Expects pre-trimmed input (callers pass `stripped` or `trimmed`).
fn is_framework_frame(line: &str) -> bool {
    let check = line.strip_prefix("at ").unwrap_or(line);

    const FRAMEWORK_PREFIXES: &[&str] = &[
        "org.apache.maven.",
        "org.junit.platform.",
        "org.junit.jupiter.",
        "org.codehaus.plexus.",
        "java.base/",
        "sun.reflect.",
        "jdk.internal.",
    ];

    for prefix in FRAMEWORK_PREFIXES {
        if check.starts_with(prefix) {
            return true;
        }
    }

    // "... N more" truncation markers
    line.starts_with("...") && line.contains("more")
}

/// Extended framework-frame check driven by the legacy whitelist plus
/// `app_packages` when known. Any `at <pkg>...` frame whose package is not
/// in `app_packages` is treated as framework noise, matching the XML
/// enrichment path's filtering behavior.
///
/// When `app_packages` is empty, falls back to the whitelist only — this
/// preserves the pre-app_packages behavior for fixtures and callers that
/// do not supply a pom groupId.
fn is_framework_frame_ext(line: &str, app_packages: &[String]) -> bool {
    if is_framework_frame(line) {
        return true;
    }
    if app_packages.is_empty() {
        return false;
    }
    // Only `at ...` frames are framework-gated. Exception headers and
    // assertion messages (e.g. "expected:<X> but was:<Y>") never start with
    // `at ` after `strip_maven_prefix`, so they pass through unchanged.
    let Some(after_at) = line.strip_prefix("at ") else {
        return false;
    };
    !app_packages
        .iter()
        .any(|pkg| after_at.starts_with(pkg.as_str()))
}

/// Strip the package prefix from an exception header, mirroring the XML
/// path's `failure_kind_label` (`rsplit('.').next()`). Turns
/// `"org.junit.ComparisonFailure: expected:<X> but was:<Y>"` into
/// `"ComparisonFailure: expected:<X> but was:<Y>"`. Passes non-exception
/// lines through unchanged.
fn shorten_exception_header(line: &str) -> String {
    let Some((fqn, rest)) = line.split_once(':') else {
        return line.to_string();
    };
    let fqn_trimmed = fqn.trim();
    // Must look like a Java FQN: non-empty, no spaces, contains a dot.
    if fqn_trimmed.is_empty()
        || fqn_trimmed.contains(char::is_whitespace)
        || !fqn_trimmed.contains('.')
    {
        return line.to_string();
    }
    // Every segment must be a valid Java identifier-ish token (letters,
    // digits, `_`, `$`). This rejects message bodies like "expected:<X>".
    let valid = fqn_trimmed.split('.').all(|seg| {
        !seg.is_empty()
            && seg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
    });
    if !valid {
        return line.to_string();
    }
    let short = fqn_trimmed.rsplit('.').next().unwrap_or(fqn_trimmed);
    format!("{short}:{rest}")
}

/// Returns true for Maven boilerplate lines that should be stripped.
/// Expects pre-trimmed input from callers.
fn is_maven_boilerplate(line: &str) -> bool {
    // Empty [ERROR] or [INFO] lines
    if line == ERROR_TAG || line == INFO_TAG || line == WARNING_TAG {
        return true;
    }

    let stripped = strip_maven_prefix(line);

    // Separator lines (dashes)
    if stripped.starts_with("---") && stripped.chars().all(|c| c == '-' || c.is_whitespace()) {
        return true;
    }

    const BOILERPLATE_PATTERNS: &[&str] = &[
        "-> [Help",
        "http://cwiki.apache.org",
        "https://cwiki.apache.org",
        "surefire-reports",
        "Re-run Maven",
        "re-run Maven",
        "full stack trace",
        "enable verbose output",
        "See dump files",
        "Failed to execute goal",
        "There are test failures",
        "For more information about the errors",
    ];

    for pattern in BOILERPLATE_PATTERNS {
        if stripped.contains(pattern) {
            return true;
        }
    }

    false
}

// ---------------------------------------------------------------------------
// Line filter for mvn compile output
// ---------------------------------------------------------------------------

/// Filter `mvn compile` (and compile-like goals such as `process-classes`,
/// `test-compile`) output — strip [INFO] noise, keep errors and summary.
///
/// Multi-module reactors emit a `Reactor Build Order:` block and a `Reactor
/// Summary for …` block with per-module status lines. Both are collapsed:
/// build-order lines are skipped outright (redundant with per-module Building
/// headers), and the summary is replaced by a one-liner
/// `N modules: M SUCCESS, K FAILURE (first-failed-name)` that only surfaces if
/// something failed (keeps BUILD SUCCESS clean for green builds).
///
/// javac `[ERROR] <path>:[<line>,<col>]` lines are deduped by (path, line, col)
/// because Maven prints them twice on failure — inline during compilation and
/// again in the trailing `[ERROR]` help block.
fn filter_mvn_compile(output: &str) -> String {
    let clean = strip_ansi(output);
    let mut in_build_order = false;
    // (status, name) per module while the Reactor Summary block is open.
    // Both slices borrow from `clean`, which outlives this vec.
    let mut reactor_modules: Option<Vec<(&str, &str)>> = None;
    // Dedup key is the matched `[ERROR] path:[L,C]` prefix — a slice of `clean`.
    let mut seen_errors: HashSet<&str> = HashSet::new();
    // When the current `[ERROR] path:[L,C]` was a duplicate, swallow the
    // javac context lines (`[ERROR] symbol: …`) that would mirror an earlier
    // occurrence emitted without the `[ERROR]` prefix.
    let mut swallow_error_context = false;
    // Inside a noisy codegen/exec plugin segment (npm builds, liquibase+jooq
    // codegen): suppress everything except error-ish lines. These plugins
    // stream arbitrary tool output — often on bare stderr with no [INFO]
    // prefix — that no line-level pattern list can keep up with.
    let mut in_noisy_segment = false;
    let mut result = String::with_capacity(clean.len() / 4);

    let push = |dst: &mut String, line: &str| {
        if !dst.is_empty() {
            dst.push('\n');
        }
        dst.push_str(line);
    };

    for raw in clean.lines() {
        let line = raw.trim();
        let stripped = strip_maven_prefix(line);

        if let Some(caps) = PLUGIN_MARKER_RE.captures(line) {
            let plugin = caps.get(1).map_or("", |m| m.as_str());
            in_noisy_segment = is_noisy_codegen_plugin(plugin);
        } else if in_noisy_segment {
            // Reset on the build footer or the next reactor module header
            // (`----< group:artifact >----`). Bare `-----` separators inside
            // tool output (liquibase UPDATE SUMMARY) must NOT reset.
            if BUILD_FOOTER_RE.is_match(stripped)
                || (stripped.starts_with("---") && stripped.contains("< "))
            {
                in_noisy_segment = false;
                // fall through — footer lines get their normal handling
            } else if !is_errorish_segment_line(line, stripped) {
                continue;
            }
        }

        if in_build_order {
            if REACTOR_BUILD_ORDER_RE.is_match(stripped)
                || stripped.is_empty()
                || line == INFO_TAG
            {
                continue;
            }
            in_build_order = false;
            // fall through — current line may be keep-worthy
        }

        if stripped == "Reactor Build Order:" {
            in_build_order = true;
            continue;
        }

        if let Some(modules) = reactor_modules.as_mut() {
            if let Some(caps) = REACTOR_SUMMARY_LINE_RE.captures(stripped) {
                let name = caps.get(1).map_or("", |m| m.as_str()).trim();
                let status = caps.get(2).map_or("", |m| m.as_str());
                modules.push((status, name));
                continue;
            }
            if stripped.is_empty() || line == INFO_TAG || stripped.starts_with("---") {
                continue;
            }
            if let Some(compact) = format_reactor_summary(modules) {
                push(&mut result, &compact);
            }
            reactor_modules = None;
            // fall through
        }

        if stripped.starts_with("Reactor Summary for ") {
            reactor_modules = Some(Vec::new());
            continue;
        }

        if let Some(short) = shorten_unknown_phase_error(stripped) {
            push(&mut result, &short);
            continue;
        }

        if !should_keep_compile_line(line) {
            swallow_error_context = false;
            continue;
        }

        if line.starts_with(ERROR_TAG) {
            if let Some(m) = COMPILE_ERROR_LOCATION_RE.find(line) {
                if !seen_errors.insert(m.as_str()) {
                    swallow_error_context = true;
                    continue;
                }
                swallow_error_context = false;
            } else if swallow_error_context && COMPILE_ERROR_CONTEXT_RE.is_match(line) {
                continue;
            } else {
                swallow_error_context = false;
            }
        } else {
            swallow_error_context = false;
        }

        push(&mut result, line);
    }

    if let Some(modules) = reactor_modules.as_ref() {
        if let Some(compact) = format_reactor_summary(modules) {
            push(&mut result, &compact);
        }
    }

    if result.is_empty() {
        return "mvn: ok".to_string();
    }

    result
}

/// Render a one-line reactor summary naming failed modules. Returns `None`
/// when every module succeeded — the trailing `BUILD SUCCESS` line is enough.
fn format_reactor_summary(modules: &[(&str, &str)]) -> Option<String> {
    if modules.is_empty() {
        return None;
    }
    let failed: Vec<&str> = modules
        .iter()
        .filter(|(status, _)| *status == "FAILURE")
        .map(|(_, name)| *name)
        .collect();
    if failed.is_empty() {
        return None;
    }
    let skipped = modules.iter().filter(|(s, _)| *s == "SKIPPED").count();
    let succeeded = modules.len() - failed.len() - skipped;
    let mut out = format!(
        "Reactor: {} modules — {} SUCCESS, {} FAILURE",
        modules.len(),
        succeeded,
        failed.len()
    );
    if skipped > 0 {
        write!(&mut out, ", {skipped} SKIPPED").ok();
    }
    write!(&mut out, " ({})", failed.join(", ")).ok();
    Some(out)
}

const INFO_NOISE_PATTERNS: &[&str] = &[
    "---",
    "===",
    "Building ",
    "Downloading ",
    "Downloaded ",
    "Scanning ",
    "Compiling ",
    "Recompiling ",
    "Nothing to compile",
    "Using auto detected",
    "Loaded ",
    "Finished at:",
    "/pom.xml",
    "from pom.xml",
    "Copying ",
    "argLine set to",
    "Migration completed",
    "Inferring ",
    "No <input",
    // githook-maven-plugin install chatter
    "Installing commit-msg hook",
    // maven-compiler-plugin trivia that precedes the actual compile step
    "Changes detected - recompiling",
    // artifactregistry-maven-wagon chatter — can be dozens of ~300-char
    // lines per build about cached artifacts not matching the current
    // remote-repo set. Non-actionable; the build still proceeds.
    "is present in the local repository, but cached",
    // GCP auth lifecycle chatter from artifactregistry-maven-wagon
    "Initializing Credentials",
    "Application Default Credentials",
    // os-maven-plugin extension banner
    "Detecting the operating system",
    "Refreshing Credentials",
    // pgpverify-maven-plugin chatter (per-artifact verify + summary)
    "Verifying ",
    "Key server(s)",
    "Create cache directory",
    "Artifacts were already validated",
    " artifact(s) in repository",
    // maven-resources-plugin non-actionable copy chatter. `copy filtered`
    // catches both variants:
    //   "Using 'UTF-8' encoding to copy filtered resources."
    //   "The encoding used to copy filtered properties files have not been set…"
    "copy filtered",
    "skip non existing resourceDirectory",
    // maven-checkstyle-plugin clean-audit output
    "Starting audit",
    "Audit done",
    "Checkstyle violations",
    // Code generators (jOOQ, protobuf, openapi-generator, etc.)
    "Generat",
    "Missing name",
    " fetched",
    " generated",
    "Affected files",
    "No schema version",
    "Removing excess",
    "Source directory",
    "Modified files",
    "License parameters",
    "Database parameters",
    "JavaGenerator",
    "Target parameters",
    "Thank you for using",
    "global references",
    "object types",
    "Creating container",
    "Container ",
    "Image ",
    "Testcontainers",
    "Docker ",
    "Ryuk ",
    "Checking the system",
    "Connected to docker",
    "Compiled successfully",
    "Creating an optimized",
    "File sizes after",
    "The project was built",
    "You can control this",
    "The build folder",
    "You may serve",
    "Find out more about deployment",
    "serve -s build",
    "npm ",
    "added ",
    "packages are looking",
    "vulnerabilities",
    "Node v",
    "postinstall",
    "prebuild",
    "env-cmd",
    "react-app-rewired",
    "ExperimentalWarning",
    "node --trace",
    "cra.link",
    "To address",
    // Surefire emits these during build; suppressed so build-only runs
    // don't surface raw test noise.
    "Running ",
    "Tests run:",
    "Results:",
    "T E S T S",
];

/// Bare text noise from plugins (no [INFO]/[ERROR] prefix).
const BARE_TEXT_NOISE: &[&str] = &[
    "Server Version:",
    "API Version:",
    "Operating System:",
    "Total Memory:",
    "- http",
    "- Use ",
    "This means",
    "Possible means",
    "In automated builds",
    "and any configuration",
    "| databasechangelog",
];

/// Plugins whose executions stream arbitrary tool output (npm builds via
/// frontend-maven-plugin or exec-maven-plugin, liquibase+jooq codegen). Their
/// segments are suppressed down to error-ish lines in `filter_mvn_compile`.
/// Matches both Maven 3 long names and Maven 4 short names (`frontend`,
/// `exec`).
fn is_noisy_codegen_plugin(plugin: &str) -> bool {
    matches!(plugin, "exec" | "exec-maven-plugin" | "frontend" | "frontend-maven-plugin")
        || plugin.contains("jooq")
        || plugin.contains("liquibase")
}

/// Within a suppressed codegen/exec segment, keep only lines that indicate a
/// failure — Maven `[ERROR]`s, npm/webpack error output, non-zero exits.
fn is_errorish_segment_line(line: &str, stripped: &str) -> bool {
    line.starts_with(ERROR_TAG)
        || stripped.contains("ERR!")
        || stripped.contains("error")
        || stripped.contains("Error")
        || stripped.contains("ERROR")
        || stripped.contains("Failed")
        || stripped.contains("failed")
        || TOTAL_TIME_RE.is_match(stripped)
}

/// Collapse Maven's `Unknown lifecycle phase "x"` error to its first
/// sentence. The raw line ends with `-> [Help 1]`, so the boilerplate filter
/// would otherwise swallow the reason entirely, and the 30-phase listing in
/// the middle carries no signal for an agent that just typo'd a goal.
fn shorten_unknown_phase_error(stripped: &str) -> Option<String> {
    let idx = stripped.find("Unknown lifecycle phase ")?;
    let rest = &stripped[idx..];
    let sentence_end = rest.find(". ").map_or(rest.len(), |i| i + 1);
    Some(format!("[ERROR] {}", &rest[..sentence_end]))
}

/// Returns true if a compile-phase output line should be kept.
/// Expects pre-trimmed input from callers.
fn should_keep_compile_line(line: &str) -> bool {
    if line.is_empty() {
        return false;
    }

    // `mvn -V` environment banner, JVM restricted-method WARNINGs, SLF4J
    // static-binder complaints, os-maven-plugin detection — never actionable.
    if is_mvn_startup_noise(line) {
        return false;
    }

    let stripped = strip_maven_prefix(line);

    if line.starts_with(ERROR_TAG) {
        return !is_maven_boilerplate(line);
    }

    if stripped.contains("BUILD SUCCESS") || stripped.contains("BUILD FAILURE") {
        return true;
    }

    if TOTAL_TIME_RE.is_match(stripped) {
        return true;
    }

    // Strip [INFO] noise
    if line.starts_with(INFO_TAG) {
        if stripped.is_empty() {
            return false;
        }

        if stripped.starts_with("[stdout]") || stripped.starts_with("[stderr]") {
            return false;
        }

        // npm lifecycle script lines: "> my-app@1.0.0 build"
        if stripped.starts_with("> ") {
            return false;
        }

        for pattern in INFO_NOISE_PATTERNS {
            if stripped.contains(pattern) {
                return false;
            }
        }

        if stripped.contains("deprecat") || stripped.contains("WARNING") {
            return false;
        }

        // Code generator config params, bundle size lines, and enforcer
        // per-rule pass notifications (regex — slower, run last).
        if CODEGEN_CONFIG_RE.is_match(stripped)
            || BUNDLE_SIZE_RE.is_match(stripped)
            || ENFORCER_RULE_PASSED_RE.is_match(stripped)
        {
            return false;
        }

        return true;
    }

    if line.starts_with(WARNING_TAG) {
        return false;
    }

    for pattern in BARE_TEXT_NOISE {
        if line.contains(pattern) {
            return false;
        }
    }

    // Keep anything else (compilation errors without prefix, etc.)
    true
}

// ---------------------------------------------------------------------------
// Line filter for mvn checkstyle:check output
// ---------------------------------------------------------------------------

/// Maven "Help" footer emitted on BUILD FAILURE. These come prefixed with
/// `[ERROR]` but are not actionable for the user — just pointers to wiki
/// pages. They are distinct from real `[ERROR]` violations, so we match by
/// substring after stripping the prefix.
const CHECKSTYLE_HELP_BOILERPLATE: &[&str] = &[
    "Failed to execute goal",
    "To see the full stack trace",
    "Re-run Maven using",
    "For more information about the errors",
    "[Help 1]",
    "[Help 2]",
    "MojoFailureException",
    "cwiki.apache.org",
];

/// Filter `mvn clean` output — collapse to one line showing what was deleted
/// and total time. If clean is combined with a later goal (`mvn clean compile`)
/// that fails, keep `[ERROR]` lines so the user sees the actual compile error.
fn filter_mvn_clean(output: &str) -> String {
    let clean = strip_ansi(output);
    let mut deleted_count: usize = 0;
    let mut first_deleted: Option<&str> = None;
    let mut total_time: Option<&str> = None;
    let mut build_failure = false;
    let mut error_lines: Vec<&str> = Vec::new();

    for line in clean.lines() {
        let trimmed = line.trim();
        let stripped = strip_maven_prefix(trimmed);

        if let Some(path) = stripped.strip_prefix("Deleting ") {
            let path = path.trim();
            if deleted_count == 0 {
                first_deleted = Some(path);
            }
            deleted_count += 1;
            continue;
        }

        if stripped.contains("BUILD FAILURE") {
            build_failure = true;
            continue;
        }

        if total_time.is_none() {
            if let Some(t) = parse_total_time(stripped) {
                total_time = Some(t);
                continue;
            }
        }

        if error_lines.len() < MAX_FAILURES_SHOWN
            && trimmed.starts_with(ERROR_TAG)
            && !is_maven_boilerplate(trimmed)
        {
            let err = stripped.trim();
            if !err.is_empty() {
                error_lines.push(err);
            }
        }
    }

    let time_str = total_time.unwrap_or("?");

    if build_failure {
        let mut result = format!("mvn clean: BUILD FAILURE ({time_str})");
        for err in &error_lines {
            result.push('\n');
            result.push_str("  ");
            result.push_str(&truncate(err, MAX_LINE_LENGTH));
        }
        return result;
    }

    match deleted_count {
        0 => format!("mvn clean: nothing to clean ({time_str})"),
        1 => format!(
            "mvn clean: deleted {} ({time_str})",
            first_deleted.unwrap_or("")
        ),
        n => format!("mvn clean: deleted {n} targets ({time_str})"),
    }
}

/// Filter `mvn checkstyle:check` output:
/// - strip ANSI codes, mvn/JVM/os-detection startup noise
/// - strip Maven model problem WARNING block (10 stock lines)
/// - strip `[INFO] Scanning / Building / ---…---` separators
/// - keep violation lines, rewritten compactly:
///   `  path:line:col [RuleName] message`
/// - keep `There are N errors reported by Checkstyle` and
///   `You have N Checkstyle violations` summaries
/// - keep `BUILD SUCCESS` / `BUILD FAILURE` and `Total time`
/// - strip trailing Help-link boilerplate
fn filter_mvn_checkstyle(output: &str) -> String {
    let clean = strip_ansi(output);
    let mut result: Vec<String> = Vec::new();

    for raw in clean.lines() {
        // Drop cross-cutting startup noise first
        if is_mvn_startup_noise(raw) {
            continue;
        }

        let line = raw.trim();
        if line.is_empty() {
            continue;
        }

        // Violations: rewrite compactly
        if let Some(caps) = CHECKSTYLE_VIOLATION_RE.captures(line) {
            let path = &caps[1];
            let lineno = &caps[2];
            let col = caps.get(3).map(|m| m.as_str()).unwrap_or("");
            let rule = &caps[4];
            let msg = &caps[5];
            let compact = if col.is_empty() {
                format!("  {}:{} [{}] {}", path, lineno, rule, msg)
            } else {
                format!("  {}:{}:{} [{}] {}", path, lineno, col, rule, msg)
            };
            result.push(compact);
            continue;
        }

        let stripped = strip_maven_prefix(line);

        // Drop Help-link boilerplate emitted after BUILD FAILURE
        if line.starts_with(ERROR_TAG)
            && CHECKSTYLE_HELP_BOILERPLATE
                .iter()
                .any(|p| stripped.contains(p))
        {
            continue;
        }

        // Keep [INFO] summary & result lines
        if line.starts_with(INFO_TAG) {
            if stripped.is_empty() {
                continue;
            }

            // Keep: N-errors / N-violations / BUILD SUCCESS|FAILURE / Total time
            if stripped.contains("Checkstyle violations")
                || stripped.contains("reported by Checkstyle")
                || stripped.contains("BUILD SUCCESS")
                || stripped.contains("BUILD FAILURE")
                || TOTAL_TIME_RE.is_match(stripped)
            {
                result.push(stripped.to_string());
                continue;
            }

            // Drop everything else: Scanning, Building, separators, plugin
            // banners, `from pom.xml`, `Finished at:`, etc. These match
            // `is_maven_boilerplate` or known noise words.
            continue;
        }

        // Strip Maven model WARNING block (empty and boilerplate WARNINGs)
        if line.starts_with(WARNING_TAG) {
            continue;
        }

        // Bare `[ERROR]` continuation (e.g., blank separator between help blocks)
        if line == ERROR_TAG {
            continue;
        }

        // Anything else (e.g., unexpected bare errors not matching the rule
        // regex) — keep, in the spirit of the fallback principle.
        result.push(line.to_string());
    }

    if result.is_empty() {
        return "mvn checkstyle: ok".to_string();
    }

    result.join("\n")
}

// ---------------------------------------------------------------------------
// Line filter for mvn dependency:tree output
// ---------------------------------------------------------------------------

/// Filter `mvn dependency:tree` — strip Maven boilerplate, omitted duplicates,
/// and "version managed" annotations. Keep tree structure and conflicts.
/// Returns the tree depth of a dependency line (0 = root, 1 = direct dep, 2+ = transitive).
/// Counts tree-drawing segments: each `|  `, `+- `, `\- `, or `   ` at the start adds one level.
fn dep_tree_depth(line: &str) -> usize {
    let mut depth = 0;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 2 < bytes.len() {
        match (bytes[i], bytes[i + 1], bytes[i + 2]) {
            (b'|', b' ', b' ') | (b'+', b'-', b' ') | (b'\\', b'-', b' ') | (b' ', b' ', b' ') => {
                depth += 1;
                i += 3;
            }
            _ => break,
        }
    }
    depth
}

fn filter_mvn_dep_tree(output: &str) -> String {
    let clean = strip_ansi(output);

    // First pass: collect clean tree lines
    let mut tree_lines: Vec<String> = Vec::new();
    for line in clean.lines() {
        let trimmed = line.trim();

        if trimmed.is_empty() || is_maven_boilerplate(trimmed) {
            continue;
        }

        let stripped = strip_maven_prefix(trimmed);

        if trimmed.starts_with(WARNING_TAG) {
            continue;
        }
        if trimmed.starts_with(INFO_TAG)
            && (stripped.is_empty()
                || stripped.starts_with("Scanning ")
                || stripped.starts_with("Building ")
                || stripped.starts_with("Loaded ")
                || stripped.contains("from pom.xml")
                || stripped.contains("BUILD SUCCESS")
                || stripped.contains("BUILD FAILURE")
                || stripped.starts_with("Total time:")
                || stripped.starts_with("Finished at:"))
        {
            continue;
        }

        if stripped.contains("omitted for duplicate") {
            continue;
        }

        let cleaned = if stripped.contains("version managed from") {
            VERSION_MANAGED_RE.replace_all(stripped, "").into_owned()
        } else {
            stripped.to_string()
        };

        tree_lines.push(cleaned);
    }

    if tree_lines.is_empty() {
        return "mvn dependency:tree: no output".to_string();
    }

    // Second pass: collapse transitive deps (depth 2+) into counts on their parent
    let mut result_lines: Vec<String> = Vec::new();
    let mut i = 0;
    while i < tree_lines.len() {
        let depth = dep_tree_depth(&tree_lines[i]);

        if depth <= 1 {
            // Root or direct dep — count transitive children
            let mut transitive_count = 0;
            let mut j = i + 1;
            while j < tree_lines.len() {
                let child_depth = dep_tree_depth(&tree_lines[j]);
                if child_depth <= depth {
                    break;
                }
                if child_depth >= depth + 2 {
                    transitive_count += 1;
                }
                j += 1;
            }

            if depth == 1 && transitive_count > 0 {
                result_lines.push(format!(
                    "{} ({} transitive)",
                    tree_lines[i], transitive_count
                ));
            } else {
                result_lines.push(tree_lines[i].clone());
            }
        }
        // depth 2+ lines are skipped (counted above)
        i += 1;
    }

    result_lines.join("\n")
}

/// Maven dependency scopes, in display order.
const DEP_LIST_SCOPES: &[&str] = &["compile", "provided", "runtime", "system", "test", "import"];

/// Parse one `dependency:list` entry into `(scope, compact_coordinate)`.
/// Input shape after the maven prefix is stripped:
/// `group:artifact:type[:classifier]:version:scope[ (optional)][ -- module …]`.
/// The default `jar` packaging and the JPMS module note carry no information
/// for a dependency inventory, so both are dropped; classifiers are kept.
fn dep_list_entry(stripped: &str) -> Option<(String, String)> {
    let coord = stripped.split(" -- ").next().unwrap_or(stripped).trim();
    let (coord, optional) = match coord.strip_suffix("(optional)") {
        Some(c) => (c.trim(), true),
        None => (coord, false),
    };
    let parts: Vec<&str> = coord.split(':').collect();
    let (group, artifact, packaging, classifier, version, scope) = match parts.as_slice() {
        [g, a, t, v, s] => (*g, *a, *t, None, *v, *s),
        [g, a, t, c, v, s] => (*g, *a, *t, Some(*c), *v, *s),
        _ => return None,
    };
    if !DEP_LIST_SCOPES.contains(&scope) {
        return None;
    }
    // Guard against prose that happens to contain colons: coordinates never
    // contain whitespace.
    if coord.contains(char::is_whitespace) {
        return None;
    }
    let mut compact = match (packaging, classifier) {
        ("jar", None) => format!("{group}:{artifact}:{version}"),
        ("jar", Some(c)) => format!("{group}:{artifact}:{c}:{version}"),
        (_, None) => format!("{group}:{artifact}:{packaging}:{version}"),
        (_, Some(c)) => format!("{group}:{artifact}:{packaging}:{c}:{version}"),
    };
    if optional {
        compact.push_str(" (optional)");
    }
    Some((scope.to_string(), compact))
}

/// Filter `mvn dependency:list` output: dedupe entries across modules, group
/// them by scope, and drop the `[INFO]` prefix, default `jar` packaging and
/// JPMS `-- module` notes. Errors and BUILD FAILURE lines are preserved.
fn filter_mvn_dep_list(output: &str) -> String {
    let clean = strip_ansi(output);
    if clean.trim().is_empty() {
        return "mvn dependency:list: no output".to_string();
    }

    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut module_count = 0usize;
    let mut error_lines: Vec<String> = Vec::new();
    let mut build_failure = false;

    for line in clean.lines() {
        let trimmed = line.trim();
        let stripped = strip_maven_prefix(trimmed);

        if stripped.starts_with("The following files have been resolved") {
            module_count += 1;
            continue;
        }
        if trimmed.starts_with(ERROR_TAG) && !stripped.is_empty() {
            if error_lines.len() < 20 {
                error_lines.push(stripped.to_string());
            }
            continue;
        }
        if stripped.contains("BUILD FAILURE") {
            build_failure = true;
            continue;
        }
        if let Some((scope, compact)) = dep_list_entry(stripped) {
            let key = format!("{scope} {compact}");
            if seen.insert(key) {
                match groups.iter_mut().find(|(s, _)| *s == scope) {
                    Some((_, deps)) => deps.push(compact),
                    None => groups.push((scope, vec![compact])),
                }
            }
        }
    }

    if groups.is_empty() && error_lines.is_empty() && !build_failure {
        return "mvn dependency:list: no dependencies found".to_string();
    }

    groups.sort_by_key(|(scope, _)| {
        DEP_LIST_SCOPES.iter().position(|s| s == scope).unwrap_or(usize::MAX)
    });

    let total: usize = groups.iter().map(|(_, deps)| deps.len()).sum();
    let mut out = String::with_capacity(clean.len() / 4);
    if total > 0 {
        out.push_str(&format!("mvn dependency:list: {total} unique deps"));
        if module_count > 1 {
            let _ = write!(out, " across {module_count} modules");
        }
        for (scope, deps) in &mut groups {
            deps.sort();
            let _ = write!(out, "\n{scope} ({}):", deps.len());
            for dep in deps.iter() {
                let _ = write!(out, "\n  {dep}");
            }
        }
    }
    if !error_lines.is_empty() || build_failure {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&error_lines.join("\n"));
        if build_failure {
            if !error_lines.is_empty() {
                out.push('\n');
            }
            out.push_str("BUILD FAILURE");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::utils::count_tokens;

    fn pkgs(s: &str) -> Vec<String> {
        vec![s.to_string()]
    }

    #[test]
    fn test_test_counts_add() {
        let mut a = TestSummary {
            run: 10,
            failures: 1,
            errors: 2,
            skipped: 3,
        };
        let b = TestSummary {
            run: 100,
            failures: 20,
            errors: 30,
            skipped: 40,
        };
        a.add(&b);
        assert_eq!(a.run, 110);
        assert_eq!(a.failures, 21);
        assert_eq!(a.errors, 32);
        assert_eq!(a.skipped, 43);
    }

    // --- parse_goals ---

    #[test]
    fn test_parse_goals_detection() {
        let v = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();

        // Multiple goals + flags
        assert_eq!(parse_goals(&v("clean test-compile checkstyle:check -Dskip.npm -q")),
                   vec!["clean", "test-compile", "checkstyle:check"]);
        // -pl takes a value: `core` is NOT a goal
        assert_eq!(parse_goals(&v("-pl core test")), vec!["test"]);
        // -rf takes a value that contains ':' — must not be mistaken for a plugin goal
        assert_eq!(parse_goals(&v("-rf :mod verify")), vec!["verify"]);
        // single goal + attached -D flag
        assert_eq!(parse_goals(&v("test -Dtest=Foo")), vec!["test"]);
        // leading flag before goals
        assert_eq!(parse_goals(&v("-q clean install")), vec!["clean", "install"]);
        // no goals
        assert_eq!(parse_goals(&v("-version")), Vec::<String>::new());
        // plugin:goal form
        assert_eq!(parse_goals(&v("dependency:tree")), vec!["dependency:tree"]);
    }

    // --- chain_runs_tests ---

    #[test]
    fn test_chain_runs_tests() {
        let g = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert!(!chain_runs_tests(&g("clean test-compile checkstyle:check")));
        assert!(!chain_runs_tests(&g("clean compile")));
        assert!(chain_runs_tests(&g("clean test")));
        assert!(chain_runs_tests(&g("clean verify")));
        assert!(chain_runs_tests(&g("clean install")));
        // plugin goal forms
        assert!(chain_runs_tests(&g("surefire:test")));
        assert!(chain_runs_tests(&g("failsafe:integration-test")));
    }

    // --- Reactor Summary collapse + javac error dedup ---
    // Multi-module reactor with aggregated Surefire output + dual-emitted
    // javac errors. Originally captured from rtk-ai/rtk#782.

    #[test]
    fn test_reactor_test_pass_accumulates_modules() {
        let input = include_str!("../../../tests/fixtures/mvn_test_reactor_pass.txt");
        let output = filter_mvn_test(input);
        // Fixture has 6 modules totalling 20 tests — accumulation must not
        // report only the first module's count.
        assert!(
            output.contains("20 passed"),
            "multi-module accumulation broken, got: {output}"
        );
        let savings = 100.0
            - (count_tokens(&output) as f64 / count_tokens(input) as f64 * 100.0);
        assert!(savings >= 95.0, "expected ≥95%, got {:.1}%", savings);
    }

    #[test]
    fn test_reactor_test_fail_accumulates_and_dedups() {
        let input = include_str!("../../../tests/fixtures/mvn_test_reactor_fail.txt");
        let output = filter_mvn_test(input);
        assert!(output.contains("20 run, 2 failed"), "got: {output}");
        // Each failure appears once in the enumerated Failures block
        // (stack trace may still reference the method name — count enumerator lines).
        let enumerated = output
            .lines()
            .filter(|l| l.starts_with("1. ") || l.starts_with("2. "))
            .count();
        assert_eq!(enumerated, 2, "expected exactly 2 enumerated failures in: {output}");
        let savings = 100.0
            - (count_tokens(&output) as f64 / count_tokens(input) as f64 * 100.0);
        assert!(savings >= 85.0, "expected ≥85%, got {:.1}%", savings);
    }

    #[test]
    fn test_reactor_compile_success_collapses() {
        let input =
            include_str!("../../../tests/fixtures/mvn_compile_reactor_success.txt");
        let output = filter_mvn_compile(input);
        // Per-module SUCCESS lines must be collapsed; only BUILD SUCCESS +
        // Total time survive for an all-green reactor.
        assert!(output.contains("BUILD SUCCESS"), "got: {output}");
        assert!(!output.contains("edeal-common ....."), "got: {output}");
        let savings = 100.0
            - (count_tokens(&output) as f64 / count_tokens(input) as f64 * 100.0);
        assert!(savings >= 90.0, "expected ≥90%, got {:.1}%", savings);
    }

    #[test]
    fn test_reactor_compile_fail_dedups_and_names_module() {
        let input =
            include_str!("../../../tests/fixtures/mvn_compile_reactor_fail.txt");
        let output = filter_mvn_compile(input);
        // Each javac location must appear exactly once (inline; help-block copy deduped).
        assert_eq!(
            output.matches("UserService.java:[42,30]").count(),
            1,
            "error dedup broken: {output}"
        );
        // Failed module surfaced in compact reactor line.
        assert!(
            output.contains("FAILURE (edeal-webapp)"),
            "failed module missing from summary: {output}"
        );
        let savings = 100.0
            - (count_tokens(&output) as f64 / count_tokens(input) as f64 * 100.0);
        assert!(savings >= 70.0, "expected ≥70%, got {:.1}%", savings);
    }

    #[test]
    fn test_filter_pass_output() {
        let input = include_str!("../../../tests/fixtures/mvn_test_pass_mavenmcp.txt");
        let output = filter_mvn_test(input);
        assert!(
            output.contains("mvn test:"),
            "should contain summary prefix"
        );
        assert!(output.contains("183 passed"), "should show 183 passed");
        assert!(output.contains("4.748 s"), "should contain total time");
        assert!(
            !output.contains("[INFO]"),
            "should not contain raw [INFO] prefix"
        );
    }

    // --- Maven-native summary trailer ---
    // Mined from real agent sessions: rtk's output gets piped through
    // `grep -E 'Tests run:|BUILD SUCCESS|BUILD FAILURE|<<< FAILURE'`, so the
    // summary must contain Maven's own patterns verbatim.

    #[test]
    fn test_pass_summary_emits_maven_native_trailer() {
        let input = include_str!("../../../tests/fixtures/mvn_test_pass_mavenmcp.txt");
        let output = filter_mvn_test(input);
        let aggregate = regex::Regex::new(
            r"(?m)^Tests run: 183, Failures: 0, Errors: 0, Skipped: 0$",
        )
        .expect("test regex");
        assert!(
            aggregate.is_match(&output),
            "maven-native aggregate line missing:\n{output}"
        );
        assert_eq!(
            output.lines().last(),
            Some("BUILD SUCCESS"),
            "BUILD SUCCESS must be the final line:\n{output}"
        );
    }

    #[test]
    fn test_fail_summary_emits_maven_native_aggregate_and_failure_marks() {
        let input = include_str!("../../../tests/fixtures/mvn_test_reactor_fail.txt");
        let output = filter_mvn_test(input);
        let aggregate = regex::Regex::new(
            r"(?m)^Tests run: 20, Failures: \d+, Errors: \d+, Skipped: \d+$",
        )
        .expect("test regex");
        assert!(
            aggregate.is_match(&output),
            "maven-native aggregate line missing:\n{output}"
        );
        let marked = output
            .lines()
            .filter(|l| l.ends_with("<<< FAILURE!"))
            .count();
        assert_eq!(
            marked, 2,
            "each enumerated failure must carry Maven's <<< FAILURE! marker:\n{output}"
        );
    }

    #[test]
    fn test_filter_maven4_pass_smoke() {
        // Maven 4.0.0-rc output: short plugin names in markers
        // (`surefire:3.5.6:test`), `-- in <Class>` per-class summaries, and
        // JPMS/final-field WARNING preamble. The test state machine must
        // still produce the compact pass summary.
        let input = include_str!("../../../tests/fixtures/mvn4_test_pass_auth.txt");
        let output = filter_mvn_test(input);
        assert!(
            output.contains("13 passed"),
            "should show pass count, got:\n{}",
            output
        );
        assert!(output.contains("19.543 s"), "should contain total time");
        assert!(
            !output.contains("WARNING"),
            "Maven 4 JPMS warnings must be dropped, got:\n{}",
            output
        );
    }

    #[test]
    fn test_filter_maven4_pass_snapshot() {
        let input = include_str!("../../../tests/fixtures/mvn4_test_pass_auth.txt");
        let output = filter_mvn_test(input);
        insta::assert_snapshot!(output);
    }

    #[test]
    fn test_filter_maven4_pass_savings() {
        let input = include_str!("../../../tests/fixtures/mvn4_test_pass_auth.txt");
        let output = filter_mvn_test(input);

        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 60.0,
            "maven 4 test pass: expected >=60% savings, got {:.1}% ({} -> {} tokens)\nOutput:\n{}",
            savings,
            input_tokens,
            output_tokens,
            output,
        );
    }

    #[test]
    fn test_filter_fail_keeps_caused_by_chain() {
        // Usage analysis: after failed runs, agents grep the tee log for
        // 'Caused by' — the text filter's 3-detail-line cap cut the root
        // cause off. Cause headers must survive the cap.
        let input = include_str!("../../../tests/fixtures/mvn_test_fail_caused_by.txt");
        let output = filter_mvn_test(input);
        assert!(
            output.contains("Caused by: org.springframework.beans.factory.BeanCreationException"),
            "intermediate cause header must be kept, got:\n{}",
            output
        );
        assert!(
            output.contains("Missing required property 'auth.scim.encryption-key'"),
            "root cause header must be kept, got:\n{}",
            output
        );
    }

    #[test]
    fn test_filter_fail_caused_by_snapshot() {
        let input = include_str!("../../../tests/fixtures/mvn_test_fail_caused_by.txt");
        let output = filter_mvn_test(input);
        insta::assert_snapshot!(output);
    }

    #[test]
    fn test_filter_fail_output() {
        let input = include_str!("../../../tests/fixtures/mvn_test_fail_auth.txt");
        let output = filter_mvn_test(input);
        assert!(
            output.contains("5 run, 2 failed"),
            "should show run/failed counts, got: {}",
            output
        );
        assert!(output.contains("23.819 s"), "should contain total time");
        assert!(
            output.contains("EmailParserTest.should_extract_domain_from_email"),
            "should list first failure"
        );
        assert!(
            output.contains("ScoreTypeTest.shouldMapToRole"),
            "should list second failure"
        );
        assert!(
            output.contains("broken.example.com"),
            "should include assertion details"
        );
        assert!(
            !output.contains("surefire-reports"),
            "should strip boilerplate"
        );
        assert!(
            !output.contains("cwiki.apache.org"),
            "should strip help links"
        );
    }

    #[test]
    fn test_pass_savings() {
        let input = include_str!("../../../tests/fixtures/mvn_test_pass_mavenmcp.txt");
        let output = filter_mvn_test(input);

        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);

        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 90.0,
            "mvn test pass: expected >=90% savings, got {:.1}% ({} -> {} tokens)",
            savings,
            input_tokens,
            output_tokens,
        );
    }

    #[test]
    fn test_fail_savings() {
        let input = include_str!("../../../tests/fixtures/mvn_test_fail_auth.txt");
        let output = filter_mvn_test(input);

        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);

        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 60.0,
            "mvn test fail: expected >=60% savings, got {:.1}% ({} -> {} tokens)",
            savings,
            input_tokens,
            output_tokens,
        );
    }

    #[test]
    fn test_filter_large_suite() {
        let input = include_str!("../../../tests/fixtures/mvn_test_large_suite.txt");
        let output = filter_mvn_test(input);
        assert!(
            output.contains("3262 run, 23 failed"),
            "should show run/failed counts, got: {}",
            output
        );
        assert!(
            output.contains("+13 more failures"),
            "should cap at 10 and show remaining"
        );
        assert!(
            output.contains("SearchReadModelTest"),
            "should list assertion failures"
        );
        assert!(
            output.contains("PatchableFieldTest"),
            "should list compilation errors"
        );
    }

    #[test]
    fn test_large_suite_savings() {
        let input = include_str!("../../../tests/fixtures/mvn_test_large_suite.txt");
        let output = filter_mvn_test(input);

        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 60.0,
            "mvn test large suite: expected >=60% savings, got {:.1}% ({} -> {} tokens)",
            savings,
            input_tokens,
            output_tokens,
        );
    }

    #[test]
    fn pass_frame_is_prefixed_maven_subset() {
        let input = include_str!("../../../tests/fixtures/mvn_test_pass_mavenmcp.txt");
        let out = filter_mvn_test(input);
        assert!(out.contains("[INFO] Tests run: 183, Failures: 0, Errors: 0, Skipped: 0"),
            "prefixed aggregate missing:\n{out}");
        assert_eq!(out.lines().last(), Some("[INFO] BUILD SUCCESS"), "\n{out}");
        assert!(!out.contains("mvn test:"), "synthetic headline leaked:\n{out}");
        assert!(!out.contains("Total time"), "Total time leaked:\n{out}");
    }

    #[test]
    fn fail_frame_is_prefixed_maven_subset() {
        let input = include_str!("../../../tests/fixtures/mvn_test_reactor_fail.txt");
        let out = filter_mvn_test(input);
        assert!(out.contains("[ERROR] Tests run: 20, Failures:"), "\n{out}");
        assert!(out.contains("[INFO] BUILD FAILURE"), "\n{out}");
        assert!(out.contains("[ERROR] Failures:"), "\n{out}");
        assert!(!out.contains("mvn test:"), "synthetic headline leaked:\n{out}");
    }

    #[test]
    fn no_tests_uses_native_surefire_warning() {
        let out = filter_mvn_test("[INFO] Building my-project 1.0\n[INFO] BUILD SUCCESS\n");
        assert_eq!(out, "[WARNING] No tests were executed!");
    }

    #[test]
    fn test_empty_input() {
        let output = filter_mvn_test("");
        assert_eq!(output, "mvn test: No tests run");
    }

    #[test]
    fn test_filter_many_failures_output() {
        let input = include_str!("../../../tests/fixtures/mvn_test_many_failures.txt");
        let output = filter_mvn_test(input);
        assert!(
            output.contains("28 run, 28 failed"),
            "should show total run/failed counts, got: {}",
            output
        );
        assert!(
            output.contains("+4 more failures"),
            "should cap at 10 and show remaining count"
        );
    }

    #[test]
    fn test_many_failures_savings() {
        let input = include_str!("../../../tests/fixtures/mvn_test_many_failures.txt");
        let output = filter_mvn_test(input);

        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 60.0,
            "mvn test many failures: expected >=60% savings, got {:.1}% ({} -> {} tokens)",
            savings,
            input_tokens,
            output_tokens,
        );
    }

    #[test]
    fn test_filter_multimodule_output() {
        let input = include_str!("../../../tests/fixtures/mvn_test_multimodule.txt");
        let output = filter_mvn_test(input);
        assert!(
            output.contains("860 run, 4 failed"),
            "should show total run/failed across modules, got: {}",
            output
        );
        assert!(
            output.contains("GitDiffReaderTest.shouldBuildDiff"),
            "should list failure from services module"
        );
        assert!(
            output.contains("ServiceUnavailableException"),
            "should include error details"
        );
        assert!(
            output.contains("01:31 min"),
            "should contain total time"
        );
    }

    #[test]
    fn test_multimodule_savings() {
        let input = include_str!("../../../tests/fixtures/mvn_test_multimodule.txt");
        let output = filter_mvn_test(input);

        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 60.0,
            "mvn test multimodule: expected >=60% savings, got {:.1}% ({} -> {} tokens)",
            savings,
            input_tokens,
            output_tokens,
        );
    }

    #[test]
    fn test_filter_pass_large_ansi() {
        let input = include_str!("../../../tests/fixtures/mvn_test_pass_large_ansi.txt");
        let output = filter_mvn_test(input);
        assert!(
            output.contains("950 passed"),
            "should show 950 passed (959-9 skipped), got: {}",
            output
        );
        assert!(
            output.contains("9 skipped"),
            "should show 9 skipped"
        );
        assert!(
            output.contains("01:32 min"),
            "should contain total time"
        );
        assert!(
            !output.contains("PortUnreachableException"),
            "should strip app log noise"
        );
        assert!(
            !output.contains("[stdout]"),
            "should strip [stdout] lines"
        );
        assert!(
            !output.contains("liquibase"),
            "should strip liquibase stderr"
        );
    }

    #[test]
    fn test_pass_large_ansi_savings() {
        let input = include_str!("../../../tests/fixtures/mvn_test_pass_large_ansi.txt");
        let output = filter_mvn_test(input);

        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        // 94% not 95%: the fixture is only ~350 tokens, so the fixed-size
        // maven-native trailer (Tests run/BUILD SUCCESS, ~13 tokens) weighs
        // ~4% here while being noise on real multi-thousand-token logs.
        assert!(
            savings >= 94.0,
            "mvn test large ANSI pass: expected >=94% savings, got {:.1}% ({} -> {} tokens)",
            savings,
            input_tokens,
            output_tokens,
        );
    }

    #[test]
    fn test_no_test_section() {
        let input = "[INFO] Building my-project 1.0\n[INFO] BUILD SUCCESS\n";
        let output = filter_mvn_test(input);
        assert_eq!(output, "mvn test: No tests run");
    }

    // --- dependency:tree tests ---

    #[test]
    fn test_dep_tree_simple() {
        let input = include_str!("../../../tests/fixtures/mvn_dep_tree_simple.txt");
        let output = filter_mvn_dep_tree(input);
        assert!(
            output.contains("com.example:my-app:jar:1.0.0"),
            "should contain root artifact, got: {}",
            output
        );
        assert!(
            output.contains("slf4j-api"),
            "should contain direct dep"
        );
        assert!(
            output.contains("guava"),
            "should contain guava"
        );
        assert!(
            !output.contains("[INFO]"),
            "should strip [INFO] prefix"
        );
        assert!(
            !output.contains("BUILD SUCCESS"),
            "should strip boilerplate"
        );
        assert!(
            !output.contains("Scanning"),
            "should strip preamble"
        );
    }

    #[test]
    fn test_dep_tree_conflicts() {
        let input = include_str!("../../../tests/fixtures/mvn_dep_tree_conflicts.txt");
        let output = filter_mvn_dep_tree(input);
        assert!(
            output.contains("omitted for conflict with 2.18.3"),
            "should keep conflict info, got: {}",
            output
        );
        assert!(
            !output.contains("BUILD SUCCESS"),
            "should strip boilerplate"
        );
    }

    #[test]
    fn test_dep_tree_beacon_strips_duplicates() {
        let input = include_str!("../../../tests/fixtures/mvn_dep_tree_beacon.txt");
        let output = filter_mvn_dep_tree(input);
        assert!(
            !output.contains("omitted for duplicate"),
            "should strip all 'omitted for duplicate' lines"
        );
        assert!(
            output.contains("com.example:beacon"),
            "should contain root artifact"
        );
        assert!(
            output.contains("spring-boot-starter-web"),
            "should contain direct deps"
        );
    }

    #[test]
    fn test_dep_tree_beacon_cleans_version_managed() {
        let input = include_str!("../../../tests/fixtures/mvn_dep_tree_beacon.txt");
        let output = filter_mvn_dep_tree(input);
        assert!(
            !output.contains("version managed from"),
            "should strip 'version managed' annotations"
        );
    }

    #[test]
    fn test_dep_tree_beacon_savings() {
        let input = include_str!("../../../tests/fixtures/mvn_dep_tree_beacon.txt");
        let output = filter_mvn_dep_tree(input);

        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 60.0,
            "mvn dep tree beacon: expected >=60% savings, got {:.1}% ({} -> {} tokens)",
            savings,
            input_tokens,
            output_tokens,
        );
    }

    #[test]
    fn test_dep_tree_simple_savings() {
        let input = include_str!("../../../tests/fixtures/mvn_dep_tree_simple.txt");
        let output = filter_mvn_dep_tree(input);

        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);

        // Small fixtures (22 lines) can't hit 60% savings — verified by beacon fixture.
        // Here we just verify the filter actually reduces output.
        assert!(
            output_tokens < input_tokens,
            "mvn dep tree simple: filter should reduce output ({} -> {} tokens)",
            input_tokens,
            output_tokens,
        );
    }

    #[test]
    fn test_dep_tree_empty() {
        let output = filter_mvn_dep_tree("");
        assert_eq!(output, "mvn dependency:tree: no output");
    }

    // --- dependency:list ---

    #[test]
    fn test_dep_list_snapshot() {
        let input = include_str!("../../../tests/fixtures/mvn_dependency_list_auth.txt");
        let output = filter_mvn_dep_list(input);
        insta::assert_snapshot!(output);
    }

    #[test]
    fn test_dep_list_savings() {
        let input = include_str!("../../../tests/fixtures/mvn_dependency_list_auth.txt");
        let output = filter_mvn_dep_list(input);

        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 60.0,
            "mvn dependency:list: expected >=60% savings, got {:.1}% ({} -> {} tokens)",
            savings,
            input_tokens,
            output_tokens,
        );
    }

    #[test]
    fn test_dep_list_groups_by_scope_and_strips_noise() {
        let input = include_str!("../../../tests/fixtures/mvn_dependency_list_auth.txt");
        let output = filter_mvn_dep_list(input);

        assert!(output.contains("compile ("), "should have a compile scope group");
        assert!(output.contains("test ("), "should have a test scope group");
        // Coordinates keep group:artifact:version, drop packaging + JPMS noise
        assert!(
            output.contains("ch.qos.logback:logback-classic:1.5.34"),
            "compact coordinate expected"
        );
        assert!(!output.contains("-- module"), "JPMS module noise must be dropped");
        assert!(!output.contains(":jar:"), "default packaging token must be dropped");
        assert!(!output.contains("[INFO]"), "maven prefixes must be dropped");
    }

    #[test]
    fn test_dep_list_empty() {
        let output = filter_mvn_dep_list("");
        assert_eq!(output, "mvn dependency:list: no output");
    }

    #[test]
    fn test_dep_list_malformed_passthrough_no_panic() {
        let output = filter_mvn_dep_list("not valid maven output\nrandom text\n");
        assert!(!output.is_empty());
    }

    #[test]
    fn test_dep_list_failure_keeps_errors() {
        let input = "[INFO] Scanning for projects...\n\
                     [ERROR] Failed to execute goal on project app: Could not resolve dependencies\n\
                     [INFO] BUILD FAILURE\n\
                     [INFO] Total time:  1.2 s\n";
        let output = filter_mvn_dep_list(input);
        assert!(output.contains("Could not resolve dependencies"));
        assert!(output.contains("BUILD FAILURE"));
    }

    #[test]
    fn test_dep_tree_ansi_codes_stripped() {
        let input = "\x1b[34;1m[INFO]\x1b[0m com.example:app:jar:1.0\n\
                      \x1b[34;1m[INFO]\x1b[0m +- org.junit:junit:jar:5.0:test\n\
                      \x1b[34;1m[INFO]\x1b[0m |  \\- org.hamcrest:hamcrest:jar:2.0:test\n\
                      \x1b[34;1m[INFO]\x1b[0m \\- com.google:guava:jar:33.0:compile";
        let output = filter_mvn_dep_tree(input);
        assert!(
            !output.contains("\x1b["),
            "output should not contain ANSI escape codes"
        );
        assert!(
            output.contains("com.example:app"),
            "should contain root artifact"
        );
        assert!(
            output.contains("junit"),
            "should contain direct dep"
        );
        assert!(
            !output.contains("hamcrest"),
            "should collapse transitive dep"
        );
    }

    #[test]
    fn test_dep_tree_large_collapses_transitive() {
        let input = include_str!("../../../tests/fixtures/mvn_dep_tree_large.txt");
        let output = filter_mvn_dep_tree(input);

        // Should show root artifact
        assert!(
            output.contains("com.example.demo:webapp"),
            "should contain root artifact"
        );

        // Direct deps should be listed
        assert!(
            output.contains("spring-boot-starter-actuator"),
            "should contain direct dep"
        );

        // Transitive deps (depth 2+) should NOT appear as separate lines
        assert!(
            !output.contains("logback-classic"),
            "should not show transitive dep logback-classic"
        );
        assert!(
            !output.contains("logback-core"),
            "should not show transitive dep logback-core"
        );

        // Direct deps with children should show transitive count
        assert!(
            output.contains("transitive"),
            "should show transitive count for deps with children"
        );

        // Output should be dramatically smaller
        let output_lines = output.lines().count();
        assert!(
            output_lines < 40,
            "collapsed tree should be under 40 lines, got {}",
            output_lines
        );
    }

    #[test]
    fn test_dep_tree_large_savings_above_80() {
        let input = include_str!("../../../tests/fixtures/mvn_dep_tree_large.txt");
        let output = filter_mvn_dep_tree(input);

        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 80.0,
            "mvn dep tree large: expected >=80% savings, got {:.1}% ({} -> {} tokens)",
            savings, input_tokens, output_tokens,
        );
    }

    #[test]
    fn snapshot_dep_tree_beacon() {
        let input = include_str!("../../../tests/fixtures/mvn_dep_tree_beacon.txt");
        let output = filter_mvn_dep_tree(input);
        insta::assert_snapshot!(output);
    }

    // --- compile filter tests (auth project: jOOQ + typescript-generator + React) ---

    #[test]
    fn test_filter_compile_auth() {
        let input = include_str!("../../../tests/fixtures/mvn_compile_auth.txt");
        let output = filter_mvn_compile(input);

        // Must preserve critical lines
        assert!(
            output.contains("BUILD SUCCESS"),
            "should keep BUILD SUCCESS, got: {}",
            output
        );
        assert!(
            output.contains("Total time:"),
            "should keep Total time"
        );

        // Must strip plugin noise
        assert!(
            !output.contains("[stdout]"),
            "should strip [stdout] lines"
        );
        assert!(
            !output.contains("Generating table"),
            "should strip jOOQ codegen"
        );
        assert!(
            !output.contains("Generating record"),
            "should strip jOOQ record gen"
        );
        assert!(
            !output.contains("Generating routine"),
            "should strip jOOQ routine gen"
        );
        assert!(
            !output.contains("Missing name"),
            "should strip jOOQ warnings"
        );
        assert!(
            !output.contains("kB  build/static"),
            "should strip bundle sizes"
        );
        assert!(
            !output.contains("The project was built"),
            "should strip CRA messages"
        );
        assert!(
            !output.contains("npm fund"),
            "should strip npm messages"
        );
        assert!(
            !output.contains("Server Version:"),
            "should strip Docker bare text"
        );
        assert!(
            !output.contains("Parsing"),
            "should strip typescript-generator parsing lines"
        );
        assert!(
            !output.contains("Loading class"),
            "should strip typescript-generator loading lines"
        );
    }

    #[test]
    fn test_compile_auth_savings() {
        let input = include_str!("../../../tests/fixtures/mvn_compile_auth.txt");
        let output = filter_mvn_compile(input);

        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 90.0,
            "mvn compile auth: expected >=90% savings, got {:.1}% ({} -> {} tokens)\nOutput:\n{}",
            savings,
            input_tokens,
            output_tokens,
            output,
        );
    }

    #[test]
    fn test_compile_npm_codegen_snapshot() {
        let input = include_str!("../../../tests/fixtures/mvn_compile_npm_codegen.txt");
        let output = filter_mvn_compile(input);
        insta::assert_snapshot!(output);
    }

    #[test]
    fn test_compile_npm_codegen_savings() {
        // Real `mvn compile` WITHOUT -Dskip.npm: frontend npm ci/build,
        // testcontainers+liquibase+jooq codegen, typescript-generator.
        // Usage analysis: such runs averaged 59% savings vs 99.7% with
        // -Dskip.npm — the npm/codegen segments are the gap.
        let input = include_str!("../../../tests/fixtures/mvn_compile_npm_codegen.txt");
        let output = filter_mvn_compile(input);

        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 90.0,
            "mvn compile npm+codegen: expected >=90% savings, got {:.1}% ({} -> {} tokens)\nOutput:\n{}",
            savings,
            input_tokens,
            output_tokens,
            output,
        );
    }

    #[test]
    fn test_compile_npm_codegen_collapses_noise() {
        let input = include_str!("../../../tests/fixtures/mvn_compile_npm_codegen.txt");
        let output = filter_mvn_compile(input);

        assert!(!output.contains("npm warn deprecated"), "npm deprecation spam must be dropped");
        assert!(
            !output.contains("build/static/js/"),
            "webpack bundle-size listing must be dropped"
        );
        assert!(
            !output.contains("Generating table"),
            "jooq per-table codegen lines must be dropped"
        );
        assert!(
            !output.contains("Missing name"),
            "jooq missing-name chatter must be dropped"
        );
        assert!(output.contains("BUILD SUCCESS"), "verdict must stay");
    }

    #[test]
    fn test_compile_map_liquibase_jooq_snapshot() {
        let input = include_str!("../../../tests/fixtures/mvn_compile_map_liquibase_jooq.txt");
        let output = filter_mvn_compile(input);
        insta::assert_snapshot!(output);
    }

    #[test]
    fn test_compile_map_liquibase_jooq_savings() {
        // Real `mvn compile` from a project whose codegen runs liquibase +
        // jooq on stderr (no [INFO] prefix) and npm via exec-maven-plugin.
        // Usage analysis: 12 such runs in July 2026 leaked 70k tokens each
        // (57% savings) — bare-stderr liquibase/jooq chatter was kept.
        let input = include_str!("../../../tests/fixtures/mvn_compile_map_liquibase_jooq.txt");
        let output = filter_mvn_compile(input);

        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 85.0,
            "mvn compile map: expected >=85% savings, got {:.1}% ({} -> {} tokens)\nOutput:\n{}",
            savings,
            input_tokens,
            output_tokens,
            output,
        );
    }

    #[test]
    fn test_compile_map_drops_bare_stderr_noise() {
        let input = include_str!("../../../tests/fixtures/mvn_compile_map_liquibase_jooq.txt");
        let output = filter_mvn_compile(input);

        assert!(
            !output.contains("Running Changeset:"),
            "bare liquibase changeset lines must be dropped"
        );
        assert!(!output.contains("@@@@"), "liquibase ASCII banner must be dropped");
        assert!(
            !output.contains("(?i:TIMESTAMP"),
            "jooq forcedType regex echo must be dropped"
        );
        assert!(
            !output.contains("</includeExpression>"),
            "jooq forcedType XML echo must be dropped"
        );
        assert!(
            !output.contains("JsonSchemaGenerator"),
            "timestamped SLF4J codegen warnings must be dropped"
        );
        assert!(
            !output.contains("> redocly"),
            "bare npm script banner lines must be dropped"
        );
        assert!(output.contains("BUILD SUCCESS"), "verdict must stay");
    }

    #[test]
    fn test_compile_unknown_phase_one_liner() {
        // `mvn build compile` — "build" is not a Maven 3 phase. The
        // informative error line ends with "-> [Help 1]", which the
        // boilerplate filter used to swallow whole, leaving a bare
        // BUILD FAILURE with no reason. Keep the first sentence, drop the
        // 30-phase listing and Help boilerplate.
        let input = include_str!("../../../tests/fixtures/mvn_unknown_phase_raw.txt");
        let output = filter_mvn_compile(input);
        assert!(
            output.contains("Unknown lifecycle phase \"build\"."),
            "the reason must survive filtering, got:\n{}",
            output
        );
        assert!(
            !output.contains("Available lifecycle phases are"),
            "the 30-phase listing must be dropped, got:\n{}",
            output
        );
        assert!(output.contains("BUILD FAILURE"));
    }

    #[test]
    fn test_compile_unknown_phase_snapshot() {
        let input = include_str!("../../../tests/fixtures/mvn_unknown_phase_raw.txt");
        let output = filter_mvn_compile(input);
        insta::assert_snapshot!(output);
    }

    #[test]
    fn test_compile_success_only() {
        let input = "[INFO] BUILD SUCCESS\n[INFO] Total time: 2.5 s\n";
        let output = filter_mvn_compile(input);
        assert!(output.contains("BUILD SUCCESS"));
        assert!(output.contains("Total time:"));
    }

    #[test]
    fn test_compile_strips_stdout_lines() {
        let input = "[INFO] [stdout] Parsing 'com.example.Foo'\n\
                      [INFO] [stdout] Loading class java.lang.String\n\
                      [INFO] [stdout] Writing declarations to: /tmp/out.d.ts\n\
                      [INFO] BUILD SUCCESS\n\
                      [INFO] Total time: 1.0 s\n";
        let output = filter_mvn_compile(input);
        assert!(!output.contains("[stdout]"), "should strip all [stdout] lines");
        assert!(output.contains("BUILD SUCCESS"));
    }

    #[test]
    fn test_compile_strips_codegen_config() {
        let input = "[INFO]   dialect                : POSTGRES_15\n\
                      [INFO]   generated              : false\n\
                      [INFO]   JPA                    : false\n\
                      [INFO] BUILD SUCCESS\n\
                      [INFO] Total time: 1.0 s\n";
        let output = filter_mvn_compile(input);
        assert!(!output.contains("dialect"), "should strip codegen config");
        assert!(!output.contains("JPA"), "should strip codegen config");
        assert!(output.contains("BUILD SUCCESS"));
    }

    #[test]
    fn test_compile_strips_bundle_sizes() {
        let input = "[INFO]   257.55 kB  build/static/js/main.js\n\
                      [INFO]   40.41 kB   build/static/js/962.chunk.js\n\
                      [INFO]   918 B      build/static/js/636.chunk.js\n\
                      [INFO] BUILD SUCCESS\n\
                      [INFO] Total time: 1.0 s\n";
        let output = filter_mvn_compile(input);
        assert!(!output.contains("kB"), "should strip bundle sizes");
        assert!(!output.contains("918 B"), "should strip small bundle sizes");
        assert!(output.contains("BUILD SUCCESS"));
    }

    #[test]
    fn test_compile_preserves_errors() {
        let input = "[INFO] Compiling 42 source files\n\
                      [ERROR] /src/Foo.java:[10,5] cannot find symbol\n\
                      [INFO] BUILD FAILURE\n\
                      [INFO] Total time: 1.0 s\n";
        let output = filter_mvn_compile(input);
        assert!(
            output.contains("[ERROR]"),
            "should preserve [ERROR] lines, got: {}",
            output
        );
        assert!(output.contains("cannot find symbol"));
        assert!(output.contains("BUILD FAILURE"));
    }

    #[test]
    fn snapshot_compile_auth() {
        let input = include_str!("../../../tests/fixtures/mvn_compile_auth.txt");
        let output = filter_mvn_compile(input);
        insta::assert_snapshot!(output);
    }

    // --- clean filter tests ---

    #[test]
    fn test_filter_mvn_clean_real_fixture() {
        // Exact output shape covered by snapshot_clean_auth; here we guard the
        // core invariant: a single-module success collapses to exactly one line.
        let input = include_str!("../../../tests/fixtures/mvn_clean_auth.txt");
        let output = filter_mvn_clean(input);
        assert_eq!(
            output.lines().count(),
            1,
            "single-module clean should collapse to one line, got: {}",
            output
        );
    }

    #[test]
    fn test_filter_mvn_clean_savings() {
        let input = include_str!("../../../tests/fixtures/mvn_clean_auth.txt");
        let output = filter_mvn_clean(input);
        let savings = 100.0 - (count_tokens(&output) as f64 / count_tokens(input) as f64 * 100.0);
        assert!(
            savings >= 90.0,
            "mvn clean: expected ≥90% savings, got {:.1}% ({} -> {} tokens)\nOutput: {}",
            savings,
            count_tokens(input),
            count_tokens(&output),
            output,
        );
    }

    #[test]
    fn test_filter_mvn_clean_no_deletions() {
        // First clean of a never-built project: no `Deleting` lines, but BUILD SUCCESS.
        let input = "[INFO] Scanning for projects...\n\
                     [INFO] Building sample 1.0\n\
                     [INFO] BUILD SUCCESS\n\
                     [INFO] Total time:  0.523 s\n";
        let output = filter_mvn_clean(input);
        assert_eq!(output, "mvn clean: nothing to clean (0.523 s)");
    }

    #[test]
    fn test_filter_mvn_clean_multi_module() {
        let input = "[INFO] Deleting /repo/mod-a/target\n\
                     [INFO] Deleting /repo/mod-b/target\n\
                     [INFO] Deleting /repo/mod-c/target\n\
                     [INFO] BUILD SUCCESS\n\
                     [INFO] Total time:  2.101 s\n";
        let output = filter_mvn_clean(input);
        assert_eq!(output, "mvn clean: deleted 3 targets (2.101 s)");
    }

    #[test]
    fn test_filter_mvn_clean_build_failure_keeps_errors() {
        // `mvn clean compile` failing at compile — clean filter must still surface [ERROR] lines.
        let input = "[INFO] Deleting /repo/target\n\
                     [ERROR] COMPILATION ERROR\n\
                     [ERROR] /repo/src/main/java/Foo.java:[12,5] cannot find symbol\n\
                     [ERROR] symbol:   method bar()\n\
                     [INFO] BUILD FAILURE\n\
                     [INFO] Total time:  0.9 s\n";
        let output = filter_mvn_clean(input);
        assert!(output.starts_with("mvn clean: BUILD FAILURE (0.9 s)"));
        assert!(output.contains("COMPILATION ERROR"));
        assert!(output.contains("cannot find symbol"));
    }

    #[test]
    fn snapshot_clean_auth() {
        let input = include_str!("../../../tests/fixtures/mvn_clean_auth.txt");
        let output = filter_mvn_clean(input);
        insta::assert_snapshot!(output);
    }

    // --- goal routing (dispatch / route_goal) ---

    #[test]
    fn test_route_goal() {
        assert_eq!(route_goal("compile"), GoalRouting::Compile);
        assert_eq!(route_goal("process-classes"), GoalRouting::Compile);
        assert_eq!(route_goal("test-compile"), GoalRouting::Compile);
        assert_eq!(route_goal("checkstyle:check"), GoalRouting::Checkstyle);
        assert_eq!(route_goal("checkstyle"), GoalRouting::Checkstyle);
        // Test-output state-machine goals (surefire/failsafe + XML enrichment):
        assert_eq!(route_goal("test"), GoalRouting::TestsLike(TestLikeGoal::Test));
        assert_eq!(
            route_goal("verify"),
            GoalRouting::TestsLike(TestLikeGoal::Verify)
        );
        // Lifecycle goals run the full test phase, so they share the same
        // test-output filter — each running its OWN goal name, not "verify":
        assert_eq!(
            route_goal("integration-test"),
            GoalRouting::TestsLike(TestLikeGoal::IntegrationTest)
        );
        assert_eq!(
            route_goal("package"),
            GoalRouting::TestsLike(TestLikeGoal::Package)
        );
        assert_eq!(
            route_goal("install"),
            GoalRouting::TestsLike(TestLikeGoal::Install)
        );
        assert_eq!(
            route_goal("deploy"),
            GoalRouting::TestsLike(TestLikeGoal::Deploy)
        );
        assert_eq!(route_goal("clean"), GoalRouting::Clean);
        assert_eq!(route_goal("dependency:tree"), GoalRouting::DepTree);
        assert_eq!(route_goal("dependency:list"), GoalRouting::DepList);
        // Direct plugin-goal invocations run the same test machinery, so they
        // share the test-output filter — invoked verbatim (plugin goal, not a
        // lifecycle phase):
        assert_eq!(
            route_goal("failsafe:integration-test"),
            GoalRouting::TestsLike(TestLikeGoal::FailsafeIntegrationTest)
        );
        assert_eq!(
            route_goal("failsafe:verify"),
            GoalRouting::TestsLike(TestLikeGoal::FailsafeVerify)
        );
        assert_eq!(
            route_goal("surefire:test"),
            GoalRouting::TestsLike(TestLikeGoal::SurefireTest)
        );
        // Still passthrough — no dedicated filter / long-running goals:
        assert_eq!(route_goal("spring-boot:run"), GoalRouting::Passthrough);
        assert_eq!(route_goal("quarkus:dev"), GoalRouting::Passthrough);
        assert_eq!(route_goal("compilee"), GoalRouting::Passthrough);
        assert_eq!(route_goal(""), GoalRouting::Passthrough);
    }

    #[test]
    fn test_testlikegoal_as_str() {
        assert_eq!(TestLikeGoal::Test.as_str(), "test");
        assert_eq!(TestLikeGoal::Verify.as_str(), "verify");
        assert_eq!(TestLikeGoal::IntegrationTest.as_str(), "integration-test");
        assert_eq!(TestLikeGoal::Package.as_str(), "package");
        assert_eq!(TestLikeGoal::Install.as_str(), "install");
        assert_eq!(TestLikeGoal::Deploy.as_str(), "deploy");
        assert_eq!(
            TestLikeGoal::FailsafeIntegrationTest.as_str(),
            "failsafe:integration-test"
        );
        assert_eq!(TestLikeGoal::FailsafeVerify.as_str(), "failsafe:verify");
        assert_eq!(TestLikeGoal::SurefireTest.as_str(), "surefire:test");
    }

    #[test]
    fn test_testlikegoal_tee_slugs_are_filesystem_safe() {
        // Tee labels become filenames — plugin goals must not leak ':' into them.
        assert_eq!(TestLikeGoal::FailsafeIntegrationTest.tee_slug(), "failsafe_integration-test");
        assert_eq!(TestLikeGoal::FailsafeVerify.tee_slug(), "failsafe_verify");
        assert_eq!(TestLikeGoal::SurefireTest.tee_slug(), "surefire_test");
        assert_eq!(TestLikeGoal::Test.tee_slug(), "test");
    }

    #[test]
    fn test_compile_like_goals_have_sanitized_tee_slugs() {
        // tee_slug becomes part of a filesystem path (e.g. `mvnd_test_compile.log`),
        // so hyphens in Maven goal names must be rewritten to underscores.
        for (goal, slug) in COMPILE_LIKE_GOALS {
            assert!(
                !slug.contains('-'),
                "tee_slug for goal {goal:?} must not contain '-' (got {slug:?})"
            );
            assert!(
                !slug.contains(':'),
                "tee_slug for goal {goal:?} must not contain ':' (got {slug:?})"
            );
        }
    }

    // --- checkstyle filter tests ---

    #[test]
    fn test_filter_checkstyle_clean() {
        let input = include_str!("../../../tests/fixtures/mvn_checkstyle_clean.txt");
        let output = filter_mvn_checkstyle(input);

        // Keep success summary
        assert!(
            output.contains("0 Checkstyle violations"),
            "should keep violation-count summary, got: {}",
            output
        );
        assert!(output.contains("BUILD SUCCESS"), "should keep BUILD SUCCESS");
        assert!(output.contains("Total time"), "should keep Total time");

        // Strip ANSI escapes (fixture has them)
        assert!(
            !output.contains('\x1b'),
            "should strip ANSI escape codes"
        );

        // Strip mvnd/maven 3.9+ startup noise
        assert!(
            !output.contains("auto-discovered prefixes"),
            "should strip 'Loaded N auto-discovered prefixes' lines"
        );
        assert!(
            !output.contains("Scanning for projects"),
            "should strip 'Scanning for projects'"
        );

        // Savings ≥60%
        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);
        assert!(
            savings >= 60.0,
            "mvn checkstyle clean: expected >=60% savings, got {:.1}% ({} -> {})\nOutput:\n{}",
            savings,
            input_tokens,
            output_tokens,
            output,
        );
    }

    #[test]
    fn test_filter_checkstyle_clean_native_warnings() {
        let input =
            include_str!("../../../tests/fixtures/mvn_checkstyle_clean_native.txt");
        let output = filter_mvn_checkstyle(input);

        assert!(output.contains("0 Checkstyle violations"));
        assert!(output.contains("BUILD SUCCESS"));

        // Strip JVM restricted-method / native-access warnings (non-prefixed WARNING:)
        assert!(
            !output.contains("sun.misc.Unsafe"),
            "should strip JVM native-access warnings"
        );
        assert!(
            !output.contains("native-access"),
            "should strip --enable-native-access hints"
        );

        // Strip os-maven-plugin detection lines
        assert!(
            !output.contains("os.detected"),
            "should strip [INFO] os.detected.* lines"
        );

        let savings = 100.0
            - (count_tokens(&output) as f64 / count_tokens(input) as f64 * 100.0);
        assert!(
            savings >= 60.0,
            "mvn checkstyle clean (native): expected >=60% savings, got {:.1}%",
            savings
        );
    }

    #[test]
    fn test_filter_checkstyle_violations() {
        let input =
            include_str!("../../../tests/fixtures/mvn_checkstyle_violations.txt");
        let output = filter_mvn_checkstyle(input);

        // Keep: error-count summary
        assert!(
            output.contains("4 errors reported by Checkstyle"),
            "should keep '4 errors reported' summary, got:\n{}",
            output
        );

        // Keep: final result
        assert!(output.contains("BUILD FAILURE"));
        assert!(output.contains("Total time"));

        // Keep: each of 4 violations (rule name must survive the rewrite)
        for rule in &[
            "UnusedImports",
            "MethodName",
            "LineLength",
            "LocalVariableName",
        ] {
            assert!(
                output.contains(rule),
                "should keep violation rule {}, got:\n{}",
                rule,
                output
            );
        }

        // Strip: maven Help-link boilerplate
        assert!(
            !output.contains("To see the full stack trace"),
            "should strip 'To see the full stack trace' boilerplate"
        );
        assert!(
            !output.contains("MojoFailureException"),
            "should strip Help-link MojoFailureException reference"
        );
        assert!(
            !output.contains("Failed to execute goal org.apache.maven.plugins"),
            "should strip 'Failed to execute goal …' [ERROR] line"
        );

        // Exactly 4 rewritten violation lines (one per rule above).
        // Our compact format is `  <path>:<line>:<col> [<Rule>] <msg>`.
        let violation_count = output
            .lines()
            .filter(|l| l.contains("ExternalAppId.java") && l.contains('['))
            .count();
        assert_eq!(
            violation_count, 4,
            "expected exactly 4 violation lines, got {}:\n{}",
            violation_count, output
        );

        // Strip: mvn startup noise (fixture has 7 `auto-discovered prefixes` lines)
        assert!(!output.contains("auto-discovered prefixes"));

        // Savings ≥60%
        let savings = 100.0
            - (count_tokens(&output) as f64 / count_tokens(input) as f64 * 100.0);
        assert!(
            savings >= 60.0,
            "mvn checkstyle violations: expected >=60% savings, got {:.1}%\nOutput:\n{}",
            savings,
            output
        );
    }

    #[test]
    fn snapshot_checkstyle_violations() {
        let input = include_str!("../../../tests/fixtures/mvn_checkstyle_violations.txt");
        let output = filter_mvn_checkstyle(input);
        insta::assert_snapshot!(output);
    }

    #[test]
    fn test_filter_verify_auth_counts() {
        let input = include_str!("../../../tests/fixtures/mvn_verify_auth.txt");
        let output = filter_mvn_verify(input);
        assert!(
            output.starts_with("mvn verify:"),
            "verify filter must emit 'mvn verify:' prefix, got: {}",
            output
        );
        assert!(
            output.contains("941 passed"),
            "should accumulate surefire+failsafe (688+262)=950 run, minus 9 skipped = 941 passed, got: {}",
            output
        );
        assert!(
            output.contains("9 skipped"),
            "should accumulate skipped (8 surefire + 1 failsafe), got: {}",
            output
        );
        assert!(
            output.contains("02:11 min"),
            "should preserve total time, got: {}",
            output
        );
        assert!(
            !output.contains("BUILD FAILURE"),
            "passing verify run should not say FAILURE, got: {}",
            output
        );
    }

    #[test]
    fn test_filter_verify_auth_savings() {
        let input = include_str!("../../../tests/fixtures/mvn_verify_auth.txt");
        let output = filter_mvn_verify(input);

        let input_tokens = count_tokens(input);
        let output_tokens = count_tokens(&output);
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 90.0,
            "mvn verify auth: expected >=90% savings, got {:.1}% ({} -> {} tokens)",
            savings,
            input_tokens,
            output_tokens,
        );
    }

    /// Helper: tempdir with N copies of the real UsersTest fixture under
    /// target/surefire-reports, class names made distinct.
    fn tmp_with_reports(n: usize) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("target/surefire-reports");
        std::fs::create_dir_all(&dir).unwrap();
        let xml = include_str!(
            "../../../tests/fixtures/surefire_xml/TEST-com.example.auth.user.UsersTest.xml"
        );
        for i in 0..n {
            let renamed = xml.replace("UsersTest", &format!("Suite{i}Test"));
            std::fs::write(dir.join(format!("TEST-com.example.Suite{i}Test.xml")), renamed)
                .unwrap();
        }
        tmp
    }

    #[test]
    fn enrich_pass_small_run_inlines_classes_and_carries_digest() {
        let tmp = tmp_with_reports(2);
        let since = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let out = super::enrich_with_reports(
            "mvn test: 24 passed (3.0 s)",
            tmp.path(),
            since,
            &pkgs("com.example"),
            "test",
        );
        assert!(out.text.contains("Suite0Test:"), "inline class list, got: {}", out.text);
        assert!(out.digest.is_some(), "digest written even for inline runs");
        assert!(!out.reference, "2 classes fit inline — no reference line needed");
    }

    #[test]
    fn enrich_pass_inline_breakdown_precedes_build_success() {
        // The pass summary now ends with a maven-native trailer; the inline
        // class breakdown must slot in before it so BUILD SUCCESS stays the
        // final line (agents `tail` the output expecting the footer last).
        let tmp = tmp_with_reports(2);
        let since = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let out = super::enrich_with_reports(
            "mvn test: 24 passed (3.0 s)\nTests run: 24, Failures: 0, Errors: 0, Skipped: 0\nBUILD SUCCESS",
            tmp.path(),
            since,
            &pkgs("com.example"),
            "test",
        );
        assert!(
            out.text.contains("Suite0Test:"),
            "inline class list, got: {}",
            out.text
        );
        assert_eq!(
            out.text.lines().last(),
            Some("BUILD SUCCESS"),
            "BUILD SUCCESS must stay last, got: {}",
            out.text
        );
    }

    #[test]
    fn enrich_pass_large_run_defers_to_digest() {
        let tmp = tmp_with_reports(6);
        let since = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let out = super::enrich_with_reports(
            "mvn test: 72 passed (9.0 s)",
            tmp.path(),
            since,
            &pkgs("com.example"),
            "test",
        );
        assert_eq!(out.text, "mvn test: 72 passed (9.0 s)");
        assert!(out.reference);
        let digest = out.digest.expect("digest for large run");
        assert!(digest.contains("Suite5Test"), "all classes in digest, got: {digest}");
    }

    #[test]
    fn enrich_clean_run_without_reports_passes_through() {
        // Replaces enrich_happy_path_passes_through_without_io: the pass path now
        // performs discovery I/O, but with no reports found the summary must come
        // back byte-identical and nothing is written.
        let tmp = tempfile::tempdir().unwrap();
        let text = "mvn test: 42 passed (1.234 s)";
        let out = super::enrich_with_reports(
            text,
            tmp.path(),
            std::time::SystemTime::now(),
            &pkgs("com.example"),
            "test",
        );
        assert_eq!(out.text, text);
        assert_eq!(out.digest, None);
    }

    #[test]
    fn enrich_pass_with_skipped_count_in_summary_still_enriches() {
        // "N passed, K skipped (t)" summaries used to bypass the old
        // `looks_clean` substring check; the new pass gate must catch them.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("target/surefire-reports");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("TEST-com.example.MicrosoftEntraIdClient2Test.xml"),
            include_str!(
                "../../../tests/fixtures/surefire_xml/TEST-com.example.auth.partners.entraid.MicrosoftEntraIdClient2Test.xml"
            ),
        )
        .unwrap();
        let since = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let out = super::enrich_with_reports(
            "mvn test: 5 passed, 8 skipped (2.0 s)",
            tmp.path(),
            since,
            &pkgs("com.example"),
            "test",
        );
        assert!(out.digest.is_some());
        let digest = out.digest.unwrap();
        assert!(digest.contains("skipped:"), "skipped names in digest, got: {digest}");
        assert!(out.reference, "8 skipped > inline cap");
    }

    #[test]
    fn enrich_no_tests_with_no_reports_emits_red_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let text = "mvn test: No tests run";
        let out = super::enrich_with_reports(
            text,
            tmp.path(),
            std::time::SystemTime::now(),
            &pkgs("com.example"),
            "test",
        );
        assert!(out.text.contains("0 tests executed"));
        assert!(out.text.contains("surefire detected"));
    }

    #[test]
    fn enrich_no_tests_for_verify_goal_uses_verify_in_message() {
        let tmp = tempfile::tempdir().unwrap();
        let text = "mvn verify: No tests run";
        let out = super::enrich_with_reports(
            text,
            tmp.path(),
            std::time::SystemTime::now(),
            &pkgs("com.example"),
            "verify",
        );
        assert!(
            out.text.contains("0 tests executed"),
            "zero-tests branch must fire for verify, got: {}",
            out.text
        );
        assert!(
            out.text.contains("mvn verify"),
            "error message must reference the verify goal, got: {}",
            out.text
        );
    }

    #[test]
    fn snapshot_verify_auth() {
        let input = include_str!("../../../tests/fixtures/mvn_verify_auth.txt");
        let output = filter_mvn_verify(input);
        insta::assert_snapshot!(output);
    }

    #[test]
    fn test_filter_mvn_test_still_emits_test_prefix() {
        let input = include_str!("../../../tests/fixtures/mvn_test_pass_mavenmcp.txt");
        let output = filter_mvn_test(input);
        assert!(
            output.starts_with("mvn test:"),
            "test filter must keep 'mvn test:' prefix after goal parameterization, got: {}",
            output
        );
    }

    #[test]
    fn shorten_exception_header_strips_fqn_package() {
        // Standard Java exception header: `org.junit.ComparisonFailure: msg`
        assert_eq!(
            super::shorten_exception_header(
                "org.junit.ComparisonFailure: expected:<He[llo]!> but was:<He[re I am]!>",
            ),
            "ComparisonFailure: expected:<He[llo]!> but was:<He[re I am]!>",
        );
        assert_eq!(
            super::shorten_exception_header("java.lang.NullPointerException: x"),
            "NullPointerException: x",
        );
    }

    #[test]
    fn shorten_exception_header_passthrough_for_non_fqn() {
        // Messages without a package-qualified prefix stay untouched.
        assert_eq!(
            super::shorten_exception_header("expected:<200> but was:<404>"),
            "expected:<200> but was:<404>",
        );
        // Simple class name (no dots) — passthrough.
        assert_eq!(
            super::shorten_exception_header("AssertionError: boom"),
            "AssertionError: boom",
        );
        // FQN token with whitespace in it — not a class, passthrough.
        assert_eq!(
            super::shorten_exception_header("not fqn: value"),
            "not fqn: value",
        );
    }

    #[test]
    fn text_filter_shortens_exception_fqn_in_first_detail() {
        // Regression: before unification, stdout parser emitted the full FQN
        // (`org.junit.ComparisonFailure:`). XML path only shows the short
        // class name — text fallback must match so both sources render
        // identically.
        let input = include_str!("../../../tests/fixtures/mvn_test_reactor_fail.txt");
        let output = filter_mvn_test(input);
        assert!(
            !output.contains("org.junit.ComparisonFailure:"),
            "text filter leaked FQN — must render short `ComparisonFailure:`:\n{output}"
        );
    }

    #[test]
    fn text_filter_drops_non_app_frames_when_app_packages_known() {
        // With `app_packages=["com.edeal.frontline"]`, `org.junit.Assert.*`
        // frames are not app code and must be dropped — same rule the XML
        // `stack_trace::process` path applies. Without this, the stdout
        // fallback kept `at org.junit.Assert.assertEquals(Assert.java:117)`
        // noise while XML output was clean.
        let input = include_str!("../../../tests/fixtures/mvn_test_reactor_fail.txt");
        let output =
            super::filter_mvn_tests_with_goal(input, "test", &pkgs("com.edeal.frontline"));
        assert!(
            !output.contains("org.junit.Assert.assertEquals"),
            "kept `org.junit.Assert` framework frame with app_packages known:\n{output}"
        );
    }

    #[test]
    fn text_filter_preserves_existing_behavior_without_app_packages() {
        // Empty app_packages falls back to the legacy whitelist — fixtures
        // that today rely on certain frames surviving must keep working.
        let input = include_str!("../../../tests/fixtures/mvn_test_reactor_fail.txt");
        let with_empty = filter_mvn_test(input); // app_packages = &[]
        let with_pkgs =
            super::filter_mvn_tests_with_goal(input, "test", &pkgs("com.edeal.frontline"));
        // Empty-packages mode keeps frames the legacy whitelist doesn't cover.
        assert!(
            with_empty.contains("org.junit.Assert.assertEquals"),
            "empty app_packages must NOT regress the legacy behavior:\n{with_empty}"
        );
        // App-packages mode drops them (covered above); sanity-check divergence.
        assert_ne!(with_empty, with_pkgs);
    }

    #[test]
    fn enrich_drops_text_failures_block_when_xml_has_failures() {
        // Regression: before deduplication the user saw two "Failures"
        // blocks — one from stdout parsing, one from surefire XML —
        // listing the same test. XML is authoritative, so the text block
        // must be stripped whenever XML failures exist.
        let tmp = tempfile::tempdir().unwrap();
        let reports_dir = tmp.path().join("target/surefire-reports");
        std::fs::create_dir_all(&reports_dir).unwrap();
        std::fs::copy(
            "tests/fixtures/java/surefire-reports/TEST-com.example.FailingTest.xml",
            reports_dir.join("TEST-com.example.FailingTest.xml"),
        )
        .unwrap();

        let since = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let text = "mvn test: 2 run, 2 failed (0.6 s)\nBUILD FAILURE\n\nFailures:\n\
                    1. com.example.FailingTest.shouldReturnUser\n\
                       AssertionFailedError: expected:<200> but was:<404>\n";
        let out =
            super::enrich_with_reports(text, tmp.path(), since, &pkgs("com.example"), "test");

        // XML block present.
        assert!(
            out.text.contains("Failures (from surefire-reports/)"),
            "missing XML failures section:\n{}",
            out.text
        );
        // Text block gone — only the XML variant remains.
        assert!(
            !out.text.contains("\nFailures:\n"),
            "text-filter 'Failures:' block leaked through — duplicate:\n{}",
            out.text
        );
        // Summary + BUILD FAILURE preserved.
        assert!(out.text.starts_with("mvn test: 2 run, 2 failed (0.6 s)\nBUILD FAILURE"));
    }

    #[test]
    fn enrich_keeps_text_failures_block_when_xml_unavailable() {
        // Fallback guarantee: if XML reports are missing, the text-filter
        // block is the only source of failure info — must survive.
        let tmp = tempfile::tempdir().unwrap();
        let since = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let text = "mvn test: 1 run, 1 failed (0.5 s)\nBUILD FAILURE\n\nFailures:\n\
                    1. com.example.LostTest.boom\n";
        let out =
            super::enrich_with_reports(text, tmp.path(), since, &pkgs("com.example"), "test");

        assert!(
            out.text.contains("Failures:\n1. com.example.LostTest.boom"),
            "fallback dropped text failures when XML was absent:\n{}",
            out.text
        );
        assert!(
            out.text.contains("no XML reports found"),
            "expected no-reports hint in fallback:\n{}",
            out.text
        );
    }

    #[test]
    fn enrich_with_surefire_fixture_appends_failures_section() {
        let tmp = tempfile::tempdir().unwrap();
        let reports_dir = tmp.path().join("target/surefire-reports");
        std::fs::create_dir_all(&reports_dir).unwrap();
        std::fs::copy(
            "tests/fixtures/java/surefire-reports/TEST-com.example.FailingTest.xml",
            reports_dir.join("TEST-com.example.FailingTest.xml"),
        )
        .unwrap();

        let since = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let text = "mvn test: 4 run, 2 failed (01:02 min)\nBUILD FAILURE";
        let out = super::enrich_with_reports(text, tmp.path(), since, &pkgs("com.example"), "test");

        assert!(out.text.contains("Failures (from surefire-reports/)"));
        assert!(out.text.contains("com.example.FailingTest.shouldReturnUser"));
        assert!(out.text.contains("reports:"));
    }

    #[test]
    fn enrich_with_both_report_dirs_appends_both_sections() {
        let tmp = tempfile::tempdir().unwrap();
        let sf = tmp.path().join("target/surefire-reports");
        let fs = tmp.path().join("target/failsafe-reports");
        std::fs::create_dir_all(&sf).unwrap();
        std::fs::create_dir_all(&fs).unwrap();
        std::fs::copy(
            "tests/fixtures/java/surefire-reports/TEST-com.example.FailingTest.xml",
            sf.join("TEST-com.example.FailingTest.xml"),
        )
        .unwrap();
        std::fs::copy(
            "tests/fixtures/java/failsafe-reports/TEST-com.example.DbIntegrationIT.xml",
            fs.join("TEST-com.example.DbIntegrationIT.xml"),
        )
        .unwrap();

        let since = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let text = "mvn verify: 10 run, 3 failed (03:30 min)\nBUILD FAILURE";
        let out = super::enrich_with_reports(text, tmp.path(), since, &pkgs("com.example"), "verify");
        assert!(out.text.contains("Failures (from surefire-reports/)"));
        assert!(out.text.contains("Integration failures (from failsafe-reports/)"));
        assert!(out.text.contains("Caused by: org.hibernate.HibernateException"));

        // The digest must combine both report dirs, not just one.
        let digest = out.digest.as_ref().expect("digest for combined report dirs");
        assert!(digest.contains("FailingTest"), "missing surefire class: {digest}");
        assert!(digest.contains("DbIntegrationIT"), "missing failsafe class: {digest}");
    }

    #[test]
    fn enrich_reactor_finds_per_module_reports() {
        // Regression: in reactor builds (and `mvn -pl <module>` from the
        // root), Surefire writes reports under each module's `target/`,
        // not under the cwd. The enricher must walk depth-1 module dirs
        // so failure details still surface — otherwise the user gets
        // "no XML reports found" despite fresh reports existing.
        let tmp = tempfile::tempdir().unwrap();

        // Module A has the failing test (under <cwd>/module-a/target/...)
        let mod_a_sf = tmp.path().join("module-a/target/surefire-reports");
        std::fs::create_dir_all(&mod_a_sf).unwrap();
        std::fs::copy(
            "tests/fixtures/java/surefire-reports/TEST-com.example.FailingTest.xml",
            mod_a_sf.join("TEST-com.example.FailingTest.xml"),
        )
        .unwrap();

        // Module B has a passing suite (under <cwd>/module-b/target/...)
        let mod_b_sf = tmp.path().join("module-b/target/surefire-reports");
        std::fs::create_dir_all(&mod_b_sf).unwrap();
        std::fs::copy(
            "tests/fixtures/java/surefire-reports/TEST-com.example.PassingTest.xml",
            mod_b_sf.join("TEST-com.example.PassingTest.xml"),
        )
        .unwrap();

        // Module C has integration failure under failsafe-reports
        let mod_c_fs = tmp.path().join("module-c/target/failsafe-reports");
        std::fs::create_dir_all(&mod_c_fs).unwrap();
        std::fs::copy(
            "tests/fixtures/java/failsafe-reports/TEST-com.example.DbIntegrationIT.xml",
            mod_c_fs.join("TEST-com.example.DbIntegrationIT.xml"),
        )
        .unwrap();

        // No reports at <cwd>/target/ — the cwd-only check would miss everything.
        assert!(!tmp.path().join("target").exists());

        let since = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let text = "mvn verify: 14 run, 3 failed (02:15 min)\nBUILD FAILURE";
        let out = super::enrich_with_reports(
            text,
            tmp.path(),
            since,
            &pkgs("com.example"),
            "verify",
        );

        // Failure details from module-a's surefire reports must surface.
        assert!(
            out.text.contains("Failures (from surefire-reports/)"),
            "missed module-a surefire reports:\n{}",
            out.text
        );
        assert!(
            out.text.contains("com.example.FailingTest.shouldReturnUser"),
            "missed FailingTest details:\n{}",
            out.text
        );

        // Integration failure from module-c must also surface.
        assert!(
            out.text.contains("Integration failures (from failsafe-reports/)"),
            "missed module-c failsafe reports:\n{}",
            out.text
        );

        // Negative: must NOT regress to the no-reports hint.
        assert!(
            !out.text.contains("no XML reports"),
            "discovered per-module reports yet still emitted no-reports hint:\n{}",
            out.text
        );
    }

    #[test]
    fn collect_reports_attaches_module_from_dir_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let xml = include_str!(
            "../../../tests/fixtures/surefire_xml/TEST-com.example.auth.user.UsersTest.xml"
        );
        let root_dir = tmp.path().join("target/surefire-reports");
        let mod_dir = tmp.path().join("services/target/surefire-reports");
        std::fs::create_dir_all(&root_dir).unwrap();
        std::fs::create_dir_all(&mod_dir).unwrap();
        std::fs::write(root_dir.join("TEST-com.example.A.xml"), xml).unwrap();
        std::fs::write(mod_dir.join("TEST-com.example.B.xml"), xml).unwrap();

        let since = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let r = super::collect_reports(
            &[root_dir, mod_dir],
            since,
            &[],
            tmp.path(),
        )
        .expect("reports must parse");
        let modules: Vec<Option<String>> =
            r.suites.iter().map(|s| s.module.clone()).collect();
        assert_eq!(modules, vec![None, Some("services".to_string())]);
    }

    #[test]
    fn enrich_reactor_real_world_multi_module() {
        // Real reactor build (anonymized from a public 4-module example):
        // module1's `SpeakerTest.speak` fails, module2 is SKIPPED.
        // Surefire writes per-module reports under
        // `<module>/target/surefire-reports/`. End-to-end check: filter
        // the raw log, then enrich against a tmpdir mimicking the real
        // on-disk layout.
        let raw = include_str!("../../../tests/fixtures/mvn_test_reactor_module_failure.txt");
        let text = filter_mvn_test(raw);

        // Sanity: filter must already report failure (Reactor Summary's
        // module-level FAILURE drives the parser even before XML enrichment).
        assert!(
            text.contains("BUILD FAILURE") || text.contains("failed"),
            "filter dropped failure signal:\n{text}"
        );

        let tmp = tempfile::tempdir().unwrap();
        // Mirror the real layout: <cwd>/<module>/target/surefire-reports/
        let m1 = tmp.path().join("module1/target/surefire-reports");
        let m2 = tmp.path().join("module2/target/surefire-reports");
        std::fs::create_dir_all(&m1).unwrap();
        std::fs::create_dir_all(&m2).unwrap();
        std::fs::copy(
            "tests/fixtures/java/surefire-reports-modules/module1/TEST-com.example.app.SpeakerTest.xml",
            m1.join("TEST-com.example.app.SpeakerTest.xml"),
        )
        .unwrap();
        std::fs::copy(
            "tests/fixtures/java/surefire-reports-modules/module2/TEST-com.example.app.AppTest.xml",
            m2.join("TEST-com.example.app.AppTest.xml"),
        )
        .unwrap();

        // Reports were just written — `since` must be older than the copy.
        let since = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let out =
            super::enrich_with_reports(&text, tmp.path(), since, &pkgs("com.example.app"), "test");

        // Failure details must surface from module1's report (not from cwd/target).
        assert!(
            out.text.contains("com.example.app.SpeakerTest.speak"),
            "missed real-world per-module failure surfacing:\n{}",
            out.text
        );
        assert!(
            out.text.contains("expected") || out.text.contains("ComparisonFailure"),
            "missed real failure message:\n{}",
            out.text
        );
        // No fallback hint — reports were found.
        assert!(
            !out.text.contains("no XML reports"),
            "discovered per-module reports yet still emitted no-reports hint:\n{}",
            out.text
        );
    }

    #[test]
    fn enrich_reactor_skips_dot_dirs_and_node_modules() {
        // Walker must skip noisy, expensive dirs commonly found alongside
        // pom.xml (`.git`, `node_modules`, `.idea`, etc.) so the depth-1
        // walk stays cheap.
        let tmp = tempfile::tempdir().unwrap();
        for skipped in [".git", ".idea", "node_modules", "target", "src"] {
            std::fs::create_dir_all(tmp.path().join(skipped).join("target/surefire-reports"))
                .unwrap();
            // Drop a malformed XML to prove the walker did NOT recurse here.
            std::fs::write(
                tmp.path()
                    .join(skipped)
                    .join("target/surefire-reports/TEST-Bogus.xml"),
                "<broken/>",
            )
            .unwrap();
        }
        let text = "mvn test: 5 run, 1 failed (0.5 s)\nBUILD FAILURE";
        let out = super::enrich_with_reports(
            text,
            tmp.path(),
            std::time::SystemTime::now() - std::time::Duration::from_secs(60),
            &pkgs("com.example"),
            "test",
        );
        // Should fall through to the no-reports hint — none of the skipped
        // dirs counted as a module.
        assert!(
            out.text.contains("no XML reports"),
            "walker recursed into a skipped dir:\n{}",
            out.text
        );
    }

    #[test]
    fn enrich_failures_without_xml_appends_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let text = "mvn test: 5 run, 2 failed (0.500 s)\nBUILD FAILURE";
        let out = super::enrich_with_reports(
            text,
            tmp.path(),
            std::time::SystemTime::now(),
            &pkgs("com.example"),
            "test",
        );
        assert!(out.text.contains("no XML reports"));
        assert!(out.text.contains("check target/surefire-reports/"));
    }

    #[test]
    fn enrich_happy_path_with_10_passed_is_short_circuited() {
        // Regression: "10 passed" must not trigger zero_tests via substring of "0 passed".
        let tmp = tempfile::tempdir().unwrap();
        let text = "mvn test: 10 passed (0.500 s)";
        let out = super::enrich_with_reports(
            text,
            tmp.path(),
            std::time::SystemTime::now(),
            &pkgs("com.example"),
            "test",
        );
        assert_eq!(out.text, text, "10 passed must short-circuit without enrichment");
    }

    #[test]
    fn snapshot_enriched_surefire_only() {
        let tmp = tempfile::tempdir().unwrap();
        let reports = tmp.path().join("target/surefire-reports");
        std::fs::create_dir_all(&reports).unwrap();
        for name in [
            "TEST-com.example.FailingTest.xml",
            "TEST-com.example.PassingTest.xml",
        ] {
            std::fs::copy(
                format!("tests/fixtures/java/surefire-reports/{name}"),
                reports.join(name),
            )
            .unwrap();
        }

        let since = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let text = "mvn test: 7 run, 2 failed (00:10 min)\nBUILD FAILURE";
        let out = super::enrich_with_reports(text, tmp.path(), since, &pkgs("com.example"), "test");
        insta::assert_snapshot!(out.text);
    }

    #[test]
    fn snapshot_enriched_surefire_and_failsafe() {
        let tmp = tempfile::tempdir().unwrap();
        let sf = tmp.path().join("target/surefire-reports");
        let fs = tmp.path().join("target/failsafe-reports");
        std::fs::create_dir_all(&sf).unwrap();
        std::fs::create_dir_all(&fs).unwrap();
        std::fs::copy(
            "tests/fixtures/java/surefire-reports/TEST-com.example.FailingTest.xml",
            sf.join("TEST-com.example.FailingTest.xml"),
        )
        .unwrap();
        std::fs::copy(
            "tests/fixtures/java/failsafe-reports/TEST-com.example.DbIntegrationIT.xml",
            fs.join("TEST-com.example.DbIntegrationIT.xml"),
        )
        .unwrap();
        std::fs::copy(
            "tests/fixtures/java/failsafe-reports/TEST-com.example.PortConflictIT.xml",
            fs.join("TEST-com.example.PortConflictIT.xml"),
        )
        .unwrap();

        let since = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let text = "mvn verify: 12 run, 4 failed (05:42 min)\nBUILD FAILURE";
        let out = super::enrich_with_reports(text, tmp.path(), since, &pkgs("com.example"), "verify");
        insta::assert_snapshot!(out.text);
    }

    #[test]
    fn snapshot_red_flag_no_tests() {
        let tmp = tempfile::tempdir().unwrap();
        let out = super::enrich_with_reports(
            "mvn test: No tests run",
            tmp.path(),
            std::time::SystemTime::now(),
            &pkgs("com.example"),
            "test",
        );
        insta::assert_snapshot!(out.text);
    }

    #[test]
    fn savings_happy_path_unchanged_by_enrichment() {
        // Happy path: with no reports discovered under `tmp`, the pass gate's
        // digest-is-none check falls back to the summary unchanged — savings
        // must match pre-enrichment.
        let text = "mvn test: 859 passed, 4 skipped (02:11 min)";
        let tmp = tempfile::tempdir().unwrap();
        let out = super::enrich_with_reports(
            text,
            tmp.path(),
            std::time::SystemTime::now(),
            &pkgs("com.example"),
            "test",
        );
        assert_eq!(out.text, text, "happy path must not allocate or append");
    }

    #[test]
    fn savings_enriched_failures_stays_under_15_percent() {
        // Simulate a ~2000-line build log whose text filter produced a short
        // summary, plus one big failsafe XML with system-err and a 3-segment
        // Caused-by chain. Total enriched output must be ≥85% smaller than raw.
        let raw_log: String = std::iter::repeat_n(
            "[INFO] Running com.example.some.Heavy.Test — lots of noisy build output\n",
            2000,
        )
        .collect::<String>();

        let tmp = tempfile::tempdir().unwrap();
        let fs = tmp.path().join("target/failsafe-reports");
        std::fs::create_dir_all(&fs).unwrap();
        std::fs::copy(
            "tests/fixtures/java/failsafe-reports/TEST-com.example.DbIntegrationIT.xml",
            fs.join("TEST-com.example.DbIntegrationIT.xml"),
        )
        .unwrap();

        let since = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let text_summary = "mvn verify: 4 run, 1 failed (01:23 min)\nBUILD FAILURE";
        let enriched = super::enrich_with_reports(text_summary, tmp.path(), since, &pkgs("com.example"), "verify");

        let raw_tokens = count_tokens(&raw_log);
        let enriched_tokens = count_tokens(&enriched.text);
        let savings = 100.0 - (enriched_tokens as f64 / raw_tokens as f64 * 100.0);
        assert!(
            savings >= 85.0,
            "expected ≥85% savings on enriched failure path, got {savings:.1}% \
             (raw={raw_tokens}, enriched={enriched_tokens})"
        );
    }

    // --- pgp + multimodule + JVM banner/warning stripping ---
    // Maven environment banner (`mvn -V`), JVM 21+ restricted-method WARNINGs,
    // SLF4J init noise, pgpverify-maven-plugin chatter, resources-plugin copy
    // lines, clean-audit checkstyle output, and mvn 3.9.x Reactor Build Order
    // `<name> <version>` format without `[pom|jar]` suffix. Originally captured
    // from rtk-ai/rtk#1241.

    #[test]
    fn test_compile_pgp_multimodule_savings() {
        let input = include_str!("../../../tests/fixtures/mvn_compile_pgp_multimodule.txt");
        let output = filter_mvn_compile(input);
        let in_tok = count_tokens(input);
        let out_tok = count_tokens(&output);
        let savings = 100.0 - (out_tok as f64 / in_tok as f64 * 100.0);
        assert!(
            savings >= 85.0,
            "expected ≥85% savings on pgp+multimodule compile success, got {savings:.1}% \
             (in={in_tok}, out={out_tok})\n--- OUTPUT ---\n{output}"
        );
    }

    #[test]
    fn test_compile_pgp_strips_banner_and_jvm_warnings() {
        let input = include_str!("../../../tests/fixtures/mvn_compile_pgp_multimodule.txt");
        let output = filter_mvn_compile(input);
        // Environment banner from `mvn -V`
        assert!(!output.contains("Apache Maven 3.9.6"), "kept Maven banner: {output}");
        assert!(!output.contains("Java version:"), "kept Java version banner: {output}");
        assert!(!output.contains("OS name:"), "kept OS banner: {output}");
        // JVM 21+ restricted-method warnings
        assert!(!output.contains("restricted method"), "kept JVM restricted-method WARNING: {output}");
        assert!(!output.contains("SLF4J:"), "kept SLF4J noise: {output}");
        // pgpverify-maven-plugin chatter
        assert!(!output.contains("Verifying com.google.guava"), "kept pgp Verifying: {output}");
        assert!(!output.contains("Key server(s)"), "kept pgp Key server line: {output}");
        // maven-resources-plugin noise
        assert!(!output.contains("encoding to copy filtered"), "kept resources encoding line: {output}");
        assert!(!output.contains("skip non existing resourceDirectory"), "kept skip resourceDirectory: {output}");
        // clean-audit checkstyle pass
        assert!(!output.contains("Audit done"), "kept Audit done: {output}");
        assert!(!output.contains("Checkstyle violations"), "kept checkstyle 0-violations: {output}");
        // Reactor Build Order modules (mvn 3.9.x `<name> <version>` format)
        assert!(!output.contains("parent-project 2.4.1-SNAPSHOT"), "kept Reactor Build Order entry: {output}");
        // Must preserve the essentials
        assert!(output.contains("BUILD SUCCESS"));
        assert!(output.contains("Total time"));
    }

    #[test]
    fn test_failure_stack_does_not_bleed_into_next_test() {
        let input = include_str!("../../../tests/fixtures/mvn_test_failure_stack_isolation.txt");
        let output = filter_mvn_test(input);
        // The next-class Running marker must NOT appear inside the failure
        // stack block (cosmetic bleed observed in diag).
        assert!(
            !output.contains("Running com.example.repository.UserRepositoryTest"),
            "failure stack bled into next test's Running marker:\n{output}"
        );
        // Sanity: we still have the real failure details.
        assert!(output.contains("UserServiceTest.testCreateUser_DuplicateEmail"));
        assert!(output.contains("AssertionError"));
    }

    #[test]
    fn test_artifactregistry_and_gcp_auth_are_stripped() {
        let input = include_str!("../../../tests/fixtures/mvn_compile_artifactregistry.txt");
        let output = filter_mvn_compile(input);
        // `artifactregistry-maven-wagon` emits ~20 copies of
        // "Artifact X:Y:Z is present in the local repository, but cached
        // from a remote repository ID that is unavailable in current build
        // context…" — non-actionable, must collapse.
        assert!(
            !output.contains("is present in the local repository, but cached"),
            "kept artifactregistry 'is present … cached from' chatter:\n{output}"
        );
        // GCP auth startup chatter
        assert!(!output.contains("Initializing Credentials"));
        assert!(!output.contains("Application Default Credentials"));
        assert!(!output.contains("Refreshing Credentials"));
        // End-of-build JUL-format Google auth warning
        assert!(
            !output.contains("warnAboutProblematicCredentials"),
            "kept Google auth JUL warning header:\n{output}"
        );
        assert!(
            !output.contains("Your application has authenticated using end user credentials"),
            "kept Google auth JUL warning body:\n{output}"
        );
        // Sanity: the real compile errors must be preserved.
        assert!(output.contains("COMPILATION ERROR"));
        assert!(output.contains("BUILD FAILURE"));
    }

    #[test]
    fn test_plugin_boilerplate_is_stripped() {
        // maven-enforcer per-rule `passed` lines, githook plugin hook
        // install chatter, and maven-compiler `Changes detected` trivia
        // are plugin wiring noise that the user never acts on.
        let input = include_str!("../../../tests/fixtures/mvn_compile_artifactregistry.txt");
        let output = filter_mvn_compile(input);
        assert!(
            !output.contains("RequireMavenVersion passed"),
            "kept enforcer 'Rule N: …passed' line:\n{output}"
        );
        assert!(
            !output.contains("Installing commit-msg hook"),
            "kept githook plugin install line:\n{output}"
        );
        assert!(
            !output.contains("Changes detected - recompiling"),
            "kept compiler-plugin 'Changes detected' trivia:\n{output}"
        );
        // Real errors must still be there.
        assert!(output.contains("COMPILATION ERROR"));
        assert!(output.contains("BUILD FAILURE"));
    }

    #[test]
    fn test_artifactregistry_fixture_savings() {
        let input = include_str!("../../../tests/fixtures/mvn_compile_artifactregistry.txt");
        let output = filter_mvn_compile(input);
        let in_tok = count_tokens(input);
        let out_tok = count_tokens(&output);
        let savings = 100.0 - (out_tok as f64 / in_tok as f64 * 100.0);
        assert!(
            savings >= 80.0,
            "artifactregistry compile-failure fixture: expected ≥80% savings, got {savings:.1}% \
             (in={in_tok}, out={out_tok})"
        );
    }

    #[test]
    fn test_mvn_test_compile_failure_surfaces_errors() {
        // Running `mvn test` on a project that fails to compile must NOT
        // return the cheerful "no tests run" line — users would miss the
        // actual compile errors. Fall back to the compile filter so the
        // error block reaches the user.
        let input = include_str!("../../../tests/fixtures/mvn_test_compile_failure.txt");
        let output = filter_mvn_test(input);
        assert!(
            !output.trim().ends_with("No tests run")
                && output.len() > "mvn test: No tests run".len(),
            "mvn test hid compile errors with 'no tests run':\n{output}"
        );
        // Must expose at least one real compile error.
        assert!(
            output.contains("COMPILATION ERROR") || output.contains("cannot find symbol"),
            "mvn test output missing compile-error signal:\n{output}"
        );
        assert!(output.contains("BUILD FAILURE"));
    }

    #[test]
    fn test_forked_vm_crash_never_emits_synthetic_pass() {
        // Surefire forked-VM crashes (System.exit, OOM kill, JVM segfault,
        // plugin timeout) emit `BUILD FAILURE` *without* a `Results:`
        // block. The state-machine parser leaves Testing with cumulative
        // counts at zero — without the BUILD FAILURE guard the success
        // branch would emit "0 passed", silently hiding a hard failure.
        // Must surface the actual error block via the compile-filter
        // fallback.
        let input = include_str!("../../../tests/fixtures/mvn_test_forked_vm_crash.txt");
        let output = filter_mvn_test(input);

        // Must NOT emit a passing summary.
        assert!(
            !output.contains("0 passed"),
            "forked-VM crash reported as 0 passed:\n{output}"
        );
        assert!(
            !output.contains("mvn test: 0 passed"),
            "forked-VM crash reported as synthetic pass:\n{output}"
        );

        // Must surface the hard failure.
        assert!(
            output.contains("BUILD FAILURE"),
            "BUILD FAILURE missing from forked-VM crash output:\n{output}"
        );
        assert!(
            output.contains("forked VM terminated") || output.contains("Crashed tests"),
            "forked-VM error signal missing:\n{output}"
        );
    }

    #[test]
    fn test_resources_plugin_encoding_advisory_is_stripped() {
        // maven-resources-plugin emits a ~100-word `[INFO]` advisory when
        // it encounters `.properties` files without an explicit filtering
        // encoding set. Pure documentation-pointer noise on success.
        let input = include_str!("../../../tests/fixtures/mvn_resources_encoding_warning.txt");
        let output = filter_mvn_compile(input);
        assert!(
            !output.contains("encoding used to copy"),
            "kept resources-plugin encoding advisory:\n{output}"
        );
        assert!(output.contains("BUILD SUCCESS"));
        assert!(output.contains("Total time"));
    }

    #[test]
    fn test_split_segments_classifies() {
        let raw = "\
[INFO] Scanning for projects...
[INFO] --- maven-clean-plugin:3.2.0:clean (default-clean) @ app ---
[INFO] Deleting /app/target
[INFO] --- maven-compiler-plugin:3.13.0:testCompile (default-testCompile) @ app ---
[INFO] Compiling 12 source files
[INFO] --- maven-checkstyle-plugin:3.6.0:check (default-cli) @ app ---
[INFO] You have 0 Checkstyle violations.
[INFO] BUILD SUCCESS
[INFO] Total time:  3.0 s";
        let segs = split_segments(raw);
        let kinds: Vec<SegmentKind> = segs.iter().map(|s| s.kind).collect();
        assert_eq!(kinds, vec![
            SegmentKind::Preamble,
            SegmentKind::Clean,
            SegmentKind::Compile,
            SegmentKind::Checkstyle,
        ]);
        // The checkstyle segment carries its body up to (not including) the BUILD block.
        assert!(segs[3].body.contains("0 Checkstyle violations"));
    }

    #[test]
    fn test_filter_mvn_multi_success() {
        let input = include_str!("../../../tests/fixtures/mvn_multi_clean_testcompile_checkstyle_pass.txt");
        let output = filter_mvn_multi(input, "clean test-compile checkstyle:check");
        assert!(output.contains("(multi-goal)"), "missing header: {output}");
        assert!(output.contains("BUILD SUCCESS"), "lost BUILD line: {output}");
        assert!(output.contains("0 Checkstyle violations") || output.contains("0 violations"),
                "lost checkstyle signal: {output}");
        // clean noise must be gone
        assert!(!output.contains("Deleting"), "clean noise leaked: {output}");
        let savings = 100.0 - (count_tokens(&output) as f64 / count_tokens(input) as f64 * 100.0);
        assert!(savings >= 85.0, "expected ≥85%, got {:.1}%", savings);
        insta::assert_snapshot!(output);
    }

    #[test]
    fn test_filter_mvn_multi_compile_failure() {
        let input = include_str!("../../../tests/fixtures/mvn_multi_compile_failure.txt");
        let output = filter_mvn_multi(input, "clean test-compile checkstyle:check");
        assert!(output.contains("BUILD FAILURE"), "lost failure signal: {output}");
        assert!(output.contains("cannot find symbol"), "lost compile error detail: {output}");
        let savings = 100.0 - (count_tokens(&output) as f64 / count_tokens(input) as f64 * 100.0);
        assert!(savings >= 60.0, "failure path still expected ≥60%, got {:.1}%", savings);
        insta::assert_snapshot!(output);
    }

    #[test]
    fn test_strip_quiet_flags() {
        let v = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(strip_quiet_flags(&v("clean verify -q")), v("clean verify"));
        assert_eq!(strip_quiet_flags(&v("--quiet clean test")), v("clean test"));
        assert_eq!(strip_quiet_flags(&v("clean test -Dq=1")), v("clean test -Dq=1"));
    }

    #[test]
    fn test_filtered_goal_args() {
        let v = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        // Drops the matched goal token AND strips -q so the filter sees full output.
        assert_eq!(filtered_goal_args(&v("-q test -DskipTests"), "test"), v("-DskipTests"));
        assert_eq!(filtered_goal_args(&v("--quiet install -Pprod"), "install"), v("-Pprod"));
        // Only the first goal token is dropped; -q removed even in tail position.
        assert_eq!(filtered_goal_args(&v("verify -q"), "verify"), Vec::<String>::new());
        // -Dq=1 is not a quiet flag — kept.
        assert_eq!(filtered_goal_args(&v("package -Dq=1"), "package"), v("-Dq=1"));
    }

    #[test]
    fn test_classify_marker_both_forms() {
        // Full artifact-id form
        assert_eq!(classify_marker("maven-clean-plugin"), SegmentKind::Clean);
        assert_eq!(classify_marker("maven-compiler-plugin"), SegmentKind::Compile);
        assert_eq!(classify_marker("maven-surefire-plugin"), SegmentKind::Surefire);
        assert_eq!(classify_marker("maven-failsafe-plugin"), SegmentKind::Failsafe);
        assert_eq!(classify_marker("maven-checkstyle-plugin"), SegmentKind::Checkstyle);
        // Short goal-prefix form (as seen in real logs)
        assert_eq!(classify_marker("surefire"), SegmentKind::Surefire);
        assert_eq!(classify_marker("failsafe"), SegmentKind::Failsafe);
        assert_eq!(classify_marker("checkstyle"), SegmentKind::Checkstyle);
        assert_eq!(classify_marker("clean"), SegmentKind::Clean);
        // Unrelated plugins → Other
        assert_eq!(classify_marker("maven-resources-plugin"), SegmentKind::Other);
        assert_eq!(classify_marker("spring-boot"), SegmentKind::Other);
        assert_eq!(classify_marker("maven-jar-plugin"), SegmentKind::Other);
    }

    #[test]
    fn test_filter_mvn_multi_verify_failure_stdout() {
        let input = include_str!("../../../tests/fixtures/mvn_multi_clean_verify_fail.txt");
        let output = filter_mvn_multi(input, "clean verify");
        assert!(output.contains("BUILD FAILURE"), "lost build failure: {output}");
        assert!(output.contains("UserProvisioningIT") || output.contains("failed"),
                "lost IT failure signal: {output}");
        let savings = 100.0 - (count_tokens(&output) as f64 / count_tokens(input) as f64 * 100.0);
        assert!(savings >= 80.0, "expected ≥80%, got {:.1}%", savings);
        insta::assert_snapshot!(output);
    }

    // --- Pure renderers for pass-run enrichment (Task 4) ---

    fn parsed_fixture(xml: &str) -> super::surefire_reports::SurefireResult {
        super::surefire_reports::parse_content(xml, &[]).expect("fixture must parse")
    }

    #[test]
    fn digest_snapshot_from_real_fixtures() {
        let mut sf = parsed_fixture(include_str!(
            "../../../tests/fixtures/surefire_xml/TEST-com.example.auth.user.UsersTest.xml"
        ));
        let entra = parsed_fixture(include_str!(
            "../../../tests/fixtures/surefire_xml/TEST-com.example.auth.partners.entraid.MicrosoftEntraIdClient2Test.xml"
        ));
        sf.suites.extend(entra.suites.clone());
        sf.skipped_tests.extend(entra.skipped_tests.clone());
        sf.summary.add(&entra.summary);
        sf.suites[0].module = Some("services".to_string());

        let digest = super::render_classes_digest("test", Some(&sf), None)
            .expect("suites present -> digest");
        insta::assert_snapshot!("pass_digest_snapshot", digest);
    }

    #[test]
    fn digest_header_uses_maven_native_format() {
        // Agents grep tee digests with Maven's own summary pattern
        // (`Tests run: N, Failures: N, Errors: N, Skipped: N`); the header must
        // match it so those greps hit instead of coming back empty.
        let mut sf = parsed_fixture(include_str!(
            "../../../tests/fixtures/surefire_xml/TEST-com.example.auth.user.UsersTest.xml"
        ));
        let entra = parsed_fixture(include_str!(
            "../../../tests/fixtures/surefire_xml/TEST-com.example.auth.partners.entraid.MicrosoftEntraIdClient2Test.xml"
        ));
        sf.suites.extend(entra.suites.clone());
        sf.skipped_tests.extend(entra.skipped_tests.clone());
        sf.summary.add(&entra.summary);

        let digest = super::render_classes_digest("test", Some(&sf), None)
            .expect("suites present -> digest");
        let header = digest.lines().next().expect("digest has a header line");
        let maven_re = regex::Regex::new(
            r"Tests run: (\d+), Failures: (\d+), Errors: (\d+), Skipped: (\d+)$",
        )
        .expect("valid regex");
        let caps = maven_re
            .captures(header)
            .unwrap_or_else(|| panic!("header not maven-native: {header}"));
        assert_eq!(&caps[1], sf.summary.run.to_string().as_str());
        assert_eq!(&caps[4], sf.summary.skipped.to_string().as_str());
    }

    #[test]
    fn digest_none_without_suites() {
        assert_eq!(super::render_classes_digest("test", None, None), None);
        let empty = super::surefire_reports::SurefireResult::default();
        assert_eq!(super::render_classes_digest("test", Some(&empty), None), None);
    }

    #[test]
    fn pass_inline_small_run_lists_classes() {
        let sf = parsed_fixture(include_str!(
            "../../../tests/fixtures/surefire_xml/TEST-com.example.auth.user.UsersTest.xml"
        ));
        let (out, needs_ref) =
            super::render_pass_inline("mvn test: 12 passed (3.1 s)", Some(&sf), None);
        assert!(out.starts_with("mvn test: 12 passed (3.1 s)\n"));
        assert!(out.contains("UsersTest:"), "short class name inline, got: {out}");
        assert!(!needs_ref, "1 class <= MAX_INLINE_CLASSES");
        insta::assert_snapshot!("pass_inline_small_snapshot", out);
    }

    #[test]
    fn pass_inline_large_run_defers_to_reference() {
        // Build >MAX_INLINE_CLASSES suites from the real fixture by cloning
        // its (real) suite stat under distinct class names.
        let base = parsed_fixture(include_str!(
            "../../../tests/fixtures/surefire_xml/TEST-com.example.auth.user.UsersTest.xml"
        ));
        let mut sf = super::surefire_reports::SurefireResult::default();
        for i in 0..6 {
            let mut s = base.suites[0].clone();
            s.class_name = format!("com.example.auth.Suite{i}Test");
            sf.suites.push(s);
            sf.summary.add(&base.summary);
        }
        let (out, needs_ref) =
            super::render_pass_inline("mvn test: 72 passed (9.0 s)", Some(&sf), None);
        assert_eq!(out, "mvn test: 72 passed (9.0 s)", "no inline list for >5 classes");
        assert!(needs_ref);
    }

    #[test]
    fn pass_inline_many_skipped_forces_reference() {
        let sf = parsed_fixture(include_str!(
            "../../../tests/fixtures/surefire_xml/TEST-com.example.auth.partners.entraid.MicrosoftEntraIdClient2Test.xml"
        ));
        // 8 skipped > MAX_INLINE_SKIPPED: names go to the digest only.
        let (out, needs_ref) =
            super::render_pass_inline("mvn test: 5 passed, 8 skipped (2.0 s)", Some(&sf), None);
        assert!(needs_ref, "skipped names beyond inline cap require the digest reference");
        assert!(
            !out.contains("skipped: "),
            "no skipped-name lines inline when count > cap, got: {out}"
        );
    }
}

