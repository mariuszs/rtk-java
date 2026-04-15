//! Filters Maven (`mvn`) command output — test results, build errors.
//!
//! State machine parser for `mvn test` output with states:
//! Preamble -> Testing -> Summary -> Done.
//! Strips thousands of noise lines to compact failure reports (99%+ savings).

use crate::cmds::java::surefire_reports::{self, FailureKind, SurefireResult, TestFailure};
use crate::core::runner;
use crate::core::tracking;
use crate::core::utils::{exit_code_from_status, resolved_command, strip_ansi, truncate};
use anyhow::{Context, Result};
use lazy_static::lazy_static;
use regex::Regex;
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

    // os-maven-plugin detection output: `[INFO] os.detected.name: linux` etc.
    if t.starts_with("[INFO] os.detected") {
        return true;
    }

    false
}

/// Auto-detect mvnw wrapper; fall back to system `mvn`.
fn mvn_command() -> std::process::Command {
    if Path::new("mvnw").exists() {
        resolved_command("./mvnw")
    } else {
        resolved_command("mvn")
    }
}

/// Run `mvn test` with state-machine filtered output.
pub fn run_test(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = mvn_command();
    cmd.arg("test");

    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: mvn test {}", args.join(" "));
    }

    let started_at = std::time::SystemTime::now();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let app_pkg = crate::cmds::java::pom_groupid::detect(&cwd);

    let cwd_for_filter = cwd.clone();
    let app_pkg_for_filter = app_pkg.clone();

    runner::run_filtered(
        cmd,
        "mvn test",
        &args.join(" "),
        move |raw: &str| {
            let filtered = filter_mvn_test(raw);
            enrich_with_reports(
                &filtered,
                &cwd_for_filter,
                started_at,
                app_pkg_for_filter.as_deref(),
            )
        },
        runner::RunOptions::with_tee("mvn_test"),
    )
}

/// Run `mvn compile` with line-filtered output.
///
/// `compile` is itself a Maven lifecycle phase (not a goal name we invented),
/// so no implicit default is added when `args` is empty — `mvn compile` runs
/// the compile phase directly.
pub fn run_compile(args: &[String], verbose: u8) -> Result<i32> {
    run_compile_like("compile", args, verbose)
}

/// Shared implementation for compile-phase-like goals: runs `mvn <goal> <args>`
/// through `filter_mvn_compile`. Used directly by `run_compile` and reused by
/// `run_other` to route `process-classes` / `test-compile` through the same
/// filter while preserving the original goal name in the invocation and in
/// the tracking label.
fn run_compile_like(goal: &str, args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = mvn_command();
    cmd.arg(goal);
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: mvn {} {}", goal, args.join(" "));
    }

    let (tool_name, tee_label) = compile_like_labels(goal);

    runner::run_filtered(
        cmd,
        tool_name,
        &args.join(" "),
        filter_mvn_compile,
        runner::RunOptions::with_tee(tee_label),
    )
}

/// Run `mvn checkstyle:check` with compact output — strips mvn/JVM startup
/// noise, keeps violations and BUILD SUCCESS/FAILURE summary.
pub fn run_checkstyle(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = mvn_command();
    cmd.arg("checkstyle:check");
    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: mvn checkstyle:check {}", args.join(" "));
    }

    runner::run_filtered(
        cmd,
        "mvn checkstyle:check",
        &args.join(" "),
        filter_mvn_checkstyle,
        runner::RunOptions::with_tee("mvn_checkstyle"),
    )
}

/// Run `mvn dependency:tree` with filtered output — strips duplicates and boilerplate.
pub fn run_dep_tree(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = mvn_command();
    cmd.arg("dependency:tree");

    for arg in args {
        cmd.arg(arg);
    }

    if verbose > 0 {
        eprintln!("Running: mvn dependency:tree {}", args.join(" "));
    }

    runner::run_filtered(
        cmd,
        "mvn dependency:tree",
        &args.join(" "),
        filter_mvn_dep_tree,
        runner::RunOptions::with_tee("mvn_dep_tree"),
    )
}

/// Goals whose output looks like `mvn compile` (same noise profile: plugin
/// codegen, npm lifecycle, Liquibase, Docker). Tuples are
/// `(goal, tool_name, tee_label)` — single source of truth for routing,
/// tracking labels, and tee filenames.
const COMPILE_LIKE_GOALS: &[(&str, &str, &str)] = &[
    ("compile", "mvn compile", "mvn_compile"),
    ("process-classes", "mvn process-classes", "mvn_process_classes"),
    ("test-compile", "mvn test-compile", "mvn_test_compile"),
];

/// Look up the `(tool_name, tee_label)` pair for a compile-like goal. Callers
/// are gated on `route_goal` / `COMPILE_LIKE_GOALS`, so the fallback is only
/// reached if that invariant is violated.
fn compile_like_labels(goal: &str) -> (&'static str, &'static str) {
    for &(g, tool, tee) in COMPILE_LIKE_GOALS {
        if g == goal {
            return (tool, tee);
        }
    }
    ("mvn compile", "mvn_compile")
}

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
    if COMPILE_LIKE_GOALS.iter().any(|(g, _, _)| *g == subcommand) {
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
pub fn run_other(args: &[OsString], verbose: u8) -> Result<i32> {
    if args.is_empty() {
        anyhow::bail!("mvn: no subcommand specified");
    }

    let subcommand = args[0].to_string_lossy();

    if verbose > 0 {
        eprintln!("Running: mvn {} ...", subcommand);
    }

    match route_goal(&subcommand) {
        GoalRouting::Compile => {
            return run_compile_like(&subcommand, &trailing_args(args), verbose);
        }
        GoalRouting::Checkstyle => {
            return run_checkstyle(&trailing_args(args), verbose);
        }
        GoalRouting::Passthrough => {}
    }

    // Everything else: passthrough with streaming (safe for spring-boot:run etc.)
    let timer = tracking::TimedExecution::start();

    let mut cmd = mvn_command();
    for arg in args {
        cmd.arg(arg);
    }

    let status = cmd
        .status()
        .with_context(|| format!("Failed to run mvn {}", subcommand))?;

    let args_str = tracking::args_display(args);
    timer.track_passthrough(
        &format!("mvn {}", args_str),
        &format!("rtk mvn {} (passthrough)", args_str),
    );

    Ok(exit_code_from_status(&status, "mvn"))
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

#[derive(Default)]
struct TestCounts {
    run: u32,
    failures: u32,
    errors: u32,
    skipped: u32,
}

impl TestCounts {
    fn add(&mut self, other: &Self) {
        self.run += other.run;
        self.failures += other.failures;
        self.errors += other.errors;
        self.skipped += other.skipped;
    }
}

struct FailureEntry {
    name: String,
    details: Vec<String>,
}

/// Parse the four count fields from a `TESTS_RUN_RE` captures. The regex
/// guarantees four numeric groups so defaulting to 0 is only a safety net.
fn parse_counts(caps: &regex::Captures) -> TestCounts {
    TestCounts {
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
    app_package: Option<&str>,
) -> String {
    if !text_summary.starts_with("mvn ") {
        return text_summary.to_string();
    }

    let zero_tests = text_summary == "mvn test: no tests run"
        || text_summary.contains("0 passed");
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
        app_package,
    );
    let fs = surefire_reports::parse_dir(
        &cwd.join("target/failsafe-reports"),
        Some(since),
        app_package,
    );

    match (zero_tests, has_failures, &sf, &fs) {
        (true, _, None, None) => {
            "mvn test: 0 tests executed — surefire nie wykrył testów. \
             Sprawdź pom.xml (plugin surefire configuration) lub uruchom: \
             rtk proxy mvn test"
                .to_string()
        }
        (_, true, None, None) => format!(
            "{text_summary}\n(no XML reports found — check target/surefire-reports/ \
             or run: rtk proxy mvn test)"
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
///
/// States: Preamble -> Testing -> Summary -> Done
/// - Preamble: skip everything before "T E S T S" marker
/// - Testing: collect failure details from [ERROR] headers and assertion lines
/// - Summary: parse final "Tests run:" line, BUILD SUCCESS/FAILURE, Total time
/// - Done: stop at Help boilerplate
fn filter_mvn_test(output: &str) -> String {
    let clean = strip_ansi(output);
    let mut state = TestParseState::Preamble;

    let mut failures: Vec<FailureEntry> = Vec::with_capacity(MAX_FAILURES_SHOWN);
    let mut current_failure: Option<FailureEntry> = None;

    let mut cumulative = TestCounts::default();
    let mut section: Option<TestCounts> = None;
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

                if let Some(caps) = TOTAL_TIME_RE.captures(stripped) {
                    total_time = Some(caps.get(1).map_or("", |m| m.as_str()).trim().to_string());
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
        return "mvn test: no tests run".to_string();
    }

    let counts = cumulative;
    let time_str = total_time.as_deref().unwrap_or("?");
    let has_failures = counts.failures > 0 || counts.errors > 0;

    if !has_failures {
        let passed = counts.run.saturating_sub(counts.skipped);
        let mut summary = format!("mvn test: {} passed", passed);
        if counts.skipped > 0 {
            summary.push_str(&format!(", {} skipped", counts.skipped));
        }
        summary.push_str(&format!(" ({})", time_str));
        return summary;
    }

    let failed_count = counts.failures + counts.errors;
    let mut result = format!("mvn test: {} run, {} failed", counts.run, failed_count);
    if counts.skipped > 0 {
        result.push_str(&format!(", {} skipped", counts.skipped));
    }
    result.push_str(&format!(" ({})\n", time_str));

    result.push_str("BUILD FAILURE\n");

    if !failures.is_empty() {
        result.push_str("\nFailures:\n");
    }
    for (i, failure) in failures.iter().enumerate() {
        writeln!(result, "{}. {}", i + 1, failure.name).unwrap();
        for detail in &failure.details {
            writeln!(result, "   {}", truncate(detail, MAX_LINE_LENGTH)).unwrap();
        }
    }
    if total_failures_seen > MAX_FAILURES_SHOWN {
        writeln!(
            result,
            "\n... +{} more failures",
            total_failures_seen - MAX_FAILURES_SHOWN
        )
        .unwrap();
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
fn filter_mvn_compile(output: &str) -> String {
    let clean = strip_ansi(output);
    let result_lines: Vec<&str> = clean
        .lines()
        .map(str::trim)
        .filter(|line| should_keep_compile_line(line))
        .collect();

    if result_lines.is_empty() {
        return "mvn: ok".to_string();
    }

    result_lines.join("\n")
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
    "from pom.xml",
    "Copying ",
    "argLine set to",
    "Migration completed",
    "Inferring ",
    "No <input",
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

    let stripped = strip_maven_prefix(line);

    // Keep error lines
    if line.starts_with(ERROR_TAG) {
        return !is_maven_boilerplate(line);
    }

    // Keep BUILD SUCCESS/FAILURE
    if stripped.contains("BUILD SUCCESS") || stripped.contains("BUILD FAILURE") {
        return true;
    }

    // Keep Total time
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

        if stripped.contains("deprecated") || stripped.contains("WARNING") {
            return false;
        }

        // Code generator config params and bundle size lines (regex — slower, run last)
        if CODEGEN_CONFIG_RE.is_match(stripped) || BUNDLE_SIZE_RE.is_match(stripped) {
            return false;
        }

        return true;
    }

    // Strip [WARNING] lines for build filter
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

    #[test]
    fn test_test_counts_add() {
        let mut a = TestCounts {
            run: 10,
            failures: 1,
            errors: 2,
            skipped: 3,
        };
        let b = TestCounts {
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
            output.contains("com.skillpanel:beacon"),
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
        let savings = 100.0 - (output_tokens as f64 / input_tokens as f64 * 100.0);

        assert!(
            savings >= 30.0,
            "mvn dep tree simple: expected >=30% savings, got {:.1}% ({} -> {} tokens)",
            savings,
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
        assert_eq!(route_goal("verify"), GoalRouting::Passthrough);
        assert_eq!(route_goal("clean"), GoalRouting::Passthrough);
        assert_eq!(route_goal("deploy"), GoalRouting::Passthrough);

        // Long-running / interactive goals must always passthrough
        assert_eq!(route_goal("spring-boot:run"), GoalRouting::Passthrough);
        assert_eq!(route_goal("quarkus:dev"), GoalRouting::Passthrough);

        // Unknown / typo: passthrough (safer default)
        assert_eq!(route_goal("compilee"), GoalRouting::Passthrough);
        assert_eq!(route_goal(""), GoalRouting::Passthrough);
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
    fn test_filter_verify_auth_counts() {
        let input = include_str!("../../../tests/fixtures/mvn_verify_auth.txt");
        let output = filter_mvn_test(input);
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
        let output = filter_mvn_test(input);

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
            Some("com.example"),
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
            Some("com.example"),
        );
        assert!(out.contains("0 tests executed"));
        assert!(out.contains("rtk proxy mvn test") || out.contains("surefire"));
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
        let out = super::enrich_with_reports(text, tmp.path(), since, Some("com.example"));

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
        let out = super::enrich_with_reports(text, tmp.path(), since, Some("com.example"));
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
            Some("com.example"),
        );
        assert!(out.contains("no XML reports"));
        assert!(out.contains("rtk proxy mvn test"));
    }
}
