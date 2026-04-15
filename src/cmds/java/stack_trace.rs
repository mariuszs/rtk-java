//! Port of maven-mcp's StackTraceProcessor.
//!
//! Parses Java stack traces into segments (top-level exception + Caused by
//! chains), classifies frames as application or framework by package prefix,
//! collapses framework noise, and preserves root-cause frames.

const MAX_HEADER_LENGTH: usize = 200;

#[derive(Debug, PartialEq)]
#[allow(dead_code)]
pub(crate) struct Segment {
    pub(crate) header: String,
    pub(crate) frames: Vec<String>,
}

/// Split a stack trace into segments.
///
/// The first non-empty line becomes the header of segment 0. Each subsequent
/// line starting with the literal `"Caused by:"` (no leading whitespace) closes
/// the current segment and opens a new one. All other lines append to the
/// current segment's frames.
///
/// Indented `"\tCaused by:"` inside Suppressed blocks stays as a frame and
/// does NOT split segments — `is_structural_line` preserves it during frame
/// collapsing.
#[allow(dead_code)]
pub(crate) fn parse_segments(trace: &str) -> Vec<Segment> {
    let trace = trace.trim();
    if trace.is_empty() {
        return Vec::new();
    }

    let mut segments = Vec::new();
    let mut current_header: Option<String> = None;
    let mut current_frames: Vec<String> = Vec::new();

    for line in trace.lines() {
        if current_header.is_none() {
            current_header = Some(line.to_string());
        } else if line.starts_with("Caused by:") {
            segments.push(Segment {
                header: current_header.take().unwrap(),
                frames: std::mem::take(&mut current_frames),
            });
            current_header = Some(line.to_string());
        } else {
            current_frames.push(line.to_string());
        }
    }

    if let Some(header) = current_header {
        segments.push(Segment {
            header,
            frames: current_frames,
        });
    }

    segments
}

/// Truncate a header to `MAX_HEADER_LENGTH` **Unicode characters** (not bytes),
/// appending "..." if truncated.
#[allow(dead_code)]
pub(crate) fn truncate_header(header: &str) -> String {
    let char_count = header.chars().count();
    if char_count <= MAX_HEADER_LENGTH {
        return header.to_string();
    }
    let truncated: String = header.chars().take(MAX_HEADER_LENGTH).collect();
    format!("{truncated}...")
}

/// A stack frame belongs to the application if, after stripping whitespace and
/// the leading `"at "` marker, the remainder starts with `app_package`.
///
/// When `app_package` is `None` or empty, every frame is considered an app frame
/// (framework collapsing disabled). Summary lines like `"\t... 42 more"` are
/// always framework artifacts.
#[allow(dead_code)]
pub(crate) fn is_application_frame(frame: &str, app_package: Option<&str>) -> bool {
    let Some(pkg) = app_package.filter(|p| !p.is_empty()) else {
        return true;
    };
    let trimmed = frame.trim_start();
    let Some(after_at) = trimmed.strip_prefix("at ") else {
        return false;
    };
    after_at.starts_with(pkg)
}

/// Structural lines must always be preserved even while collapsing framework
/// frames: Suppressed block headers and **indented** Caused-by lines (which
/// appear inside Suppressed blocks; top-level Caused-by is already a segment
/// boundary, not a frame).
#[allow(dead_code)]
pub(crate) fn is_structural_line(line: &str) -> bool {
    if line.is_empty() {
        return false;
    }
    let trimmed = line.trim_start();
    if trimmed.starts_with("Suppressed:") {
        return true;
    }
    if trimmed.starts_with("Caused by:") {
        // Only structural when indented (nested in suppressed). Top-level
        // Caused by: is handled by parse_segments, not here.
        return line
            .chars()
            .next()
            .is_some_and(char::is_whitespace);
    }
    false
}

/// Push frames to `output`, collapsing runs of consecutive framework frames
/// into a single `"\t... N framework frames omitted"` marker.
///
/// When `app_package` is `None`, all frames are considered app frames and no
/// collapsing occurs — pass-through mode.
#[allow(dead_code)]
pub(crate) fn add_collapsed_frames(
    output: &mut Vec<String>,
    frames: &[String],
    app_package: Option<&str>,
) {
    let filter = app_package.is_some_and(|p| !p.is_empty());
    if !filter {
        for frame in frames {
            output.push(frame.clone());
        }
        return;
    }

    let mut framework_count: usize = 0;
    for frame in frames {
        let structural = is_structural_line(frame);
        if structural || is_application_frame(frame, app_package) {
            if framework_count > 0 {
                output.push(format!("\t... {framework_count} framework frames omitted"));
                framework_count = 0;
            }
            if structural {
                output.push(truncate_header(frame));
            } else {
                output.push(frame.clone());
            }
        } else {
            framework_count += 1;
        }
    }
    if framework_count > 0 {
        output.push(format!("\t... {framework_count} framework frames omitted"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_segments_empty_input_returns_empty() {
        assert!(parse_segments("").is_empty());
    }

    #[test]
    fn parse_segments_single_header_no_frames() {
        let trace = "java.lang.RuntimeException: boom";
        let segs = parse_segments(trace);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].header, "java.lang.RuntimeException: boom");
        assert!(segs[0].frames.is_empty());
    }

    #[test]
    fn parse_segments_single_segment_with_frames() {
        let trace = "java.lang.RuntimeException: boom\n\
                     \tat com.example.A.foo(A.java:1)\n\
                     \tat com.example.B.bar(B.java:2)";
        let segs = parse_segments(trace);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].frames.len(), 2);
    }

    #[test]
    fn parse_segments_caused_by_starts_new_segment() {
        let trace = "java.lang.RuntimeException: outer\n\
                     \tat com.example.A.foo(A.java:1)\n\
                     Caused by: java.io.IOException: inner\n\
                     \tat com.example.B.bar(B.java:2)";
        let segs = parse_segments(trace);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].header, "java.lang.RuntimeException: outer");
        assert_eq!(segs[0].frames, vec!["\tat com.example.A.foo(A.java:1)"]);
        assert_eq!(segs[1].header, "Caused by: java.io.IOException: inner");
        assert_eq!(segs[1].frames, vec!["\tat com.example.B.bar(B.java:2)"]);
    }

    #[test]
    fn parse_segments_indented_caused_by_stays_as_frame() {
        // Inside a Suppressed block, the "Caused by:" is indented and must NOT
        // split segments — it stays as a frame so structural handling keeps it.
        let trace = "java.lang.RuntimeException: outer\n\
                     \tSuppressed: java.io.IOException: suppressed\n\
                     \t\tat com.example.A.foo(A.java:1)\n\
                     \t\tCaused by: java.lang.Error: nested\n\
                     Caused by: java.io.IOException: real cause";
        let segs = parse_segments(trace);
        assert_eq!(segs.len(), 2, "indented Caused by must not split segments");
        assert_eq!(segs[0].frames.len(), 3, "Suppressed block stays in outer");
        assert_eq!(segs[1].header, "Caused by: java.io.IOException: real cause");
    }

    #[test]
    fn truncate_header_short_passes_through() {
        assert_eq!(truncate_header("short"), "short");
    }

    #[test]
    fn truncate_header_exact_200_chars_passes() {
        let s = "a".repeat(200);
        assert_eq!(truncate_header(&s), s);
    }

    #[test]
    fn truncate_header_over_200_chars_truncates_with_ellipsis() {
        let s = "a".repeat(250);
        let out = truncate_header(&s);
        assert_eq!(out.chars().count(), 203); // 200 + "..."
        assert!(out.ends_with("..."));
    }

    #[test]
    fn truncate_header_utf8_multibyte_safe() {
        // 100 4-byte chars = 400 bytes but 100 chars — must not panic
        let s = "日".repeat(100);
        assert_eq!(truncate_header(&s), s);
        let s = "日".repeat(250);
        let out = truncate_header(&s);
        assert_eq!(out.chars().count(), 203);
        assert!(out.ends_with("..."));
    }

    #[test]
    fn is_app_frame_no_filter_accepts_everything() {
        assert!(is_application_frame("\tat com.example.A.foo(A.java:1)", None));
        assert!(is_application_frame("\tat org.springframework.boot.Run(Run.java:1)", None));
        assert!(is_application_frame("\t... 42 more", None));
    }

    #[test]
    fn is_app_frame_with_package_accepts_matching() {
        assert!(is_application_frame(
            "\tat com.example.A.foo(A.java:1)",
            Some("com.example"),
        ));
        assert!(!is_application_frame(
            "\tat org.springframework.boot.Run(Run.java:1)",
            Some("com.example"),
        ));
    }

    #[test]
    fn is_app_frame_rejects_summary_dots() {
        // "\t... 42 more" is a framework artifact, never app
        assert!(!is_application_frame("\t... 42 more", Some("com.example")));
    }

    #[test]
    fn is_app_frame_rejects_empty_or_whitespace() {
        assert!(!is_application_frame("", Some("com.example")));
        assert!(!is_application_frame("   ", Some("com.example")));
    }

    #[test]
    fn is_structural_suppressed_top_level() {
        assert!(is_structural_line("\tSuppressed: java.io.IOException"));
        assert!(is_structural_line("Suppressed: foo"));
    }

    #[test]
    fn is_structural_indented_caused_by_only() {
        // Top-level "Caused by:" is a segment boundary, not structural
        assert!(!is_structural_line("Caused by: java.io.IOException"));
        // Indented "Caused by:" inside suppressed is structural
        assert!(is_structural_line("\tCaused by: java.io.IOException"));
        assert!(is_structural_line("  Caused by: nested"));
    }

    #[test]
    fn is_structural_regular_frame_no() {
        assert!(!is_structural_line("\tat com.example.A.foo(A.java:1)"));
        assert!(!is_structural_line(""));
    }

    fn collect_collapsed(frames: &[&str], app_package: Option<&str>) -> Vec<String> {
        let frames: Vec<String> = frames.iter().map(|s| s.to_string()).collect();
        let mut out = Vec::new();
        add_collapsed_frames(&mut out, &frames, app_package);
        out
    }

    #[test]
    fn collapse_no_filter_keeps_everything() {
        let frames = [
            "\tat org.framework.Foo(Foo.java:1)",
            "\tat com.example.A.foo(A.java:1)",
            "\tat org.framework.Bar(Bar.java:2)",
        ];
        let out = collect_collapsed(&frames, None);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn collapse_all_framework_yields_single_summary() {
        let frames = [
            "\tat org.framework.Foo(Foo.java:1)",
            "\tat org.framework.Bar(Bar.java:2)",
            "\tat org.framework.Baz(Baz.java:3)",
        ];
        let out = collect_collapsed(&frames, Some("com.example"));
        assert_eq!(out, vec!["\t... 3 framework frames omitted"]);
    }

    #[test]
    fn collapse_alternating_produces_multiple_summaries() {
        let frames = [
            "\tat org.framework.Foo(Foo.java:1)",
            "\tat com.example.A.one(A.java:10)",
            "\tat org.framework.Bar(Bar.java:2)",
            "\tat org.framework.Baz(Baz.java:3)",
            "\tat com.example.B.two(B.java:20)",
        ];
        let out = collect_collapsed(&frames, Some("com.example"));
        assert_eq!(
            out,
            vec![
                "\t... 1 framework frames omitted",
                "\tat com.example.A.one(A.java:10)",
                "\t... 2 framework frames omitted",
                "\tat com.example.B.two(B.java:20)",
            ]
        );
    }

    #[test]
    fn collapse_preserves_structural_inline() {
        let frames = [
            "\tat org.framework.Foo(Foo.java:1)",
            "\tSuppressed: java.io.IOException",
            "\t\tat org.framework.Bar(Bar.java:2)",
            "\t\tCaused by: java.lang.Error: nested",
        ];
        let out = collect_collapsed(&frames, Some("com.example"));
        assert_eq!(
            out,
            vec![
                "\t... 1 framework frames omitted",
                "\tSuppressed: java.io.IOException",
                "\t... 1 framework frames omitted",
                "\t\tCaused by: java.lang.Error: nested",
            ]
        );
    }
}
