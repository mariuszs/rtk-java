//! Parses Maven Surefire/Failsafe XML test reports from
//! `target/surefire-reports/TEST-*.xml` and `target/failsafe-reports/*.xml`.
//! Uses quick-xml streaming parser. Time-gated by `started_at` to skip stale
//! reports from previous runs.

use crate::cmds::java::stack_trace;
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

pub const DEFAULT_STACK_TRACE_LINES: usize = 50;
pub const DEFAULT_PER_TEST_OUTPUT_LIMIT: usize = 2000;
#[allow(dead_code)]
pub const DEFAULT_TOTAL_OUTPUT_LIMIT: usize = 10_000;

#[derive(Debug, Default, PartialEq)]
pub struct TestSummary {
    pub run: u32,
    pub failures: u32,
    pub errors: u32,
    pub skipped: u32,
}

impl TestSummary {
    fn add(&mut self, other: &Self) {
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

#[derive(Debug, Default, PartialEq)]
pub struct SurefireResult {
    pub summary: TestSummary,
    pub failures: Vec<TestFailure>,
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
/// `app_package` is passed to `stack_trace::process` for frame classification.
///
/// Returns `None` only if the XML is completely malformed; otherwise a
/// best-effort result is returned.
#[allow(dead_code)]
pub(crate) fn parse_content(xml: &str, app_package: Option<&str>) -> Option<SurefireResult> {
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
            Ok(Event::End(e)) => {
                match local_name(e.name().as_ref()) {
                    b"failure" | b"error" => {
                        let processed = stack_trace::process(
                            stack_buf.trim(),
                            app_package,
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
                                if last.test_class == current_class.clone().unwrap_or_default()
                                    && last.test_method
                                        == current_method.clone().unwrap_or_default()
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
    Some(truncate_test_output(&combined, per_test_limit))
}

fn truncate_test_output(output: &str, max_chars: usize) -> String {
    let char_count = output.chars().count();
    if char_count <= max_chars {
        return output.to_string();
    }
    let skip = char_count - max_chars;
    let tail: String = output.chars().skip(skip).collect();
    format!("... ({skip} chars truncated)\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_content_single_passing() {
        let xml = include_str!(
            "../../../tests/fixtures/java/surefire-reports/TEST-com.example.PassingTest.xml"
        );
        let result = parse_content(xml, None).expect("passing testsuite parses");
        assert!(result.summary.run >= 1);
        assert_eq!(result.summary.failures, 0);
        assert_eq!(result.summary.errors, 0);
        assert!(result.failures.is_empty());
    }

    #[test]
    fn parse_content_single_failing_extracts_details() {
        let xml = include_str!(
            "../../../tests/fixtures/java/surefire-reports/TEST-com.example.FailingTest.xml"
        );
        let result = parse_content(xml, None).expect("failing testsuite parses");
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
        let result = parse_content(xml, None).expect("parses");
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
        let result = parse_content(xml, None).expect("parses");
        assert!(result.failures.iter().any(|f| f.kind == FailureKind::Error));
    }

    #[test]
    fn parse_content_skipped_testsuite_counts_skipped() {
        let xml = include_str!(
            "../../../tests/fixtures/java/surefire-reports/TEST-com.example.SkippedTest.xml"
        );
        let result = parse_content(xml, None).expect("parses");
        assert!(result.summary.skipped > 0);
    }
}
