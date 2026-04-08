//! Filters Maven (`mvn`) command output — test results, build errors.
//!
//! State machine parser for `mvn test` output with states:
//! Preamble -> Testing -> Summary -> Done.
//! Strips thousands of noise lines to compact failure reports (99%+ savings).

use crate::core::runner;
use crate::core::tracking;
use crate::core::utils::{exit_code_from_status, resolved_command, strip_ansi};
use anyhow::{Context, Result};
use lazy_static::lazy_static;
use regex::Regex;
use std::ffi::OsString;
use std::path::Path;

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

    runner::run_filtered(
        cmd,
        "mvn test",
        &args.join(" "),
        filter_mvn_test,
        runner::RunOptions::with_tee("mvn_test"),
    )
}

/// Run `mvn build` (defaults to `package` goal) with line-filtered output.
pub fn run_build(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = mvn_command();

    // Default to 'package' if no goal specified
    if args.is_empty() {
        cmd.arg("package");
    } else {
        for arg in args {
            cmd.arg(arg);
        }
    }

    if verbose > 0 {
        let display = if args.is_empty() {
            "package".to_string()
        } else {
            args.join(" ")
        };
        eprintln!("Running: mvn {}", display);
    }

    runner::run_filtered(
        cmd,
        "mvn build",
        &args.join(" "),
        filter_mvn_build,
        runner::RunOptions::with_tee("mvn_build"),
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

/// Goals that produce filterable build output (short-lived, captured).
const BUILD_GOALS: &[&str] = &["compile", "package", "clean", "install", "verify"];

/// Handles mvn subcommands not matched by dedicated Clap variants.
/// Build-like goals go through `filter_mvn_build`; everything else
/// streams directly via `status()` (safe for long-running goals).
pub fn run_other(args: &[OsString], verbose: u8) -> Result<i32> {
    if args.is_empty() {
        anyhow::bail!("mvn: no subcommand specified");
    }

    let subcommand = args[0].to_string_lossy();

    if verbose > 0 {
        eprintln!("Running: mvn {} ...", subcommand);
    }

    // Route build-like goals through the build filter
    if BUILD_GOALS.contains(&subcommand.as_ref()) {
        let string_args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        return run_build(&string_args, verbose);
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

enum BuildResult {
    Success,
    Failure,
}

struct FailureEntry {
    name: String,
    details: Vec<String>,
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

    let mut failures: Vec<FailureEntry> = Vec::new();
    let mut current_failure: Option<FailureEntry> = None;

    let mut counts = TestCounts::default();
    let mut build_result: Option<BuildResult> = None;
    let mut total_time: Option<String> = None;
    let mut found_tests_section = false;

    for line in clean.lines() {
        let trimmed = line.trim();
        let stripped = strip_maven_prefix(trimmed);

        // Global transition: T E S T S marker resets to Testing from any state
        // (multi-module builds emit this marker per module)
        if stripped.contains("T E S T S") {
            found_tests_section = true;
            state = TestParseState::Testing;
            continue;
        }

        match state {
            TestParseState::Preamble => {}
            TestParseState::Testing => {
                if stripped == "Results:" {
                    if let Some(f) = current_failure.take() {
                        failures.push(f);
                    }
                    state = TestParseState::Summary;
                    continue;
                }

                if let Some(caps) = FAILURE_HEADER_RE.captures(trimmed) {
                    if let Some(f) = current_failure.take() {
                        failures.push(f);
                    }
                    let test_name = caps.get(1).map_or("", |m| m.as_str()).to_string();
                    current_failure = Some(FailureEntry {
                        name: test_name,
                        details: Vec::new(),
                    });
                    continue;
                }

                if let Some(ref mut f) = current_failure {
                    if f.details.len() >= MAX_DETAIL_LINES {
                        continue;
                    }
                    if is_framework_frame(stripped)
                        || is_maven_boilerplate(trimmed)
                        || stripped.is_empty()
                        || (trimmed.starts_with("[ERROR]") && stripped.contains("<<<"))
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

                if let Some(caps) = TESTS_RUN_RE.captures(stripped) {
                    counts.run = caps.get(1).map_or(0, |m| m.as_str().parse().unwrap_or(0));
                    counts.failures =
                        caps.get(2).map_or(0, |m| m.as_str().parse().unwrap_or(0));
                    counts.errors =
                        caps.get(3).map_or(0, |m| m.as_str().parse().unwrap_or(0));
                    counts.skipped =
                        caps.get(4).map_or(0, |m| m.as_str().parse().unwrap_or(0));
                }

                if stripped.contains("BUILD SUCCESS") {
                    build_result = Some(BuildResult::Success);
                } else if stripped.contains("BUILD FAILURE") {
                    build_result = Some(BuildResult::Failure);
                }

                if let Some(caps) = TOTAL_TIME_RE.captures(stripped) {
                    total_time = Some(caps.get(1).map_or("", |m| m.as_str()).trim().to_string());
                    state = TestParseState::Done;
                }
            }
            TestParseState::Done => break,
        }
    }

    if !found_tests_section {
        return "mvn test: no tests run".to_string();
    }

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

    if let Some(ref br) = build_result {
        let label = match br {
            BuildResult::Success => "SUCCESS",
            BuildResult::Failure => "FAILURE",
        };
        result.push_str(&format!("BUILD {}\n", label));
    }

    if !failures.is_empty() {
        result.push_str("\nFailures:\n");
    }
    for (i, failure) in failures.iter().take(MAX_FAILURES_SHOWN).enumerate() {
        result.push_str(&format!("{}. {}\n", i + 1, failure.name));
        for detail in &failure.details {
            if detail.len() > MAX_LINE_LENGTH {
                result.push_str(&format!("   {}...\n", &detail[..MAX_LINE_LENGTH]));
            } else {
                result.push_str(&format!("   {}\n", detail));
            }
        }
    }
    if failures.len() > MAX_FAILURES_SHOWN {
        result.push_str(&format!(
            "\n... +{} more failures\n",
            failures.len() - MAX_FAILURES_SHOWN
        ));
    }

    result.trim().to_string()
}

/// Strip [INFO], [ERROR], [WARNING] prefixes from Maven output lines.
fn strip_maven_prefix(line: &str) -> &str {
    let trimmed = line.trim();
    for tag in &["[INFO]", "[ERROR]", "[WARNING]"] {
        if let Some(rest) = trimmed.strip_prefix(tag) {
            return rest.trim_start();
        }
    }
    trimmed
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
    if line == "[ERROR]" || line == "[INFO]" || line == "[WARNING]" {
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
// Line filter for mvn build output
// ---------------------------------------------------------------------------

/// Filter `mvn build`/`mvn package` output — strip [INFO] noise, keep errors and summary.
fn filter_mvn_build(output: &str) -> String {
    let clean = strip_ansi(output);
    let mut result_lines: Vec<String> = Vec::new();

    for line in clean.lines() {
        let trimmed = line.trim();
        if should_keep_build_line(trimmed) {
            result_lines.push(trimmed.to_string());
        }
    }

    if result_lines.is_empty() {
        return "mvn: ok".to_string();
    }

    result_lines.join("\n")
}

const INFO_NOISE_PATTERNS: &[&str] = &[
    "---",
    "Building ",
    "Downloading ",
    "Downloaded ",
    "Scanning ",
    "Compiling ",
    "Recompiling ",
    "Nothing to compile",
    "Using auto detected",
    "Loaded ",
    "Creating container",
    "Container ",
    "Image ",
    "Testcontainers",
    "Docker ",
    "Ryuk ",
    "Checking the system",
    "Connected to docker",
    "Running ",
    "Tests run:",
    "Results:",
    "T E S T S",
    "Finished at:",
    "from pom.xml",
];

/// Returns true if a build output line should be kept.
fn should_keep_build_line(line: &str) -> bool {
    let trimmed = line.trim();

    if trimmed.is_empty() {
        return false;
    }

    let stripped = strip_maven_prefix(trimmed);

    // Keep error lines
    if trimmed.starts_with("[ERROR]") {
        return !is_maven_boilerplate(trimmed);
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
    if trimmed.starts_with("[INFO]") {
        if stripped.is_empty() {
            return false;
        }

        if stripped.starts_with("---") && stripped.chars().all(|c| c == '-' || c.is_whitespace()) {
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

        return true;
    }

    // Strip [WARNING] lines for build filter
    if trimmed.starts_with("[WARNING]") {
        return false;
    }

    // Keep anything else (compilation errors without prefix, etc.)
    true
}

// ---------------------------------------------------------------------------
// Line filter for mvn dependency:tree output
// ---------------------------------------------------------------------------

/// Filter `mvn dependency:tree` — strip Maven boilerplate, omitted duplicates,
/// and "version managed" annotations. Keep tree structure and conflicts.
fn filter_mvn_dep_tree(output: &str) -> String {
    let clean = strip_ansi(output);
    let mut result_lines: Vec<String> = Vec::new();

    for line in clean.lines() {
        let trimmed = line.trim();

        // Skip empty lines and Maven boilerplate
        if trimmed.is_empty() || is_maven_boilerplate(trimmed) {
            continue;
        }

        let stripped = strip_maven_prefix(trimmed);

        // Skip non-tree Maven lines (Scanning, Building, separators, etc.)
        if trimmed.starts_with("[WARNING]") {
            continue;
        }
        if trimmed.starts_with("[INFO]") {
            if stripped.is_empty() {
                continue;
            }
            // Skip separator lines
            if stripped.starts_with("---")
                && stripped.chars().all(|c| c == '-' || c.is_whitespace())
            {
                continue;
            }
            // Skip preamble noise
            if stripped.starts_with("Scanning ")
                || stripped.starts_with("Building ")
                || stripped.starts_with("Loaded ")
                || stripped.contains("from pom.xml")
                || stripped.contains("BUILD SUCCESS")
                || stripped.contains("BUILD FAILURE")
                || TOTAL_TIME_RE.is_match(stripped)
                || stripped.starts_with("Finished at:")
            {
                continue;
            }
        }

        // Skip lines with "omitted for duplicate"
        if stripped.contains("omitted for duplicate") {
            continue;
        }

        // Clean "version managed" annotations from kept lines
        let cleaned = VERSION_MANAGED_RE.replace_all(stripped, "").to_string();

        result_lines.push(cleaned);
    }

    if result_lines.is_empty() {
        return "mvn dependency:tree: no output".to_string();
    }

    result_lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::utils::count_tokens;

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
}
