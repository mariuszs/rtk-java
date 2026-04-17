//! Filters Maven (`mvn`) command output — test results, build errors.
//!
//! State machine parser for `mvn test` output with states:
//! Preamble -> Testing -> Summary -> Done.
//! Strips thousands of noise lines to compact failure reports (99%+ savings).

use crate::cmds::java::surefire_reports::{self, FailureKind, SurefireResult, TestFailure, TestSummary};
use crate::core::runner;
use crate::core::tracking;
use crate::core::utils::{exit_code_from_status, resolved_command, strip_ansi, truncate};
use anyhow::{Context, Result};
use lazy_static::lazy_static;
use regex::Regex;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::Path;

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

/// Goals that share the test-output state machine (surefire + failsafe).
/// Restricted to the two variants the filter can format — adding a third
/// forces the matcher here to be updated, which is the point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestLikeGoal {
    Test,
    Verify,
}

impl TestLikeGoal {
    fn as_str(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Verify => "verify",
        }
    }

    fn filter(self) -> fn(&str) -> String {
        match self {
            Self::Test => filter_mvn_test,
            Self::Verify => filter_mvn_verify,
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

/// Run `<binary> test` with state-machine filter + surefire/failsafe XML enrichment.
pub fn run_test(binary: MvnBinary, args: &[String], verbose: u8) -> Result<i32> {
    run_tests_like(binary, TestLikeGoal::Test, args, verbose)
}

/// Run `<binary> verify`. Verify is the canonical goal that produces
/// `target/failsafe-reports/` (integration tests), so this is where failsafe
/// XML enrichment is most valuable; the state machine accumulates surefire +
/// failsafe `T E S T S` blocks into one combined summary.
pub fn run_verify(binary: MvnBinary, args: &[String], verbose: u8) -> Result<i32> {
    run_tests_like(binary, TestLikeGoal::Verify, args, verbose)
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
    let app_pkgs = crate::cmds::java::pom_groupid::detect(&cwd);

    let cwd_for_filter = cwd.clone();
    let filter = goal.filter();

    let (tool_name, tee_label) = mvn_labels(binary, goal_str, goal_str);
    runner::run_filtered(
        cmd,
        &tool_name,
        &args.join(" "),
        move |raw: &str| {
            let filtered = filter(raw);
            enrich_with_reports(&filtered, &cwd_for_filter, started_at, &app_pkgs, goal_str)
        },
        runner::RunOptions::with_tee(&tee_label),
    )
}

/// Run `mvn compile` with line-filtered output.
///
/// `compile` is itself a Maven lifecycle phase (not a goal name we invented),
/// so no implicit default is added when `args` is empty — `mvn compile` runs
/// the compile phase directly.
pub fn run_compile(binary: MvnBinary, args: &[String], verbose: u8) -> Result<i32> {
    run_compile_like(binary, "compile", args, verbose)
}

/// Shared implementation for compile-phase-like goals: runs `<binary> <goal> <args>`
/// through `filter_mvn_compile`. Used directly by `run_compile` and reused by
/// `run_other` to route `process-classes` / `test-compile` through the same
/// filter while preserving the original goal name in the invocation and in
/// the tracking label.
fn run_compile_like(binary: MvnBinary, goal: &str, args: &[String], verbose: u8) -> Result<i32> {
    let tee_slug = COMPILE_LIKE_GOALS
        .iter()
        .find_map(|&(g, slug)| (g == goal).then_some(slug))
        .expect("goal must be in COMPILE_LIKE_GOALS — gated by route_goal / run_compile");
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

/// Routing decision for a raw mvn subcommand seen on `run_other` — i.e. the
/// first positional arg after `rtk mvn`. Pure function, easy to unit-test.
#[derive(Debug, PartialEq, Eq)]
enum GoalRouting {
    /// Re-dispatch to `run_compile` (filter_mvn_compile).
    Compile,
    /// Re-dispatch to `run_checkstyle` (filter_mvn_checkstyle).
    Checkstyle,
    /// Stream unchanged via `status()`; tracked for metrics only.
    Passthrough,
}

fn route_goal(subcommand: &str) -> GoalRouting {
    if COMPILE_LIKE_GOALS.iter().any(|(g, _)| *g == subcommand) {
        return GoalRouting::Compile;
    }
    if subcommand == "checkstyle:check" || subcommand == "checkstyle" {
        return GoalRouting::Checkstyle;
    }
    GoalRouting::Passthrough
}

/// Convert `args[1..]` into `Vec<String>`, lossy-decoding any non-UTF-8 bytes.
/// The subcommand (args[0]) is stripped so callers can re-dispatch to a
/// `run_*` function that prepends its own goal name.
fn trailing_args(args: &[OsString]) -> Vec<String> {
    args.iter()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

/// Handles mvn subcommands not matched by dedicated Clap variants.
/// Compile-like goals go through `filter_mvn_compile`; `checkstyle` and
/// `checkstyle:check` go through `filter_mvn_checkstyle`; everything else
/// streams directly via `status()` (safe for long-running goals like
/// `spring-boot:run`, and metric-only for rare ones like `package`).
pub fn run_other(binary: MvnBinary, args: &[OsString], verbose: u8) -> Result<i32> {
    if args.is_empty() {
        anyhow::bail!("{binary}: no subcommand specified");
    }

    let subcommand = args[0].to_string_lossy();

    if verbose > 0 {
        eprintln!("Running: {binary} {} ...", subcommand);
    }

    match route_goal(&subcommand) {
        GoalRouting::Compile => {
            return run_compile_like(binary, &subcommand, &trailing_args(args), verbose);
        }
        GoalRouting::Checkstyle => {
            return run_checkstyle(binary, &trailing_args(args), verbose);
        }
        GoalRouting::Passthrough => {}
    }

    // Everything else: passthrough with streaming (safe for spring-boot:run etc.)
    let timer = tracking::TimedExecution::start();

    let mut cmd = mvn_command(binary);
    for arg in args {
        cmd.arg(arg);
    }

    let status = cmd
        .status()
        .with_context(|| format!("Failed to run {binary} {}", subcommand))?;

    let args_str = tracking::args_display(args);
    timer.track_passthrough(
        &format!("{binary} {}", args_str),
        &format!("rtk {binary} {} (passthrough)", args_str),
    );

    Ok(exit_code_from_status(&status, binary.as_str()))
}

// ---------------------------------------------------------------------------
// State machine parser for mvn test output
// ---------------------------------------------------------------------------

const MAX_DETAIL_LINES: usize = 3;
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

/// Wrap the text-filter summary with structured failure details sourced from
/// `target/surefire-reports/` and `target/failsafe-reports/` XML files.
pub(crate) fn enrich_with_reports(
    text_summary: &str,
    cwd: &std::path::Path,
    since: std::time::SystemTime,
    app_packages: &[String],
    goal: &str,
) -> String {
    if !text_summary.starts_with("mvn ") {
        return text_summary.to_string();
    }

    let zero_tests = text_summary.ends_with(": no tests run")
        || text_summary.contains(": 0 passed");
    let has_failures =
        text_summary.contains("failed") || text_summary.contains("BUILD FAILURE");
    let looks_clean = text_summary.contains("passed (")
        && !text_summary.contains("failed")
        && !text_summary.contains("BUILD FAILURE");

    if looks_clean && !zero_tests {
        return text_summary.to_string();
    }

    let sf = surefire_reports::parse_dir(
        &cwd.join("target/surefire-reports"),
        Some(since),
        app_packages,
    );
    let fs = surefire_reports::parse_dir(
        &cwd.join("target/failsafe-reports"),
        Some(since),
        app_packages,
    );

    match (zero_tests, has_failures, &sf, &fs) {
        (true, _, None, None) => format!(
            "mvn {goal}: 0 tests executed — surefire detected no tests. \
             Check pom.xml (surefire plugin configuration) or run: \
             rtk proxy mvn {goal}"
        ),
        (_, true, None, None) => format!(
            "{text_summary}\n(no XML reports found — check target/surefire-reports/ \
             or run: rtk proxy mvn {goal})"
        ),
        _ => render_enriched(text_summary, sf.as_ref(), fs.as_ref()),
    }
}

fn render_enriched(
    text_summary: &str,
    surefire: Option<&SurefireResult>,
    failsafe: Option<&SurefireResult>,
) -> String {
    let mut out = String::from(text_summary);

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

fn render_failure_block(out: &mut String, failures: &[TestFailure]) {
    let shown = failures.iter().take(MAX_FAILURES_PER_SOURCE);
    for (i, f) in shown.enumerate() {
        writeln!(out, "{}. {}.{}", i + 1, f.test_class, f.test_method).ok();
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
pub(crate) fn filter_mvn_test(output: &str) -> String {
    filter_mvn_tests_with_goal(output, "test")
}

pub(crate) fn filter_mvn_verify(output: &str) -> String {
    filter_mvn_tests_with_goal(output, "verify")
}

/// Shared state machine parser for test-producing goals (`test`, `verify`).
///
/// States: Preamble -> Testing -> Summary -> Done
/// - Preamble: skip everything before "T E S T S" marker
/// - Testing: collect failure details from [ERROR] headers and assertion lines
/// - Summary: parse final "Tests run:" line, BUILD SUCCESS/FAILURE, Total time
/// - Done: stop at Help boilerplate
fn filter_mvn_tests_with_goal(output: &str, goal: &str) -> String {
    let clean = strip_ansi(output);
    let mut state = TestParseState::Preamble;

    let mut failures: Vec<FailureEntry> = Vec::with_capacity(MAX_FAILURES_SHOWN);
    let mut current_failure: Option<FailureEntry> = None;

    let mut cumulative = TestSummary::default();
    let mut section: Option<TestSummary> = None;
    let mut total_time: Option<String> = None;
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
                    if f.details.len() >= MAX_DETAIL_LINES {
                        continue;
                    }
                    if is_framework_frame(stripped)
                        || is_maven_boilerplate(trimmed)
                        || stripped.is_empty()
                        || (trimmed.starts_with(ERROR_TAG) && stripped.contains("<<<"))
                    {
                        continue;
                    }
                    f.details.push(stripped.to_string());
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

                if let Some(t) = parse_total_time(stripped) {
                    total_time = Some(t.to_string());
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
        return format!("mvn {goal}: no tests run");
    }

    let counts = cumulative;
    let time_str = total_time.as_deref().unwrap_or("?");
    let has_failures = counts.failures > 0 || counts.errors > 0;

    if !has_failures {
        let passed = counts.run.saturating_sub(counts.skipped);
        let mut summary = format!("mvn {goal}: {} passed", passed);
        if counts.skipped > 0 {
            summary.push_str(&format!(", {} skipped", counts.skipped));
        }
        summary.push_str(&format!(" ({})", time_str));
        return summary;
    }

    let failed_count = counts.failures + counts.errors;
    let mut result = format!("mvn {goal}: {} run, {} failed", counts.run, failed_count);
    if counts.skipped > 0 {
        result.push_str(&format!(", {} skipped", counts.skipped));
    }
    result.push_str(&format!(" ({})\n", time_str));

    result.push_str("BUILD FAILURE\n");

    if !failures.is_empty() {
        result.push_str("\nFailures:\n");
    }
    for (i, failure) in failures.iter().enumerate() {
        writeln!(result, "{}. {}", i + 1, failure.name).ok();
        for detail in &failure.details {
            writeln!(result, "   {}", truncate(detail, MAX_LINE_LENGTH)).ok();
        }
    }
    if total_failures_seen > MAX_FAILURES_SHOWN {
        writeln!(
            result,
            "\n... +{} more failures",
            total_failures_seen - MAX_FAILURES_SHOWN
        )
        .ok();
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

    // --- Regression coverage on fixtures adapted from rtk-ai/rtk#782 ---
    // (multi-module reactor with aggregated Surefire output + dual-emitted
    // javac errors — validates our Reactor Summary collapse and error dedup.)

    #[test]
    fn test_pr782_test_pass_accumulates_modules() {
        let input = include_str!("../../../tests/fixtures/mvn_pr782_test_pass_raw.txt");
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
    fn test_pr782_test_fail_accumulates_and_dedups() {
        let input = include_str!("../../../tests/fixtures/mvn_pr782_test_fail_raw.txt");
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
    fn test_pr782_compile_success_collapses_reactor() {
        let input =
            include_str!("../../../tests/fixtures/mvn_pr782_compile_success_raw.txt");
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
    fn test_pr782_compile_fail_dedups_and_names_failed_module() {
        let input =
            include_str!("../../../tests/fixtures/mvn_pr782_compile_fail_raw.txt");
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
    fn test_empty_input() {
        let output = filter_mvn_test("");
        assert_eq!(output, "mvn test: no tests run");
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

        assert!(
            savings >= 95.0,
            "mvn test large ANSI pass: expected >=95% savings, got {:.1}% ({} -> {} tokens)",
            savings,
            input_tokens,
            output_tokens,
        );
    }

    #[test]
    fn test_no_test_section() {
        let input = "[INFO] Building my-project 1.0\n[INFO] BUILD SUCCESS\n";
        let output = filter_mvn_test(input);
        assert_eq!(output, "mvn test: no tests run");
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

    // --- run_other routing ---

    #[test]
    fn test_route_goal() {
        // Compile-family → compile filter
        assert_eq!(route_goal("compile"), GoalRouting::Compile);
        assert_eq!(route_goal("process-classes"), GoalRouting::Compile);
        assert_eq!(route_goal("test-compile"), GoalRouting::Compile);

        // Checkstyle (both canonical and short form)
        assert_eq!(route_goal("checkstyle:check"), GoalRouting::Checkstyle);
        assert_eq!(route_goal("checkstyle"), GoalRouting::Checkstyle);

        // Rare lifecycle phases → passthrough (rare in real usage)
        assert_eq!(route_goal("package"), GoalRouting::Passthrough);
        assert_eq!(route_goal("install"), GoalRouting::Passthrough);
        assert_eq!(route_goal("clean"), GoalRouting::Passthrough);
        assert_eq!(route_goal("deploy"), GoalRouting::Passthrough);

        // Long-running / interactive goals must always passthrough
        assert_eq!(route_goal("spring-boot:run"), GoalRouting::Passthrough);
        assert_eq!(route_goal("quarkus:dev"), GoalRouting::Passthrough);

        // Unknown / typo: passthrough (safer default)
        assert_eq!(route_goal("compilee"), GoalRouting::Passthrough);
        assert_eq!(route_goal(""), GoalRouting::Passthrough);
    }

    #[test]
    fn test_run_other_empty_args_errors() {
        let result = run_other(MvnBinary::Mvn, &[], 0);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("no subcommand"),
            "expected 'no subcommand' error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_run_other_empty_args_errors_mvnd() {
        let result = run_other(MvnBinary::Mvnd, &[], 0);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("mvnd: no subcommand"),
            "expected 'mvnd: no subcommand' error, got: {}",
            err_msg
        );
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

    #[test]
    fn enrich_happy_path_passes_through_without_io() {
        let tmp = tempfile::tempdir().unwrap();
        // No target/ directory exists under tmp — ensures no I/O fallback would succeed.
        let text = "mvn test: 42 passed (1.234 s)";
        let out = super::enrich_with_reports(
            text,
            tmp.path(),
            std::time::SystemTime::now(),
            &pkgs("com.example"),
            "test",
        );
        assert_eq!(out, text);
    }

    #[test]
    fn enrich_no_tests_with_no_reports_emits_red_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let text = "mvn test: no tests run";
        let out = super::enrich_with_reports(
            text,
            tmp.path(),
            std::time::SystemTime::now(),
            &pkgs("com.example"),
            "test",
        );
        assert!(out.contains("0 tests executed"));
        assert!(out.contains("rtk proxy mvn test") || out.contains("surefire"));
    }

    #[test]
    fn enrich_no_tests_for_verify_goal_uses_verify_in_message() {
        let tmp = tempfile::tempdir().unwrap();
        let text = "mvn verify: no tests run";
        let out = super::enrich_with_reports(
            text,
            tmp.path(),
            std::time::SystemTime::now(),
            &pkgs("com.example"),
            "verify",
        );
        assert!(
            out.contains("0 tests executed"),
            "zero-tests branch must fire for verify, got: {}",
            out
        );
        assert!(
            out.contains("rtk proxy mvn verify"),
            "error message must reference the verify goal, got: {}",
            out
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

        assert!(out.contains("Failures (from surefire-reports/)"));
        assert!(out.contains("com.example.FailingTest.shouldReturnUser"));
        assert!(out.contains("reports:"));
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
        assert!(out.contains("Failures (from surefire-reports/)"));
        assert!(out.contains("Integration failures (from failsafe-reports/)"));
        assert!(out.contains("Caused by: org.hibernate.HibernateException"));
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
        assert!(out.contains("no XML reports"));
        assert!(out.contains("rtk proxy mvn test"));
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
        assert_eq!(out, text, "10 passed must short-circuit without enrichment");
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
        insta::assert_snapshot!(out);
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
        insta::assert_snapshot!(out);
    }

    #[test]
    fn snapshot_red_flag_no_tests() {
        let tmp = tempfile::tempdir().unwrap();
        let out = super::enrich_with_reports(
            "mvn test: no tests run",
            tmp.path(),
            std::time::SystemTime::now(),
            &pkgs("com.example"),
            "test",
        );
        insta::assert_snapshot!(out);
    }

    #[test]
    fn savings_happy_path_unchanged_by_enrichment() {
        // Happy path short-circuits without I/O; savings must match pre-enrichment.
        let text = "mvn test: 859 passed, 4 skipped (02:11 min)";
        let tmp = tempfile::tempdir().unwrap();
        let out = super::enrich_with_reports(
            text,
            tmp.path(),
            std::time::SystemTime::now(),
            &pkgs("com.example"),
            "test",
        );
        assert_eq!(out, text, "happy path must not allocate or append");
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
        let enriched_tokens = count_tokens(&enriched);
        let savings = 100.0 - (enriched_tokens as f64 / raw_tokens as f64 * 100.0);
        assert!(
            savings >= 85.0,
            "expected ≥85% savings on enriched failure path, got {savings:.1}% \
             (raw={raw_tokens}, enriched={enriched_tokens})"
        );
    }

    // --- Regression coverage on fixtures adapted from rtk-ai/rtk#1241 ---
    // (covers gaps found when running our filter against the competing JVM
    // PR's fixtures: Maven environment banner, JVM 21+ restricted-method
    // WARNINGs, SLF4J init noise, pgpverify-maven-plugin chatter,
    // maven-resources-plugin copy lines, clean-audit checkstyle output, and
    // mvn 3.9.x Reactor Build Order `<name> <version>` format without
    // `[pom|jar]` suffix.)

    #[test]
    fn test_pr1241_compile_pgp_multimodule_savings() {
        let input = include_str!("../../../tests/fixtures/mvn_pr1241_compile_pgp_multimodule.txt");
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
    fn test_pr1241_compile_pgp_strips_banner_and_jvm_warnings() {
        let input = include_str!("../../../tests/fixtures/mvn_pr1241_compile_pgp_multimodule.txt");
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
    fn test_pr1241_test_failure_stack_does_not_bleed_next_test() {
        let input = include_str!("../../../tests/fixtures/mvn_pr1241_test_failure_simple.txt");
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
            !output.trim().ends_with("no tests run")
                && output.len() > "mvn test: no tests run".len(),
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
}
