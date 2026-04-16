//! Port of maven-mcp's StackTraceProcessor.
//!
//! Parses Java stack traces into segments (top-level exception + Caused by
//! chains), classifies frames as application or framework by package prefix,
//! collapses framework noise, and preserves root-cause frames.

const MAX_HEADER_LENGTH: usize = 200;
const DEFAULT_ROOT_CAUSE_APP_FRAMES: usize = 10;

#[derive(Debug, PartialEq)]
struct Segment {
    header: String,
    frames: Vec<String>,
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
fn parse_segments(trace: &str) -> Vec<Segment> {
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
fn is_application_frame(frame: &str, app_package: Option<&str>) -> bool {
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
fn is_structural_line(line: &str) -> bool {
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
///
/// When `max_app_frames` is `Some(n)`, at most `n` non-structural application
/// frames are kept (root-cause mode). Structural lines bypass the cap.
fn add_frames(
    output: &mut Vec<String>,
    frames: &[String],
    app_package: Option<&str>,
    max_app_frames: Option<usize>,
) {
    let filter = app_package.is_some_and(|p| !p.is_empty());
    if !filter {
        for frame in frames {
            output.push(frame.clone());
        }
        return;
    }

    let mut app_count: usize = 0;
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
            } else if max_app_frames.is_none_or(|cap| app_count < cap) {
                output.push(frame.clone());
                app_count += 1;
            }
        } else {
            framework_count += 1;
        }
    }
    if framework_count > 0 {
        output.push(format!("\t... {framework_count} framework frames omitted"));
    }
}

/// Process a Java stack trace:
///   - Top-level header preserved (truncated to 200 chars).
///   - Non-root segments: header + collapsed frames.
///   - Root (last) segment: header + capped root-cause frames.
///   - If `max_lines > 0` and the collapsed output exceeds the cap,
///     `apply_hard_cap` is called to truncate while preserving the root cause.
///
/// Returns `None` iff `raw` is empty or whitespace-only.
pub(crate) fn process(raw: &str, app_package: Option<&str>, max_lines: usize) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let segments = parse_segments(trimmed);
    if segments.is_empty() {
        return Some(trimmed.to_string());
    }

    let mut out: Vec<String> = Vec::new();
    out.push(truncate_header(&segments[0].header));

    if segments.len() == 1 {
        add_frames(&mut out, &segments[0].frames, app_package, None);
    } else {
        add_frames(&mut out, &segments[0].frames, app_package, None);
        for seg in &segments[1..segments.len() - 1] {
            out.push(truncate_header(&seg.header));
            add_frames(&mut out, &seg.frames, app_package, None);
        }
        let root = segments.last().unwrap();
        out.push(truncate_header(&root.header));
        add_frames(
            &mut out,
            &root.frames,
            app_package,
            Some(DEFAULT_ROOT_CAUSE_APP_FRAMES),
        );
    }

    if max_lines > 0 && out.len() > max_lines {
        out = apply_hard_cap(out, &segments, max_lines);
    }

    Some(out.join("\n"))
}

/// Apply a hard cap while preserving the root cause.
///
/// - For a single segment: straight truncate to `max_lines`.
/// - For multiple segments:
///   - If the root-cause header's index in `out` is already beyond the cap,
///     build a synthetic output: `[top_header, "... (intermediate frames
///     truncated)", root_header, root frames until cap]`.
///   - Otherwise (root-cause header within the cap): straight truncate.
fn apply_hard_cap(out: Vec<String>, segments: &[Segment], max_lines: usize) -> Vec<String> {
    if segments.len() <= 1 {
        let mut out = out;
        out.truncate(max_lines);
        return out;
    }

    let root = segments.last().unwrap();
    let truncated_root_header = truncate_header(&root.header);
    let root_idx = out
        .iter()
        .rposition(|line| line == &truncated_root_header);

    let Some(idx) = root_idx else {
        let mut out = out;
        out.truncate(max_lines);
        return out;
    };

    if idx < max_lines.saturating_sub(1) {
        let mut out = out;
        out.truncate(max_lines);
        return out;
    }

    // Root cause beyond the cap — build synthetic layout.
    let mut result: Vec<String> = Vec::with_capacity(max_lines);
    if let Some(top) = out.first() {
        result.push(top.clone());
    }
    if max_lines >= 3 {
        result.push("\t... (intermediate frames truncated)".to_string());
    }
    result.push(truncated_root_header.clone());

    let mut remaining = max_lines.saturating_sub(result.len());
    for line in &out[(idx + 1)..] {
        if remaining == 0 {
            break;
        }
        result.push(line.clone());
        remaining -= 1;
    }
    result
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

    fn collect_root_cause(frames: &[&str], app_package: Option<&str>) -> Vec<String> {
        let frames: Vec<String> = frames.iter().map(|s| s.to_string()).collect();
        let mut out = Vec::new();
        add_frames(
            &mut out,
            &frames,
            app_package,
            Some(DEFAULT_ROOT_CAUSE_APP_FRAMES),
        );
        out
    }

    #[test]
    fn root_cause_caps_app_frames_at_ten() {
        let mut frames = Vec::new();
        for i in 0..15 {
            frames.push(format!("\tat com.example.A.m{i}(A.java:{i})"));
        }
        let frame_refs: Vec<&str> = frames.iter().map(|s| s.as_str()).collect();
        let out = collect_root_cause(&frame_refs, Some("com.example"));
        // 10 kept, 5 dropped silently (no "framework" marker because these are app frames)
        assert_eq!(out.len(), 10);
    }

    #[test]
    fn root_cause_no_filter_keeps_all_frames() {
        let mut frames = Vec::new();
        for i in 0..15 {
            frames.push(format!("\tat com.example.A.m{i}(A.java:{i})"));
        }
        let frame_refs: Vec<&str> = frames.iter().map(|s| s.as_str()).collect();
        let out = collect_root_cause(&frame_refs, None);
        assert_eq!(out.len(), 15);
    }

    #[test]
    fn root_cause_structural_bypasses_cap() {
        // Structural lines are always preserved, even if we already hit the 10-app cap.
        let mut frames = Vec::new();
        for i in 0..10 {
            frames.push(format!("\tat com.example.A.m{i}(A.java:{i})"));
        }
        frames.push("\tSuppressed: x".to_string());
        frames.push("\tat com.example.Z.zzz(Z.java:99)".to_string()); // 11th app — dropped
        let frame_refs: Vec<&str> = frames.iter().map(|s| s.as_str()).collect();
        let out = collect_root_cause(&frame_refs, Some("com.example"));
        assert_eq!(out.len(), 11, "10 app frames + 1 structural, 11th app dropped");
        assert!(out.contains(&"\tSuppressed: x".to_string()));
    }

    #[test]
    fn root_cause_collapses_framework_as_before() {
        let frames = [
            "\tat com.example.A.foo(A.java:1)",
            "\tat org.framework.X(X.java:1)",
            "\tat org.framework.Y(Y.java:2)",
            "\tat com.example.B.bar(B.java:2)",
        ];
        let out = collect_root_cause(&frames, Some("com.example"));
        assert_eq!(
            out,
            vec![
                "\tat com.example.A.foo(A.java:1)",
                "\t... 2 framework frames omitted",
                "\tat com.example.B.bar(B.java:2)",
            ]
        );
    }

    #[test]
    fn process_empty_returns_none() {
        assert!(process("", Some("com.example"), 0).is_none());
        assert!(process("   \n  ", Some("com.example"), 0).is_none());
    }

    #[test]
    fn process_single_segment_no_filter_returns_verbatim() {
        let trace = "java.lang.RuntimeException: boom\n\tat com.example.A.foo(A.java:1)";
        let out = process(trace, None, 0).unwrap();
        assert_eq!(out, trace);
    }

    #[test]
    fn process_single_segment_collapses_framework() {
        let trace = "java.lang.AssertionError: fail\n\
                     \tat com.example.Test.t(Test.java:5)\n\
                     \tat org.junit.runner.Run(Run.java:1)\n\
                     \tat org.junit.runner.Run(Run.java:2)";
        let out = process(trace, Some("com.example"), 0).unwrap();
        assert_eq!(
            out,
            "java.lang.AssertionError: fail\n\
             \tat com.example.Test.t(Test.java:5)\n\
             \t... 2 framework frames omitted"
        );
    }

    #[test]
    fn process_multi_segment_preserves_root_cause() {
        let trace = "java.lang.RuntimeException: outer\n\
                     \tat org.spring.Foo(Foo.java:1)\n\
                     Caused by: java.io.IOException: middle\n\
                     \tat org.hibernate.Bar(Bar.java:2)\n\
                     Caused by: java.net.ConnectException: inner\n\
                     \tat com.example.DbService.connect(DbService.java:42)";
        let out = process(trace, Some("com.example"), 0).unwrap();
        assert!(out.contains("java.lang.RuntimeException: outer"));
        assert!(out.contains("Caused by: java.io.IOException: middle"));
        assert!(out.contains("Caused by: java.net.ConnectException: inner"));
        assert!(out.contains("\tat com.example.DbService.connect(DbService.java:42)"));
        assert!(out.contains("framework frames omitted"));
    }

    fn collect_collapsed(frames: &[&str], app_package: Option<&str>) -> Vec<String> {
        let frames: Vec<String> = frames.iter().map(|s| s.to_string()).collect();
        let mut out = Vec::new();
        add_frames(&mut out, &frames, app_package, None);
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

    #[test]
    fn hard_cap_single_segment_simple_truncate() {
        // Non-app frames collapse to one marker, so a 21-line input produces
        // a 2-line out. Verify cap-not-triggered behavior first.
        let mut trace = String::from("java.lang.RuntimeException: boom");
        for i in 0..20 {
            trace.push_str(&format!("\n\tat com.example.A.m{i}(A.java:{i})"));
        }
        // With app_package=Some("com.example"), all frames are app frames.
        // out = [header, 20 app frames]. With cap=5, out.len()=21 > 5 →
        // straight-truncate to 5.
        let out = process(&trace, Some("com.example"), 5).unwrap();
        assert_eq!(out.lines().count(), 5);
    }

    #[test]
    fn hard_cap_multi_segment_preserves_root_cause() {
        // Two segments, each with enough app frames that even after
        // framework-frame collapsing, the total pushed lines exceeds the cap
        // AND the root-cause header sits at/beyond (max_lines - 1), forcing
        // the synthetic "... (intermediate frames truncated)" layout.
        let trace = "java.lang.RuntimeException: outer\n\
                     \tat com.example.A.foo(A.java:1)\n\
                     \tat com.example.A.bar(A.java:2)\n\
                     \tat com.example.A.baz(A.java:3)\n\
                     \tat com.example.A.qux(A.java:4)\n\
                     Caused by: java.io.IOException: real cause\n\
                     \tat com.example.DbService.connect(Db.java:88)\n\
                     \tat com.example.DbService.prepare(Db.java:91)\n\
                     \tat com.example.DbService.execute(Db.java:94)";
        // out from process(): [top_header, 4 app frames, root_header, 3 app frames]
        // out.len() = 9. With max_lines = 5, root_idx = 5, 5 >= 5-1=4 → synthetic.
        let out = process(trace, Some("com.example"), 5).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 5, "must fit exactly in max_lines=5, got: {out}");
        assert_eq!(lines[0], "java.lang.RuntimeException: outer");
        assert_eq!(lines[1], "\t... (intermediate frames truncated)");
        assert_eq!(lines[2], "Caused by: java.io.IOException: real cause");
        assert!(
            lines[3].contains("com.example.DbService"),
            "first root frame must survive; got line 3: {:?}",
            lines[3]
        );
    }

    #[test]
    fn hard_cap_multi_segment_root_within_limit_straight_truncate() {
        // Root cause header at line 3 of output, cap at 10 → straight truncate.
        let trace = "java.lang.RuntimeException: outer\n\
                     \tat com.example.A.foo(A.java:1)\n\
                     Caused by: java.io.IOException: inner\n\
                     \tat com.example.B.bar(B.java:1)\n\
                     \tat com.example.B.baz(B.java:2)\n\
                     \tat com.example.B.qux(B.java:3)\n\
                     \tat com.example.B.quux(B.java:4)\n\
                     \tat com.example.B.corge(B.java:5)";
        let out = process(trace, Some("com.example"), 6).unwrap();
        assert_eq!(out.lines().count(), 6);
    }

    #[test]
    fn process_real_world_spring_fixture() {
        let trace = include_str!("../../../tests/fixtures/java/stack-traces/multi-caused-by.txt");
        let out = process(trace, Some("com.example"), 50).unwrap();
        assert!(out.contains("Caused by: org.springframework.beans.factory.BeanCreationException"));
        assert!(out.contains("Caused by: org.hibernate.HibernateException"));
        assert!(out.contains("com.example.DbIntegrationIT.shouldConnect"));
        assert!(out.contains("framework frames omitted"));
    }
}
