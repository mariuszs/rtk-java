//! Filters Vitest test output to show only failures.

use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crate::core::stream::exec_capture;
use crate::core::tracking;
use crate::core::utils::{package_manager_exec, strip_ansi};
use crate::parser::{
    FormatMode, OutputParser, ParseResult, TestFailure, TestResult, TokenFormatter,
    emit_degradation_warning, emit_passthrough_warning, extract_json_object, truncate_output,
    truncate_passthrough,
};

/// Vitest JSON output structures (tool-specific format)
#[derive(Debug, Deserialize)]
struct VitestJsonOutput {
    #[serde(rename = "testResults")]
    test_results: Vec<VitestTestFile>,
    #[serde(rename = "numTotalTests")]
    num_total_tests: usize,
    #[serde(rename = "numPassedTests")]
    num_passed_tests: usize,
    #[serde(rename = "numFailedTests")]
    num_failed_tests: usize,
    #[serde(rename = "numPendingTests", default)]
    num_pending_tests: usize,
}

#[derive(Debug, Deserialize)]
struct VitestTestFile {
    name: String,
    #[serde(rename = "assertionResults")]
    assertion_results: Vec<VitestTest>,
    #[serde(default)]
    status: String,
    /// Why the file failed as a whole (an import that does not resolve, a
    /// throw at module scope): no test ran, so no assertion carries it.
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct VitestTest {
    #[serde(rename = "fullName")]
    full_name: String,
    status: String,
    #[serde(rename = "failureMessages")]
    failure_messages: Vec<String>,
}

/// Parser for Vitest JSON output
pub struct VitestParser;

impl OutputParser for VitestParser {
    type Output = TestResult;

    fn parse(input: &str) -> ParseResult<TestResult> {
        // Tier 1: Try JSON parsing (with extraction fallback for pnpm/dotenv prefixes)
        let json_result = serde_json::from_str::<VitestJsonOutput>(input).or_else(|first_err| {
            // Fallback: Try extracting JSON object from prefixed output
            if let Some(extracted) = extract_json_object(input) {
                serde_json::from_str::<VitestJsonOutput>(extracted)
            } else {
                Err(first_err)
            }
        });

        match json_result {
            Ok(json) => {
                let failures = extract_failures_from_json(&json);
                let failed_files = json
                    .test_results
                    .iter()
                    .filter(|file| file_level_failure(file).is_some())
                    .count();

                let result = TestResult {
                    total: json.num_total_tests,
                    passed: json.num_passed_tests,
                    failed: json.num_failed_tests + failed_files,
                    skipped: json.num_pending_tests,
                    duration_ms: None,
                    failures,
                };

                ParseResult::Full(result)
            }
            Err(e) => {
                // Tier 2: Try regex extraction (only fires if user overrides --reporter flag)
                match extract_stats_regex(input) {
                    Some(result) => {
                        ParseResult::Degraded(result, vec![format!("JSON parse failed: {}", e)])
                    }
                    None => {
                        // Tier 3: Passthrough
                        ParseResult::Passthrough(truncate_passthrough(input))
                    }
                }
            }
        }
    }
}

/// Extract failures from JSON structure
fn extract_failures_from_json(json: &VitestJsonOutput) -> Vec<TestFailure> {
    let mut failures = Vec::new();

    for file in &json.test_results {
        if let Some(failure) = file_level_failure(file) {
            failures.push(failure);
        }
        for test in &file.assertion_results {
            if test.status == "failed" {
                let error_message = test.failure_messages.join("\n");
                failures.push(TestFailure {
                    test_name: test.full_name.clone(),
                    file_path: file.name.clone(),
                    error_message,
                    stack_trace: None,
                });
            }
        }
    }

    failures
}

/// A file that failed without a failing test: its error is in `message` alone,
/// and `numFailedTests` does not count it. A file whose tests failed is left to
/// them (Jest repeats every assertion failure in `message`).
fn file_level_failure(file: &VitestTestFile) -> Option<TestFailure> {
    let has_failed_test = file.assertion_results.iter().any(|t| t.status == "failed");
    if file.status != "failed" || has_failed_test || file.message.trim().is_empty() {
        return None;
    }
    Some(TestFailure {
        test_name: file.name.clone(),
        file_path: file.name.clone(),
        error_message: file.message.clone(),
        stack_trace: None,
    })
}

/// Tier 2: Extract test statistics using regex (degraded mode)
fn extract_stats_regex(output: &str) -> Option<TestResult> {
    static TESTS_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"Tests\s+(?:(\d+)\s+failed\s+\|\s+)?(\d+)\s+passed").unwrap());
    static DURATION_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"Duration\s+([\d.]+)(ms|s)").unwrap());

    let clean_output = strip_ansi(output);

    let mut passed = 0;
    let mut failed = 0;
    let mut total = 0;

    // Parse test counts
    if let Some(caps) = TESTS_RE.captures(&clean_output) {
        if let Some(fail_str) = caps.get(1) {
            failed = fail_str.as_str().parse().unwrap_or(0);
        }
        if let Some(pass_str) = caps.get(2) {
            passed = pass_str.as_str().parse().unwrap_or(0);
        }
        total = passed + failed;
    }

    // Parse duration
    let duration_ms = DURATION_RE.captures(&clean_output).and_then(|caps| {
        let value: f64 = caps[1].parse().ok()?;
        let unit = &caps[2];
        Some(if unit == "ms" {
            value as u64
        } else {
            (value * 1000.0) as u64
        })
    });

    // Only return if we found valid data
    if total > 0 {
        Some(TestResult {
            total,
            passed,
            failed,
            skipped: 0,
            duration_ms,
            failures: extract_failures_regex(&clean_output),
        })
    } else {
        None
    }
}

/// Extract failures using regex
fn extract_failures_regex(output: &str) -> Vec<TestFailure> {
    let mut failures = Vec::new();
    let lines: Vec<&str> = output.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        if line.contains("[x]") || line.contains("FAIL") {
            let mut error_lines = vec![line.to_string()];
            i += 1;

            // Collect subsequent indented lines
            while i < lines.len() && lines[i].starts_with("  ") {
                error_lines.push(lines[i].trim().to_string());
                i += 1;
            }

            if !error_lines.is_empty() {
                failures.push(TestFailure {
                    test_name: error_lines[0].clone(),
                    file_path: String::new(),
                    error_message: error_lines[1..].join("\n"),
                    stack_trace: None,
                });
            }
        } else {
            i += 1;
        }
    }

    failures
}

pub fn run_vitest(args: &[String], verbose: u8) -> Result<i32> {
    // Vitest 5 no longer prints the JSON reporter to stdout: it writes
    // `.vitest/json/output.json` and prints only "JSON report written to …".
    // Name the file ourselves (older Vitest honours `--outputFile.json` too) so
    // the report is read from a known place and nothing lands in the project.
    // Without a temp dir the run still works, parsed from stdout as before.
    let report_dir = tempfile::tempdir().ok();
    let report = report_dir
        .as_ref()
        .map(|dir| dir.path().join("report.json"));
    run_framework_test(vitest_invocation(args, report.as_deref()), verbose)
}

pub fn run_jest(args: &[String], verbose: u8) -> Result<i32> {
    run_framework_test(jest_invocation(args), verbose)
}

/// What `rtk vitest` or `rtk jest` spawns: the tool, the arguments it gets,
/// and whether the user's own reporter output is shown instead of parsed.
struct TestInvocation {
    framework: &'static str,
    args: Vec<String>,
    passthrough: bool,
    /// Where the JSON report is written, when it is not on stdout.
    report_file: Option<PathBuf>,
}

fn vitest_invocation(args: &[String], report: Option<&Path>) -> TestInvocation {
    // A user-chosen `--outputFile` sends the report where they asked, so there
    // is nothing for rtk to parse: show their run as it is.
    let passthrough = has_explicit_vitest_reporter(args) || has_vitest_output_file(args);
    let mut effective = vec!["run".to_string()];
    let mut report_file = None;

    if !passthrough {
        effective.push("--reporter=json".to_string());
        if let Some(path) = report {
            effective.push(format!("--outputFile.json={}", path.display()));
            report_file = Some(path.to_path_buf());
        }
    }

    for arg in args {
        if should_skip_vitest_arg(arg) {
            continue;
        }
        effective.push(arg.clone());
    }

    TestInvocation {
        framework: "vitest",
        args: effective,
        passthrough,
        report_file,
    }
}

fn jest_invocation(args: &[String]) -> TestInvocation {
    let mut effective = vec![
        // Force non-watch mode
        "--no-watch".to_string(),
        // Enable JSON structured output
        "--json".to_string(),
    ];
    effective.extend(
        args.iter()
            .filter(|arg| !should_skip_jest_arg(arg))
            .cloned(),
    );
    TestInvocation {
        framework: "jest",
        args: effective,
        passthrough: false,
        report_file: None,
    }
}

fn should_skip_jest_arg(arg: &str) -> bool {
    arg == "run"
        || arg.starts_with("--json")
        || arg.starts_with("--reporter")
        || arg.starts_with("--watch")
}

fn run_framework_test(invocation: TestInvocation, verbose: u8) -> Result<i32> {
    let timer = tracking::TimedExecution::start();
    let framework = invocation.framework;
    let mut cmd = package_manager_exec(framework);
    cmd.args(&invocation.args);

    let result = exec_capture(&mut cmd).context(format!("Failed to run {}", framework))?;
    let combined = result.combined();
    let report = invocation
        .report_file
        .as_deref()
        .and_then(|path| std::fs::read_to_string(path).ok());

    let filtered = format_test_output(
        framework,
        report_or_stdout(report.as_deref(), &result.stdout),
        &combined,
        invocation.passthrough,
        verbose,
    );
    let tee_label = format!("{}_run", framework);
    let raw = raw_with_report(&combined, report.as_deref());

    let rendered = render_test_output(&filtered, &raw, &tee_label, result.exit_code);
    let shown = crate::core::runner::emit_guarded(&rendered, None, &raw);

    timer.track(
        format!("{} run", framework).as_str(),
        format!("rtk {} run", framework).as_str(),
        &raw,
        &shown,
    );

    if !result.success() {
        return Ok(result.exit_code);
    }
    Ok(0)
}

struct FormattedTestOutput {
    text: String,
    truncated: bool,
}

impl FormattedTestOutput {
    fn new(text: String) -> Self {
        Self {
            text,
            truncated: false,
        }
    }

    fn truncated(text: String) -> Self {
        Self {
            text,
            truncated: true,
        }
    }
}

fn has_explicit_vitest_reporter(args: &[String]) -> bool {
    args.iter()
        .any(|arg| arg == "--reporter" || arg.starts_with("--reporter="))
}

fn has_vitest_output_file(args: &[String]) -> bool {
    args.iter().any(|arg| arg.starts_with("--outputFile"))
}

/// The JSON to parse: the report file when the run wrote one, else stdout
/// (a run that died before reporting leaves the file absent or empty).
fn report_or_stdout<'a>(report: Option<&'a str>, stdout: &'a str) -> &'a str {
    report
        .filter(|json| !json.trim().is_empty())
        .unwrap_or(stdout)
}

/// The run's raw output with the report it wrote to a file: what the guard,
/// the tee and tracking measure against. Without the report, the never-worse
/// guard would compare a failure summary with the one-line "JSON report
/// written to …" notice and print the notice instead.
fn raw_with_report(combined: &str, report: Option<&str>) -> String {
    match report.filter(|json| !json.trim().is_empty()) {
        Some(json) => format!("{combined}{json}\n"),
        None => combined.to_string(),
    }
}

fn should_skip_vitest_arg(arg: &str) -> bool {
    arg == "run" || arg.starts_with("--json") || arg.starts_with("--watch")
}

fn format_test_output(
    framework: &str,
    json: &str,
    combined: &str,
    passthrough_requested: bool,
    verbose: u8,
) -> FormattedTestOutput {
    if passthrough_requested {
        return format_passthrough_output(combined);
    }

    let parse_result = VitestParser::parse(json);
    let mode = FormatMode::from_verbosity(verbose);
    match parse_result {
        ParseResult::Full(data) => {
            if verbose > 0 {
                eprintln!("{} run (Tier 1: Full JSON parse)", framework);
            }
            FormattedTestOutput::new(data.format(mode))
        }
        ParseResult::Degraded(data, warnings) => {
            if verbose > 0 {
                emit_degradation_warning(framework, &warnings.join(", "));
            }
            FormattedTestOutput::new(data.format(mode))
        }
        ParseResult::Passthrough(_) => {
            emit_passthrough_warning(framework, "All parsing tiers failed");
            // stderr carries why the run produced no report (import or config
            // error); stdout alone would show only Vitest's banner.
            format_passthrough_output(combined)
        }
    }
}

fn format_passthrough_output(raw: &str) -> FormattedTestOutput {
    let max_chars = crate::core::config::limits().passthrough_max_chars;
    format_passthrough_output_with_limit(raw, max_chars)
}

fn format_passthrough_output_with_limit(raw: &str, max_chars: usize) -> FormattedTestOutput {
    let text = truncate_output(raw, max_chars);

    if raw.chars().count() > max_chars {
        FormattedTestOutput::truncated(text)
    } else {
        FormattedTestOutput::new(text)
    }
}

fn render_test_output(
    filtered: &FormattedTestOutput,
    raw: &str,
    tee_label: &str,
    exit_code: i32,
) -> String {
    render_test_output_with_hints(
        filtered,
        raw,
        tee_label,
        exit_code,
        crate::core::tee::force_tee_hint,
        crate::core::tee::tee_and_hint,
    )
}

fn render_test_output_with_hints<F, T>(
    filtered: &FormattedTestOutput,
    raw: &str,
    tee_label: &str,
    exit_code: i32,
    force_hint: F,
    tee_hint: T,
) -> String
where
    F: FnOnce(&str, &str) -> Option<String>,
    T: FnOnce(&str, &str, i32) -> Option<String>,
{
    let hint = if filtered.truncated {
        force_hint(raw, tee_label)
    } else {
        tee_hint(raw, tee_label, exit_code)
    };

    crate::core::runner::with_hint(&filtered.text, hint.as_deref()).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn test_vitest_invocation_runs_vitest() {
        let invocation = vitest_invocation(&args(&["src/a.test.ts"]), None);

        assert_eq!(invocation.framework, "vitest");
    }

    #[test]
    fn test_jest_invocation_runs_jest_once_with_json_output() {
        let invocation = jest_invocation(&args(&["src/a.test.js"]));

        assert_eq!(invocation.framework, "jest");
        assert_eq!(
            invocation.args,
            args(&["--no-watch", "--json", "src/a.test.js"])
        );
        assert!(!invocation.passthrough);
    }

    #[test]
    fn test_jest_invocation_drops_run_json_reporter_and_watch_args() {
        let invocation = jest_invocation(&args(&[
            "run",
            "--json",
            "--reporters=default",
            "--watch",
            "-t",
            "adds",
            "src/a.test.js",
        ]));

        assert_eq!(
            invocation.args,
            args(&["--no-watch", "--json", "-t", "adds", "src/a.test.js"])
        );
    }

    #[test]
    fn test_vitest_parser_json() {
        let json = r#"{
            "numTotalTests": 13,
            "numPassedTests": 13,
            "numFailedTests": 0,
            "numPendingTests": 0,
            "testResults": [],
            "startTime": 1000
        }"#;

        let result = VitestParser::parse(json);
        assert_eq!(result.tier(), 1);
        assert!(result.is_ok());

        let data = result.unwrap();
        assert_eq!(data.total, 13);
        assert_eq!(data.passed, 13);
        assert_eq!(data.failed, 0);
        assert_eq!(data.duration_ms, None);
    }

    #[test]
    fn test_vitest_parser_regex_fallback() {
        let text = r#"
 Test Files  2 passed (2)
      Tests  13 passed (13)
   Duration  450ms
        "#;

        let result = VitestParser::parse(text);
        assert_eq!(result.tier(), 2); // Degraded
        assert!(result.is_ok());

        let data = result.unwrap();
        assert_eq!(data.passed, 13);
        assert_eq!(data.failed, 0);
    }

    #[test]
    fn test_vitest_parser_passthrough() {
        let invalid = "random output with no structure";
        let result = VitestParser::parse(invalid);
        assert_eq!(result.tier(), 3); // Passthrough
        assert!(!result.is_ok());
    }

    #[test]
    fn test_strip_ansi() {
        let input = "\x1b[32m✓\x1b[0m test passed";
        let output = strip_ansi(input);
        assert_eq!(output, "✓ test passed");
        assert!(!output.contains("\x1b"));
    }

    #[test]
    fn test_vitest_parser_with_pnpm_prefix() {
        let input = r#"
Scope: all 6 workspace projects
 WARN  deprecated inflight@1.0.6: This module is not supported

{"numTotalTests": 13, "numPassedTests": 13, "numFailedTests": 0, "numPendingTests": 0, "testResults": [], "startTime": 1000}
"#;
        let result = VitestParser::parse(input);
        assert_eq!(result.tier(), 1, "Should succeed with Tier 1 (full parse)");
        assert!(result.is_ok());

        let data = result.unwrap();
        assert_eq!(data.total, 13);
        assert_eq!(data.passed, 13);
        assert_eq!(data.failed, 0);
    }

    #[test]
    fn test_vitest_parser_with_dotenv_prefix() {
        let input = r#"[dotenv] Loading environment variables from .env
[dotenv] Injected 5 variables

{"numTotalTests": 5, "numPassedTests": 4, "numFailedTests": 1, "numPendingTests": 0, "testResults": [], "startTime": 2000}
"#;
        let result = VitestParser::parse(input);
        assert_eq!(result.tier(), 1, "Should succeed with Tier 1 (full parse)");
        assert!(result.is_ok());

        let data = result.unwrap();
        assert_eq!(data.total, 5);
        assert_eq!(data.passed, 4);
        assert_eq!(data.failed, 1);
        assert_eq!(data.duration_ms, None);
    }

    #[test]
    fn test_vitest_parser_with_nested_json() {
        let input = r#"prefix text
{"numTotalTests": 2, "numPassedTests": 2, "numFailedTests": 0, "numPendingTests": 0, "testResults": [{"name": "test.js", "assertionResults": [{"fullName": "nested test", "status": "passed", "failureMessages": []}]}], "startTime": 1000}
"#;
        let result = VitestParser::parse(input);
        assert_eq!(result.tier(), 1, "Should succeed with Tier 1 (full parse)");
        assert!(result.is_ok());

        let data = result.unwrap();
        assert_eq!(data.total, 2);
        assert_eq!(data.passed, 2);
    }

    #[test]
    fn test_vitest_json_report_goes_to_the_file_rtk_reads() {
        let report = Path::new("/tmp/rtk-vitest/report.json");
        let invocation = vitest_invocation(&args(&["src/a.test.ts"]), Some(report));

        assert!(!invocation.passthrough);
        assert_eq!(
            invocation.args,
            args(&[
                "run",
                "--reporter=json",
                "--outputFile.json=/tmp/rtk-vitest/report.json",
                "src/a.test.ts",
            ])
        );
        assert_eq!(invocation.report_file.as_deref(), Some(report));
    }

    #[test]
    fn test_vitest_user_output_file_keeps_their_reporter_output() {
        let invocation = vitest_invocation(
            &args(&["--outputFile=out.json", "src/a.test.ts"]),
            Some(Path::new("/tmp/rtk-vitest/report.json")),
        );

        assert!(invocation.passthrough);
        assert_eq!(
            invocation.args,
            args(&["run", "--outputFile=out.json", "src/a.test.ts"])
        );
        assert_eq!(invocation.report_file, None);
    }

    #[test]
    fn test_vitest_parses_report_file_not_the_written_to_notice() {
        // Vitest 5 sends the JSON report to a file; stdout carries only this notice.
        let stdout = "JSON report written to /tmp/rtk-vitest/report.json\n";
        let report = r#"{"numTotalTests":2,"numPassedTests":1,"numFailedTests":1,"numPendingTests":0,"testResults":[{"name":"/p/src/a.test.ts","assertionResults":[{"fullName":"a passes","status":"passed","failureMessages":[]},{"fullName":"a fails","status":"failed","failureMessages":["AssertionError: expected 1 to be 2"]}]}]}"#;

        let json = report_or_stdout(Some(report), stdout);
        let filtered = format_test_output("vitest", json, stdout, false, 0);

        assert!(
            filtered.text.contains("PASS (1) FAIL (1)"),
            "{}",
            filtered.text
        );
        assert!(filtered.text.contains("expected 1 to be 2"));
    }

    #[test]
    fn test_vitest_file_that_fails_to_import_reports_its_error() {
        // Vitest 5.0.5: broken.test.ts imports a module that does not exist,
        // fail.test.ts has one failing assertion.
        let report = r#"{"numTotalTestSuites":2,"numPassedTestSuites":0,"numFailedTestSuites":2,"numPendingTestSuites":0,"numTotalTests":1,"numPassedTests":0,"numFailedTests":1,"numPendingTests":0,"numTodoTests":0,"success":false,"testResults":[{"assertionResults":[],"startTime":1793529349127,"endTime":1793529349127,"status":"failed","message":"Cannot find module './missing' imported from /home/user/proj/broken.test.ts","name":"/home/user/proj/broken.test.ts"},{"assertionResults":[{"ancestorTitles":[],"fullName":"adds","status":"failed","title":"adds","failureMessages":["AssertionError: expected 2 to be 3 // Object.is equality"]}],"startTime":1793529349127,"endTime":1793529349127,"status":"failed","message":"","name":"/home/user/proj/fail.test.ts"}]}"#;

        let filtered = format_test_output("vitest", report, "", false, 0);

        assert!(filtered.text.contains("FAIL (2)"), "{}", filtered.text);
        assert!(
            filtered.text.contains("Cannot find module './missing'"),
            "{}",
            filtered.text
        );
        assert!(filtered.text.contains("expected 2 to be 3"));
    }

    #[test]
    fn test_vitest_raw_output_includes_the_report_file() {
        let notice = "JSON report written to /tmp/rtk-vitest/report.json\n";
        let report = r#"{"numTotalTests":1}"#;

        let raw = raw_with_report(notice, Some(report));

        assert!(raw.starts_with(notice));
        assert!(raw.contains(report));
        assert_eq!(raw_with_report(notice, None), notice);
    }

    #[test]
    fn test_vitest_missing_report_falls_back_to_stdout() {
        assert_eq!(report_or_stdout(None, "{}"), "{}");
        assert_eq!(report_or_stdout(Some("  \n"), "{}"), "{}");
    }

    #[test]
    fn test_vitest_unparsable_run_shows_stderr_too() {
        // A run that dies before reporting (bad import, config error) explains
        // itself on stderr; the passthrough must not drop it.
        let combined = "Error: Failed to load url ./missing (resolved id: ./missing)\n";

        let filtered = format_test_output("vitest", "", combined, false, 0);

        assert!(
            filtered.text.contains("Failed to load url"),
            "{}",
            filtered.text
        );
    }

    #[test]
    fn test_vitest_effective_args_inject_json_reporter_by_default() {
        let effective = vitest_invocation(&args(&["run", "constants.test.ts", "--watch"]), None);

        assert!(!effective.passthrough);
        assert_eq!(
            effective.args,
            args(&["run", "--reporter=json", "constants.test.ts"])
        );
    }

    #[test]
    fn test_vitest_effective_args_preserve_explicit_reporter_equals() {
        let effective =
            vitest_invocation(&args(&["constants.test.ts", "--reporter=verbose"]), None);

        assert!(effective.passthrough);
        assert_eq!(
            effective.args,
            args(&["run", "constants.test.ts", "--reporter=verbose"])
        );
    }

    #[test]
    fn test_vitest_effective_args_preserve_explicit_reporter_value() {
        let effective = vitest_invocation(
            &args(&["run", "constants.test.ts", "--reporter", "verbose"]),
            None,
        );

        assert!(effective.passthrough);
        assert_eq!(
            effective.args,
            args(&["run", "constants.test.ts", "--reporter", "verbose"])
        );
    }

    #[test]
    fn test_vitest_explicit_reporter_keeps_verbose_output() {
        let output = r#"
 ✓ constants/publicPaths.test.ts > public paths > keeps docs path
 ✓ constants/publicPaths.test.ts > public paths > keeps app path

 Test Files  1 passed (1)
      Tests  2 passed (2)
   Duration  450ms
"#;

        let filtered = format_test_output("vitest", output, output, true, 0);

        assert!(filtered.text.contains("keeps docs path"));
        assert!(filtered.text.contains("keeps app path"));
        assert!(filtered.text.contains("Tests  2 passed"));
        assert!(!filtered.truncated);
    }

    #[test]
    fn test_vitest_explicit_reporter_truncated_output_adds_recovery_hint() {
        let output = format!(
            "{}\n Test Files  1 passed (1)\n      Tests  80 passed (80)\n",
            " ✓ constants/publicPaths.test.ts > public paths > keeps verbose case\n".repeat(80)
        );

        let filtered = format_passthrough_output_with_limit(&output, 200);
        let rendered = render_test_output_with_hints(
            &filtered,
            &output,
            "vitest_run",
            0,
            |raw, label| {
                assert_eq!(raw, output);
                assert_eq!(label, "vitest_run");
                Some("[full output: /tmp/vitest_run.log]".to_string())
            },
            |_, _, _| Some("[full output: wrong-path.log]".to_string()),
        );

        assert!(filtered.truncated);
        assert!(rendered.contains("[RTK:PASSTHROUGH] Output truncated"));
        assert!(rendered.contains("[full output: /tmp/vitest_run.log]"));
        assert!(!rendered.contains("wrong-path.log"));
    }

    /// The hint goes on the line after the report, reusing the report's own terminator,
    /// and stands alone when there is no report.
    #[test]
    fn test_vitest_hint_joins_without_a_blank_line() {
        let render = |text: &str| {
            render_test_output_with_hints(
                &FormattedTestOutput::new(text.to_string()),
                "raw",
                "vitest_run",
                1,
                |_, _| None,
                |_, _, _| Some("[full output: vitest_run.log]".to_string()),
            )
        };

        assert_eq!(
            render("Tests: 1 failed\n"),
            "Tests: 1 failed\n[full output: vitest_run.log]"
        );
        assert_eq!(render(""), "[full output: vitest_run.log]");
    }
}
