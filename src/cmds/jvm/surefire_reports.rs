//! Parses Maven Surefire/Failsafe XML test reports from
//! `target/surefire-reports/TEST-*.xml` and `target/failsafe-reports/*.xml`.
//! Uses quick-xml streaming parser. Time-gated by `started_at` to skip stale
//! reports from previous runs.

use crate::cmds::jvm::stack_trace;
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;
use std::time::SystemTime;

pub const DEFAULT_STACK_TRACE_LINES: usize = 50;
pub const DEFAULT_PER_TEST_OUTPUT_LIMIT: usize = 2000;
/// Line budget for captured output, on top of the char limit. A Spring context
/// failure dumps its `CONDITIONS EVALUATION REPORT` into `system-out`: short
/// lines, so the char cap alone still lets ~70 of them through.
pub const DEFAULT_PER_TEST_OUTPUT_LINES: usize = 12;
const DEFAULT_TOTAL_OUTPUT_LIMIT: usize = 10_000;

#[derive(Debug, Default, PartialEq)]
pub struct TestSummary {
    pub run: u32,
    pub failures: u32,
    pub errors: u32,
    pub skipped: u32,
}

impl TestSummary {
    pub(crate) fn add(&mut self, other: &Self) {
        self.run += other.run;
        self.failures += other.failures;
        self.errors += other.errors;
        self.skipped += other.skipped;
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum FailureKind {
    Failure,
    Error,
}

#[derive(Debug, PartialEq)]
pub struct TestFailure {
    pub test_class: String,
    pub test_method: String,
    pub kind: FailureKind,
    pub message: Option<String>,
    pub failure_type: Option<String>,
    pub stack_trace: Option<String>,
    pub test_output: Option<String>,
}

#[derive(Debug, PartialEq, Clone)]
pub struct SuiteStat {
    pub class_name: String,
    pub tests: u32,
    pub skipped: u32,
    pub time_secs: f64,
    pub module: Option<String>,
}

#[derive(Debug, PartialEq, Clone)]
pub struct SkippedTest {
    pub class: String,
    pub method: String,
    pub reason: Option<String>,
}

#[derive(Debug, Default, PartialEq)]
pub struct SurefireResult {
    pub summary: TestSummary,
    pub failures: Vec<TestFailure>,
    pub suites: Vec<SuiteStat>,
    pub skipped_tests: Vec<SkippedTest>,
    pub files_read: usize,
    pub files_skipped_stale: usize,
    pub files_malformed: usize,
}

fn local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|b| *b == b':').next().unwrap_or(name)
}

fn extract_attr(
    reader: &Reader<&[u8]>,
    start: &BytesStart<'_>,
    key: &[u8],
) -> Option<String> {
    for attr in start.attributes().flatten() {
        if local_name(attr.key.as_ref()) != key {
            continue;
        }
        if let Ok(value) = attr.decode_and_unescape_value(reader.decoder()) {
            return Some(value.into_owned());
        }
    }
    None
}

fn parse_u32_attr(reader: &Reader<&[u8]>, start: &BytesStart<'_>, key: &[u8]) -> u32 {
    extract_attr(reader, start, key)
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0)
}

/// Parse a single Surefire XML testsuite string into a partial result.
/// `app_packages` is passed to `stack_trace::process` for frame classification.
///
/// Returns `None` only if the XML is completely malformed; otherwise a
/// best-effort result is returned.
pub(crate) fn parse_content(xml: &str, app_packages: &[String]) -> Option<SurefireResult> {
    #[derive(Clone, Copy, PartialEq)]
    enum CaptureField {
        StackTrace,
        SystemOut,
        SystemErr,
    }

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();

    let mut result = SurefireResult::default();
    let mut saw_testsuite = false;
    let mut current_class: Option<String> = None;
    let mut current_method: Option<String> = None;
    let mut current_has_failure = false;

    let mut pending_message: Option<String> = None;
    let mut pending_type: Option<String> = None;
    let mut pending_kind: Option<FailureKind> = None;
    let mut stack_buf = String::new();
    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();
    let mut capture: Option<CaptureField> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                match local_name(e.name().as_ref()) {
                    b"testsuite" => {
                        saw_testsuite = true;
                        let file_summary = TestSummary {
                            run: parse_u32_attr(&reader, &e, b"tests"),
                            failures: parse_u32_attr(&reader, &e, b"failures"),
                            errors: parse_u32_attr(&reader, &e, b"errors"),
                            skipped: parse_u32_attr(&reader, &e, b"skipped"),
                        };
                        result.suites.push(SuiteStat {
                            class_name: extract_attr(&reader, &e, b"name").unwrap_or_default(),
                            tests: file_summary.run,
                            skipped: file_summary.skipped,
                            time_secs: extract_attr(&reader, &e, b"time")
                                .and_then(|v| v.parse::<f64>().ok())
                                .unwrap_or(0.0),
                            module: None,
                        });
                        result.summary.add(&file_summary);
                    }
                    b"testcase" => {
                        current_class = extract_attr(&reader, &e, b"classname");
                        current_method = extract_attr(&reader, &e, b"name");
                        current_has_failure = false;
                    }
                    b"failure" | b"error" => {
                        let kind = if local_name(e.name().as_ref()) == b"failure" {
                            FailureKind::Failure
                        } else {
                            FailureKind::Error
                        };
                        pending_message = extract_attr(&reader, &e, b"message");
                        pending_type = extract_attr(&reader, &e, b"type");
                        pending_kind = Some(kind);
                        stack_buf.clear();
                        capture = Some(CaptureField::StackTrace);
                        current_has_failure = true;
                    }
                    b"system-out" if current_has_failure => {
                        stdout_buf.clear();
                        capture = Some(CaptureField::SystemOut);
                    }
                    b"system-err" if current_has_failure => {
                        stderr_buf.clear();
                        capture = Some(CaptureField::SystemErr);
                    }
                    b"skipped" => {
                        result.skipped_tests.push(SkippedTest {
                            class: current_class.clone().unwrap_or_default(),
                            method: current_method.clone().unwrap_or_default(),
                            reason: extract_attr(&reader, &e, b"message").filter(|s| !s.is_empty()),
                        });
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if let Some(field) = capture {
                    if let Ok(text) = t.unescape() {
                        match field {
                            CaptureField::StackTrace => stack_buf.push_str(&text),
                            CaptureField::SystemOut => stdout_buf.push_str(&text),
                            CaptureField::SystemErr => stderr_buf.push_str(&text),
                        }
                    }
                }
            }
            // Surefire wraps a stack trace in `<![CDATA[…]]>` whenever it
            // contains characters that would otherwise need escaping — which
            // is most real traces (generics, `<init>`, `&`). quick-xml reports
            // those as `CData`, never as `Text`, so without this arm every
            // CDATA-wrapped report silently loses its trace and the failure is
            // rendered from the `message` attribute alone.
            Ok(Event::CData(t)) => {
                if let Some(field) = capture {
                    let text = String::from_utf8_lossy(&t);
                    match field {
                        CaptureField::StackTrace => stack_buf.push_str(&text),
                        CaptureField::SystemOut => stdout_buf.push_str(&text),
                        CaptureField::SystemErr => stderr_buf.push_str(&text),
                    }
                }
            }
            Ok(Event::End(e)) => {
                match local_name(e.name().as_ref()) {
                    b"failure" | b"error" => {
                        let processed = stack_trace::process(
                            stack_buf.trim(),
                            app_packages,
                            DEFAULT_STACK_TRACE_LINES,
                        );
                        result.failures.push(TestFailure {
                            test_class: current_class.clone().unwrap_or_default(),
                            test_method: current_method.clone().unwrap_or_default(),
                            kind: pending_kind.take().unwrap_or(FailureKind::Failure),
                            message: pending_message
                                .take()
                                .filter(|s| !s.is_empty())
                                .map(|s| stack_trace::truncate_header(&s)),
                            failure_type: pending_type.take().filter(|s| !s.is_empty()),
                            stack_trace: processed,
                            test_output: None,
                        });
                        capture = None;
                    }
                    b"system-out" | b"system-err" => {
                        capture = None;
                    }
                    b"testcase" => {
                        let combined = combine_test_output(
                            &stdout_buf,
                            &stderr_buf,
                            DEFAULT_PER_TEST_OUTPUT_LIMIT,
                        );
                        stdout_buf.clear();
                        stderr_buf.clear();
                        if let Some(combined) = combined {
                            if let Some(last) = result.failures.last_mut() {
                                if current_class.as_deref() == Some(last.test_class.as_str())
                                    && current_method.as_deref()
                                        == Some(last.test_method.as_str())
                                {
                                    last.test_output = Some(combined);
                                }
                            }
                        }
                        current_class = None;
                        current_method = None;
                        current_has_failure = false;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }

    if !saw_testsuite {
        return None;
    }

    Some(result)
}

fn combine_test_output(stdout: &str, stderr: &str, per_test_limit: usize) -> Option<String> {
    // Drop per-JVM-launch warning boilerplate (byte-buddy/Mockito agent
    // banners) first: with the 12-line tail budget they would otherwise
    // crowd out the content the agent actually needs.
    let stdout = clean_captured(&drop_jvm_runtime_noise(stdout));
    let stderr = clean_captured(&drop_jvm_runtime_noise(stderr));
    let stdout = stdout.trim();
    let stderr = stderr.trim();
    if stdout.is_empty() && stderr.is_empty() {
        return None;
    }
    let mut combined = String::new();
    if !stdout.is_empty() {
        combined.push_str(stdout);
    }
    if !stderr.is_empty() {
        if !combined.is_empty() {
            combined.push_str("\n[STDERR]\n");
        } else {
            combined.push_str("[STDERR]\n");
        }
        combined.push_str(stderr);
    }
    Some(truncate_test_output(&collapse_blank_runs(&combined), per_test_limit))
}

/// Console colour codes as they survive into a surefire report. Surefire
/// escapes the ESC byte into the literal text `&amp#27;` — its own form, note
/// the missing `;` after `amp` — so a Spring Boot log line arrives as
/// `&amp#27;[35m2026-07-31 12:07:37.523&amp#27;[0;39m …`: nine bytes of noise
/// per code, ~10 codes per line. Real ESC bytes and the well-formed `&#27;`
/// spelling are matched too, since which one appears depends on the surefire
/// version and on whether the report was written through CDATA.
static CAPTURED_ANSI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:\x1b|&amp#27;|&#27;)\[[0-9;]*[A-Za-z]").unwrap());

/// Application log lines at the two lowest levels inside captured output.
/// Two console layouts show up in practice: Spring Boot's default
/// (`2026-07-31 12:07:37.523 DEBUG [main] c.d.Foo : msg`) and Logback's stock
/// pattern (`11:54:21.931 [main] DEBUG c.d.Foo - msg`), so the optional
/// thread field is allowed on either side of the level. Only TRACE/DEBUG are
/// dropped: with a 12-line budget an INFO or WARN line is routinely the one
/// that explains the failure (`Cannot find requested user …`).
static CAPTURED_DEBUG_LINE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(?:\d{4}-\d{2}-\d{2}[ T])?\d{2}:\d{2}:\d{2}[.,]\d{1,9}\s+(?:\[[^\]]*\]\s+)?(?:TRACE|DEBUG)\b",
    )
    .unwrap()
});

/// Banner Spring's `ConditionEvaluationReportLogger` prints when a context
/// fails to load. Everything from it on is the report: `Positive matches`,
/// `Negative matches`, `Exclusions`, `Unconditional classes` — either bare
/// `None`s or thousands of indented auto-configuration class names, never a
/// clue about why the context failed. It is emitted on the failure path, so
/// it is terminal within the block; cutting at the banner puts the tail
/// budget back on the application's own log lines above it.
const CONDITIONS_REPORT_BANNER: &str = "CONDITIONS EVALUATION REPORT";

/// Spring Boot's own words for "the application context is up":
/// `Started SettlementServiceIntegrationTest in 2.206 seconds (process running
/// for 9.61)`. Everything above it is context startup — the banner,
/// Testcontainers, Flyway, Hikari, the Hibernate connection-pool dump —
/// printed identically for every test class and never about the test that
/// failed. A real report spent all 128 lines of its `<system-out>` on it. The
/// marker is self-delimiting in the direction that matters: a context that
/// *fails* to start never prints it, and there the startup log IS the
/// diagnostic, so nothing is cut.
static SPRING_STARTUP_COMPLETE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Started \S+ in [\d.]+ seconds \(process running for [\d.]+\)").unwrap()
});

/// Chatter the TestContext framework prints once per test class before the
/// context is even looked up ("Could not detect default configuration classes
/// for test class …", "Found @SpringBootConfiguration …"). It appears with no
/// startup marker after it when the context is reused across classes, so the
/// marker cut alone does not reach it.
const CAPTURED_BOOTSTRAP_NOISE: &[&str] = &[
    "AnnotationConfigContextLoaderUtils",
    "SpringBootTestContextBootstrapper",
    "DefaultTestContextBootstrapper",
];

/// Strip console colour codes, cut Spring's conditions report, and drop
/// TRACE/DEBUG log lines from a captured output block, so the tail budget is
/// spent on lines that carry meaning. ANSI goes first: the level field is
/// only findable once the colour codes wrapped around it are gone.
fn clean_captured(text: &str) -> String {
    let text = CAPTURED_ANSI_RE.replace_all(text, "");
    let mut kept: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.trim() == CONDITIONS_REPORT_BANNER {
            // The `====` rule printed directly above the banner belongs to it.
            if kept.last().is_some_and(|l| is_banner_rule(l)) {
                kept.pop();
            }
            break;
        }
        if SPRING_STARTUP_COMPLETE_RE.is_match(line) {
            // Startup finished here: everything collected so far was getting
            // the application up, marker line included.
            kept.clear();
            continue;
        }
        if CAPTURED_DEBUG_LINE_RE.is_match(line)
            || CAPTURED_BOOTSTRAP_NOISE.iter().any(|p| line.contains(p))
        {
            continue;
        }
        kept.push(line);
    }
    kept.join("\n")
}

/// A `====…` rule line, the separator Spring prints around the banner.
fn is_banner_rule(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 4 && t.bytes().all(|b| b == b'=')
}

/// Remove bare-text JVM agent/self-attach warning lines from a captured
/// output block. Returns the input unchanged (no reallocation churn beyond
/// the single rebuild) when nothing matches.
fn drop_jvm_runtime_noise(text: &str) -> String {
    if !text
        .lines()
        .any(stack_trace::is_jvm_runtime_noise)
    {
        return text.to_string();
    }
    text.lines()
        .filter(|l| !stack_trace::is_jvm_runtime_noise(l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Collapse runs of blank lines to a single one. Spring's
/// `CONDITIONS EVALUATION REPORT`, dumped into `system-out` on every context
/// load failure, is ~80 lines of which most are blank — the char cap alone
/// lets it through as lines the agent still pays for.
fn keep_last_lines(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines {
        return text.to_string();
    }
    let dropped = lines.len() - max_lines;
    let mut out = format!("... ({dropped} lines truncated)\n");
    out.push_str(&lines[dropped..].join("\n"));
    out
}

fn collapse_blank_runs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            if blank_run {
                continue;
            }
            blank_run = true;
        } else {
            blank_run = false;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.truncate(out.trim_end().len());
    out
}

fn truncate_test_output(output: &str, max_chars: usize) -> String {
    let output = &keep_last_lines(output, DEFAULT_PER_TEST_OUTPUT_LINES);
    let char_count = output.chars().count();
    if char_count <= max_chars {
        return output.to_string();
    }
    let skip = char_count - max_chars;
    let tail: String = output.chars().skip(skip).collect();
    format!("... ({skip} chars truncated)\n{tail}")
}

/// Scan a directory for `TEST-*.xml` files and merge their parsed results.
///
/// - Files whose `mtime < since` are skipped and counted in `files_skipped_stale`.
/// - Files that parse to `None` (malformed) count in `files_malformed`.
/// - Returns `None` only if the directory does not exist or is empty.
pub fn parse_dir(
    dir: &Path,
    since: Option<SystemTime>,
    app_packages: &[String],
) -> Option<SurefireResult> {
    if !dir.exists() || !dir.is_dir() {
        return None;
    }

    let entries = std::fs::read_dir(dir).ok()?;
    let mut aggregate = SurefireResult::default();
    let mut any_candidate = false;

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.starts_with("TEST-") || !name.ends_with(".xml") {
            continue;
        }
        any_candidate = true;

        if let Some(since) = since {
            let modified = entry.metadata().ok().and_then(|m| m.modified().ok());
            match modified {
                Some(m) if m >= since => {}
                Some(_) => {
                    aggregate.files_skipped_stale += 1;
                    continue;
                }
                None => {
                    aggregate.files_skipped_stale += 1;
                    continue;
                }
            }
        }

        let Ok(content) = std::fs::read_to_string(&path) else {
            aggregate.files_malformed += 1;
            eprintln!("rtk mvn: skipping unreadable {}", name);
            continue;
        };

        match parse_content(&content, app_packages) {
            Some(file_result) => {
                aggregate.files_read += 1;
                aggregate.summary.add(&file_result.summary);
                aggregate.failures.extend(file_result.failures);
                aggregate.suites.extend(file_result.suites);
                aggregate.skipped_tests.extend(file_result.skipped_tests);
            }
            None => {
                aggregate.files_malformed += 1;
                eprintln!("rtk mvn: skipping malformed {}", name);
            }
        }
    }

    if !any_candidate {
        return None;
    }

    apply_total_output_limit(&mut aggregate.failures, DEFAULT_TOTAL_OUTPUT_LIMIT);
    Some(aggregate)
}

fn apply_total_output_limit(failures: &mut [TestFailure], total_limit: usize) {
    let mut budget = total_limit;
    let mut exhausted = false;
    for failure in failures.iter_mut() {
        if exhausted {
            failure.test_output = None;
            continue;
        }
        if let Some(out) = &failure.test_output {
            let len = out.chars().count();
            if len > budget {
                failure.test_output = None;
                exhausted = true;
            } else {
                budget -= len;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn copy_fixture(
        tmp: &tempfile::TempDir,
        fixture_name: &str,
        mtime: Option<SystemTime>,
    ) -> std::path::PathBuf {
        let src = std::path::Path::new("tests/fixtures/java/surefire-reports").join(fixture_name);
        let dst = tmp.path().join(fixture_name);
        std::fs::copy(&src, &dst).expect("copy fixture");
        if let Some(mtime) = mtime {
            filetime::set_file_mtime(&dst, filetime::FileTime::from_system_time(mtime))
                .expect("set mtime");
        }
        dst
    }

    #[test]
    fn parse_dir_missing_returns_none() {
        assert!(super::parse_dir(
            std::path::Path::new("/definitely/does/not/exist/rtk-test"),
            None,
            &[]
        )
        .is_none());
    }

    #[test]
    fn parse_dir_empty_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(super::parse_dir(tmp.path(), None, &[]).is_none());
    }

    #[test]
    fn parse_dir_ignores_non_test_prefix_files() {
        let tmp = tempfile::tempdir().unwrap();
        copy_fixture(&tmp, "TEST-com.example.PassingTest.xml", None);
        std::fs::write(tmp.path().join("summary.xml"), "<x/>").unwrap();
        std::fs::write(tmp.path().join("other.txt"), "hi").unwrap();

        let result = super::parse_dir(tmp.path(), None, &[]).expect("parses");
        assert_eq!(result.files_read, 1);
    }

    #[test]
    fn parse_dir_aggregates_multi_file_counts() {
        let tmp = tempfile::tempdir().unwrap();
        copy_fixture(&tmp, "TEST-com.example.PassingTest.xml", None);
        copy_fixture(&tmp, "TEST-com.example.FailingTest.xml", None);
        copy_fixture(&tmp, "TEST-com.example.SkippedTest.xml", None);

        let result = super::parse_dir(tmp.path(), None, &[]).expect("parses");
        assert_eq!(result.files_read, 3);
        assert!(result.summary.run >= 3);
        assert!(result.summary.failures >= 2);
        assert!(result.summary.skipped >= 1);
    }

    #[test]
    fn parse_dir_time_gate_skips_stale_files() {
        let tmp = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let stale = now - Duration::from_secs(60 * 60); // 1h ago
        let fresh = now + Duration::from_millis(50);

        copy_fixture(&tmp, "TEST-com.example.PassingTest.xml", Some(stale));
        copy_fixture(&tmp, "TEST-com.example.FailingTest.xml", Some(fresh));

        let since = now;
        let result = super::parse_dir(tmp.path(), Some(since), &[]).expect("parses");
        assert_eq!(result.files_read, 1, "only the fresh file counts");
        assert_eq!(result.files_skipped_stale, 1);
        assert_eq!(result.summary.failures, 2, "from FailingTest only");
    }

    #[test]
    fn parse_dir_malformed_counts_but_continues() {
        let tmp = tempfile::tempdir().unwrap();
        copy_fixture(&tmp, "TEST-com.example.PassingTest.xml", None);
        std::fs::write(
            tmp.path().join("TEST-com.example.Broken.xml"),
            "<not-xml>>>>",
        )
        .unwrap();

        let result = super::parse_dir(tmp.path(), None, &[]).expect("parses");
        assert_eq!(result.files_read, 1);
        assert_eq!(result.files_malformed, 1);
    }

    #[test]
    fn parse_content_single_passing() {
        let xml = include_str!(
            "../../../tests/fixtures/java/surefire-reports/TEST-com.example.PassingTest.xml"
        );
        let result = parse_content(xml, &[]).expect("passing testsuite parses");
        assert!(result.summary.run >= 1);
        assert_eq!(result.summary.failures, 0);
        assert_eq!(result.summary.errors, 0);
        assert!(result.failures.is_empty());
    }

    #[test]
    fn parse_content_cdata_stack_trace_extracted() {
        // Real report from a Spring context load failure. Surefire wraps the
        // trace in CDATA, which quick-xml reports as `CData` and never as
        // `Text` — without that arm the trace was silently dropped and the
        // failure rendered from the `message` attribute alone. Every real
        // report on disk uses CDATA; the hand-written fixtures do not, which
        // is why the unit tests stayed green while the feature was dead.
        let xml = include_str!(
            "../../../tests/fixtures/java/surefire-reports/TEST-com.example.selfie.domain.CandidateServiceSpec.xml"
        );
        let result = parse_content(xml, &["com.example".to_string()])
            .expect("context failure testsuite parses");
        assert_eq!(result.failures.len(), 1);
        let f = &result.failures[0];

        let trace = f
            .stack_trace
            .as_deref()
            .expect("CDATA stack trace must be captured");
        assert!(
            trace.contains("Caused by:"),
            "the Caused by chain must survive, got:\n{trace}"
        );
        assert!(
            trace.contains("could not resolve package for GenericFeatureAwareVersion"),
            "the ROOT cause is the line agents grep the tee log for, got:\n{trace}"
        );
        assert!(
            trace.lines().count() <= DEFAULT_STACK_TRACE_LINES,
            "framework collapsing must keep the trace short, got {} lines",
            trace.lines().count()
        );
    }

    #[test]
    fn parse_content_cdata_captured_output_line_capped() {
        // Enabling CDATA also switches on `system-out`, where Spring dumps its
        // CONDITIONS EVALUATION REPORT: short lines, so the char cap alone let
        // ~70 of them through.
        let xml = include_str!(
            "../../../tests/fixtures/java/surefire-reports/TEST-com.example.selfie.domain.CandidateServiceSpec.xml"
        );
        let result = parse_content(xml, &["com.example".to_string()]).expect("parses");
        let output = result.failures[0]
            .test_output
            .as_deref()
            .expect("captured output present");
        assert!(
            output.lines().count() <= DEFAULT_PER_TEST_OUTPUT_LINES + 1,
            "captured output must be line-capped, got {} lines:\n{output}",
            output.lines().count()
        );
    }

    #[test]
    fn captured_output_strips_surefire_escaped_ansi_and_debug() {
        // Real `<system-out>` bytes from a skiller run (services module,
        // 2026-07-31): Spring Boot logs to a colour console, and surefire
        // escapes each ESC byte as the literal text `&amp#27;`. Rendered
        // verbatim, one 4-test class spent 4282 of its 5432 output chars on
        // this — DEBUG chatter wrapped in nine-byte colour codes — while the
        // two lines that explain the failure sat just above the tail window.
        let xml = include_str!(
            "../../../tests/fixtures/java/surefire-reports/TEST-com.example.answers.ExamFinishedReceiverTest.xml"
        );
        let result = parse_content(xml, &["com.example".to_string()]).expect("parses");
        let output = result.failures[0]
            .test_output
            .as_deref()
            .expect("captured output present");

        assert!(
            !output.contains("&amp#27;") && !output.contains('\u{1b}'),
            "colour codes must not reach the agent:\n{output}"
        );
        assert!(
            !output.contains("DEBUG"),
            "DEBUG chatter must not spend the tail budget:\n{output}"
        );
        assert!(
            output.contains("Ignoring answers from eval for TOKN-1234-ABCD"),
            "the WARN line that explains the failure must survive:\n{output}"
        );
        assert!(
            output.len() < 700,
            "captured output should collapse to a few informative lines, got {} chars:\n{output}",
            output.len()
        );
    }

    #[test]
    fn captured_output_cuts_application_startup_chatter() {
        // Real report (projects/rozrachunki-app, 2026-07-31): 128 lines of
        // `<system-out>`, all of it Testcontainers + Spring Boot + Flyway +
        // Hikari + Hibernate startup, ending on Spring's own "Started … in N
        // seconds (process running for M)". The whole 12-line tail budget went
        // to it — 1622 of that run's 2474 chars — and none of it says anything
        // about the assertion that failed.
        let xml = include_str!(
            "../../../tests/fixtures/java/surefire-reports/TEST-com.example.settlements.service.SettlementServiceIntegrationTest.xml"
        );
        let result = parse_content(xml, &["com.example".to_string()]).expect("parses");
        let failure = &result.failures[0];

        assert!(
            failure
                .test_output
                .as_deref()
                .map(|o| o.trim().is_empty())
                .unwrap_or(true),
            "a block that is nothing but startup chatter must not be rendered: {:?}",
            failure.test_output
        );
        assert!(
            failure
                .message
                .as_deref()
                .is_some_and(|m| m.contains("expected: 156.01")),
            "the assertion itself must survive: {:?}",
            failure.message
        );
    }

    #[test]
    fn captured_output_keeps_what_the_test_logged_after_startup() {
        // The cut is anchored on Spring's own startup-complete marker, so
        // whatever the test logs afterwards — the part that can explain a
        // failure — is exactly what is left.
        let stdout = "\
2026-07-31T23:17:33.564+02:00  INFO 1344785 --- [           main] c.e.s.SettlementTest     : Starting SettlementTest using Java 25.0.2
2026-07-31T23:17:34.298+02:00  INFO 1344785 --- [           main] com.zaxxer.hikari.HikariDataSource       : HikariPool-1 - Starting...
2026-07-31T23:17:35.682+02:00  INFO 1344785 --- [           main] c.e.s.SettlementTest     : Started SettlementTest in 2.206 seconds (process running for 9.61)
2026-07-31T23:17:36.100+02:00  WARN 1344785 --- [           main] org.hibernate.orm.jdbc.error             : ERROR: new row for relation \"invoice\" violates check constraint";
        let cleaned = clean_captured(stdout);
        assert!(
            !cleaned.contains("HikariPool-1") && !cleaned.contains("Started SettlementTest"),
            "startup must be cut up to and including the marker:\n{cleaned}"
        );
        assert!(
            cleaned.contains("violates check constraint"),
            "what the test logged after startup must survive:\n{cleaned}"
        );
    }

    #[test]
    fn captured_output_keeps_startup_when_the_context_never_started() {
        // No startup-complete marker means the context failed while starting —
        // there the startup log IS the diagnostic and nothing may be cut.
        let stdout = "\
2026-07-31T23:17:33.564+02:00  INFO 1344785 --- [           main] c.e.s.SettlementTest     : Starting SettlementTest using Java 25.0.2
2026-07-31T23:17:34.298+02:00  INFO 1344785 --- [           main] com.zaxxer.hikari.HikariDataSource       : HikariPool-1 - Starting...
2026-07-31T23:17:34.900+02:00 ERROR 1344785 --- [           main] o.s.boot.SpringApplication               : Application run failed";
        let cleaned = clean_captured(stdout);
        assert!(
            cleaned.contains("HikariPool-1") && cleaned.contains("Application run failed"),
            "a context that never started keeps its whole log:\n{cleaned}"
        );
    }

    #[test]
    fn captured_output_drops_test_context_bootstrap_chatter() {
        // Printed per test class by the TestContext framework, never about the
        // test: four lines / ~1000 chars ahead of the two Hibernate WARNs that
        // actually explained the failure (projects/najemnik, 2026-08-01).
        let stdout = "\
2026-08-01T12:16:11.425+02:00  INFO 224826 --- [           main] t.c.s.AnnotationConfigContextLoaderUtils : Could not detect default configuration classes for test class [com.example.app.InvoiceServiceTest]: InvoiceServiceTest does not declare any static, non-private, non-final, nested classes annotated with @Configuration.
2026-08-01T12:16:11.429+02:00  INFO 224826 --- [           main] .b.t.c.SpringBootTestContextBootstrapper : Found @SpringBootConfiguration com.example.app.ExampleApplication for test class com.example.app.InvoiceServiceTest
2026-08-01T12:16:11.436+02:00  WARN 224826 --- [           main] org.hibernate.orm.jdbc.error             : HHH000247: ErrorCode: 0, SQLState: 23514
2026-08-01T12:16:11.436+02:00  WARN 224826 --- [           main] org.hibernate.orm.jdbc.error             : ERROR: new row for relation \"invoice_section\" violates check constraint";
        let cleaned = clean_captured(stdout);
        assert!(
            !cleaned.contains("AnnotationConfigContextLoaderUtils")
                && !cleaned.contains("SpringBootTestContextBootstrapper"),
            "bootstrap chatter must not spend the tail budget:\n{cleaned}"
        );
        assert_eq!(
            cleaned.lines().count(),
            2,
            "the two lines that explain the failure are what is left:\n{cleaned}"
        );
    }

    #[test]
    fn captured_output_cuts_spring_conditions_report() {
        // Two independent real shapes: the selfie report on disk carries a
        // degenerate all-`None` report twice in `<system-err>` (590 chars of
        // pure banner), while an auth integration failure rendered the other
        // extreme — the `Unconditional classes` listing, thousands of indented
        // auto-configuration class names alternating with blank lines, which
        // `collapse_blank_runs` cannot touch because the blanks are not
        // consecutive. Neither says why the context failed.
        let stdout = "\
11:54:22.212 [main] WARN  c.d.a.u.p.PasswordResetService - Cannot find requested user
============================
CONDITIONS EVALUATION REPORT
============================


Positive matches:
-----------------

    None


Unconditional classes:
----------------------

    org.springframework.boot.autoconfigure.info.ProjectInfoAutoConfiguration

    org.springframework.boot.autoconfigure.availability.ApplicationAvailabilityAutoConfiguration";
        let out = super::combine_test_output(stdout, "", 2000).expect("captured output");
        assert_eq!(out, "11:54:22.212 [main] WARN  c.d.a.u.p.PasswordResetService - Cannot find requested user");
    }

    #[test]
    fn captured_output_is_dropped_when_only_the_conditions_report_remains() {
        // The selfie shape: `<system-err>` holds nothing but the report, so
        // there is no captured output left to show.
        let stderr = "\n\n============================\nCONDITIONS EVALUATION REPORT\n\
                      ============================\n\n\nExclusions:\n-----------\n\n    None\n";
        assert!(super::combine_test_output("", stderr, 2000).is_none());
    }

    #[test]
    fn captured_output_keeps_info_and_warn_lines() {
        // The auth counterpart: Logback's stock pattern puts the thread
        // before the level. INFO/WARN carry the diagnosis (`Cannot find
        // requested user …`) and must never be dropped with the DEBUG noise.
        let stdout = "\
11:54:21.900 [main] DEBUG c.d.auth.user.UserLookupService - loading user cache
11:54:22.211 [main] INFO  c.d.a.u.p.PasswordResetService - Password reset requested. Email=test.user@no-encryptor.example.com
11:54:22.212 [main] WARN  c.d.a.u.p.PasswordResetService - Cannot find requested user test.user@no-encryptor.example.com";
        let out = super::combine_test_output(stdout, "", 2000).expect("captured output");
        assert!(!out.contains("loading user cache"), "DEBUG kept: {out}");
        assert!(out.contains("Password reset requested"), "INFO dropped: {out}");
        assert!(out.contains("Cannot find requested user"), "WARN dropped: {out}");
    }

    #[test]
    fn combine_output_drops_jvm_agent_noise() {
        // byte-buddy/Mockito self-attach banners open every forked test JVM's
        // stderr; with the 12-line tail budget they crowd out real content.
        let stderr = "\
Mockito is currently self-attaching to enable the inline-mock-maker. This will no longer work in future releases of the JDK.
WARNING: A Java agent has been loaded dynamically (/x/.m2/repository/net/bytebuddy/byte-buddy-agent/1.17.8/byte-buddy-agent-1.17.8.jar)
WARNING: If a serviceability tool is in use, please run with -XX:+EnableDynamicAgentLoading to hide this warning
WARNING: If a serviceability tool is not in use, please run with -Djdk.instrument.traceUsage for more information
WARNING: Dynamic loading of agents will be disallowed by default in a future release
real stderr content the agent needs";
        let out = super::combine_test_output("", stderr, 2000).expect("captured output");
        assert!(out.contains("real stderr content"), "real line dropped: {out}");
        assert!(
            !out.contains("Mockito is currently self-attaching"),
            "Mockito banner leaked: {out}"
        );
        assert!(
            !out.contains("Java agent has been loaded"),
            "agent-load warning leaked: {out}"
        );
        assert!(
            !out.contains("serviceability tool"),
            "serviceability hint leaked: {out}"
        );
    }

    #[test]
    fn combine_output_all_noise_yields_none_marker_free_stderr() {
        // If stderr is nothing but agent noise, the [STDERR] marker must not
        // appear for an effectively-empty block.
        let stderr = "\
WARNING: A Java agent has been loaded dynamically (/x/byte-buddy-agent.jar)
WARNING: Dynamic loading of agents will be disallowed by default in a future release";
        let out = super::combine_test_output("stdout line", stderr, 2000).expect("captured output");
        assert!(out.contains("stdout line"));
        assert!(!out.contains("[STDERR]"), "empty STDERR block leaked: {out}");
    }

    #[test]
    fn collapse_blank_runs_keeps_one_separator() {
        let input = "alpha\n\n\n\nbeta\n\ngamma";
        assert_eq!(super::collapse_blank_runs(input), "alpha\n\nbeta\n\ngamma");
    }

    #[test]
    fn parse_content_single_failing_extracts_details() {
        let xml = include_str!(
            "../../../tests/fixtures/java/surefire-reports/TEST-com.example.FailingTest.xml"
        );
        let result = parse_content(xml, &[]).expect("failing testsuite parses");
        assert_eq!(result.summary.failures, 2);
        assert_eq!(result.failures.len(), 2);
        let first = &result.failures[0];
        assert_eq!(first.test_class, "com.example.FailingTest");
        assert!(first.message.as_deref().unwrap_or("").contains("expected"));
        assert!(first.stack_trace.is_some());
        assert_eq!(first.kind, FailureKind::Failure);
    }

    #[test]
    fn parse_content_captures_system_out_err_only_for_failed_tests() {
        let xml = include_str!(
            "../../../tests/fixtures/java/surefire-reports/TEST-com.example.FailingTestWithLogs.xml"
        );
        let result = parse_content(xml, &[]).expect("parses");
        assert_eq!(result.failures.len(), 2);
        let with_both_streams = result
            .failures
            .iter()
            .find(|f| f.test_method == "shouldConnectToDb")
            .expect("shouldConnectToDb present");
        let output = with_both_streams
            .test_output
            .as_deref()
            .expect("test_output captured");
        assert!(output.contains("Initializing connection pool"));
        assert!(output.contains("[STDERR]"));
        assert!(output.contains("Connection refused"));

        let with_stdout_only = result
            .failures
            .iter()
            .find(|f| f.test_method == "shouldProcessData")
            .expect("shouldProcessData present");
        let output = with_stdout_only.test_output.as_deref().unwrap_or("");
        assert!(output.contains("Processing batch"));
        assert!(!output.contains("[STDERR]"));

        // Passing test's <system-out> must NOT be captured
        let passing_system_out_text = "This output belongs to a passing test";
        for failure in &result.failures {
            if let Some(out) = &failure.test_output {
                assert!(
                    !out.contains(passing_system_out_text),
                    "passing-test stdout must not leak into a failure's test_output"
                );
            }
        }
    }

    #[test]
    fn parse_content_error_testsuite_marks_failure_kind_error() {
        let xml = include_str!(
            "../../../tests/fixtures/java/surefire-reports/TEST-com.example.ErrorTest.xml"
        );
        let result = parse_content(xml, &[]).expect("parses");
        assert!(result.failures.iter().any(|f| f.kind == FailureKind::Error));
    }

    #[test]
    fn parse_content_skipped_testsuite_counts_skipped() {
        let xml = include_str!(
            "../../../tests/fixtures/java/surefire-reports/TEST-com.example.SkippedTest.xml"
        );
        let result = parse_content(xml, &[]).expect("parses");
        assert!(result.summary.skipped > 0);
    }

    #[test]
    fn apply_total_output_limit_nulls_out_excess() {
        let mut failures = vec![
            TestFailure {
                test_class: "A".into(),
                test_method: "m1".into(),
                kind: FailureKind::Failure,
                message: None,
                failure_type: None,
                stack_trace: None,
                test_output: Some("a".repeat(4000)),
            },
            TestFailure {
                test_class: "A".into(),
                test_method: "m2".into(),
                kind: FailureKind::Failure,
                message: None,
                failure_type: None,
                stack_trace: None,
                test_output: Some("b".repeat(4000)),
            },
            TestFailure {
                test_class: "A".into(),
                test_method: "m3".into(),
                kind: FailureKind::Failure,
                message: None,
                failure_type: None,
                stack_trace: None,
                test_output: Some("c".repeat(4000)),
            },
        ];
        super::apply_total_output_limit(&mut failures, 10_000);
        assert!(failures[0].test_output.is_some());
        assert!(failures[1].test_output.is_some());
        assert!(
            failures[2].test_output.is_none(),
            "third should exceed 10k cumulative"
        );
    }

    #[test]
    fn parse_content_collects_suite_stats() {
        let xml = include_str!(
            "../../../tests/fixtures/surefire_xml/TEST-com.example.auth.user.UsersTest.xml"
        );
        let r = parse_content(xml, &[]).expect("real fixture must parse");
        assert_eq!(r.suites.len(), 1);
        let s = &r.suites[0];
        assert_eq!(s.class_name, "com.example.auth.user.UsersTest");
        assert_eq!(s.tests, r.summary.run);
        assert_eq!(s.skipped, r.summary.skipped);
        assert!(s.time_secs > 0.0, "testsuite time attr must be parsed");
        assert_eq!(s.module, None);
    }

    #[test]
    fn parse_content_collects_skipped_test_names() {
        let xml = include_str!(
            "../../../tests/fixtures/surefire_xml/TEST-com.example.auth.partners.entraid.MicrosoftEntraIdClient2Test.xml"
        );
        let r = parse_content(xml, &[]).expect("real fixture must parse");
        assert_eq!(r.skipped_tests.len(), 8);
        let st = &r.skipped_tests[0];
        assert_eq!(st.class, "com.example.auth.partners.entraid.MicrosoftEntraIdClient2Test");
        assert!(!st.method.is_empty());
    }
}
