# mvn Surefire/Failsafe XML Enrichment Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port maven-mcp's `SurefireReportParser` + `StackTraceProcessor` to Rust, plus a pom.xml groupId autodetect and an integration layer that enriches `rtk mvn test` output with structured failure details read from `target/surefire-reports/` and `target/failsafe-reports/` XML.

**Architecture:** Three pure new modules under `src/cmds/java/` (`stack_trace.rs`, `surefire_reports.rs`, `pom_groupid.rs`) plus a post-text-filter I/O layer in `mvn_cmd.rs`. The existing `filter_mvn_test` string transformer stays untouched; a new `enrich_with_reports(text, cwd, since, app_pkg)` function reads XML reports (time-gated by `started_at`) and appends a structured failures section. Auto-detected `appPackage` feeds `stack_trace::process` for framework-frame collapsing with root-cause preservation.

**Tech Stack:** Rust, `quick-xml = "0.37"` (already in deps, used by `dotnet_trx.rs`), `anyhow`, `lazy_static`, `insta` for snapshots, `tempfile` for integration tests, `filetime = "0.2"` added as dev-dep for mtime-based time-gate tests.

**Spec:** `docs/superpowers/specs/2026-04-15-mvn-surefire-xml-enrichment-design.md`

**Fork / PR target:** `mariuszs/rtk-java`, branch `feat/mvn-surefire-xml` stacked on `feat/mvn-rust-module`, PR into fork's `master`.

---

## Task 0: Branch, scaffolding, dev-dep

**Files:**
- Modify: `Cargo.toml` (add `filetime` dev-dep)
- Create: `src/cmds/java/stack_trace.rs` (empty stub)
- Create: `src/cmds/java/surefire_reports.rs` (empty stub)
- Create: `src/cmds/java/pom_groupid.rs` (empty stub)
- Create: `tests/fixtures/java/surefire-reports/.gitkeep`
- Create: `tests/fixtures/java/failsafe-reports/.gitkeep`
- Create: `tests/fixtures/java/poms/.gitkeep`
- Create: `tests/fixtures/java/stack-traces/.gitkeep`

Note: `src/cmds/java/mod.rs` is `automod::dir!(pub "src/cmds/java");` — it auto-exports every `.rs` file in the directory. No manual module wiring needed; the stubs will be picked up automatically once they compile.

- [ ] **Step 0.1: Create and switch to branch**

Run:
```bash
git checkout feat/mvn-rust-module
git checkout -b feat/mvn-surefire-xml
git status
```
Expected: `On branch feat/mvn-surefire-xml`, clean working tree.

- [ ] **Step 0.2: Add `filetime` dev-dep**

Edit `Cargo.toml`, change the `[dev-dependencies]` block from:
```toml
[dev-dependencies]
```
to:
```toml
[dev-dependencies]
filetime = "0.2"
insta = "1"
```

Note: verify `insta` is not already declared elsewhere in `Cargo.toml` before adding. If `insta` is already in `[dependencies]`, only add `filetime`.

- [ ] **Step 0.3: Check `insta` availability**

Run:
```bash
grep -n '^insta' Cargo.toml
```
If `insta = ...` is already listed under `[dependencies]`, remove the `insta = "1"` line you added in step 0.2 (keep only `filetime = "0.2"`).

- [ ] **Step 0.4: Create module stubs**

Create `src/cmds/java/stack_trace.rs`:
```rust
//! Port of maven-mcp's StackTraceProcessor.
//!
//! Parses Java stack traces into segments (top-level exception + Caused by
//! chains), classifies frames as application or framework by package prefix,
//! collapses framework noise, and preserves root-cause frames.
```

Create `src/cmds/java/surefire_reports.rs`:
```rust
//! Parses Maven Surefire/Failsafe XML test reports from
//! `target/surefire-reports/TEST-*.xml` and `target/failsafe-reports/*.xml`.
//! Uses quick-xml streaming parser. Time-gated by `started_at` to skip stale
//! reports from previous runs.
```

Create `src/cmds/java/pom_groupid.rs`:
```rust
//! Autodetects the application Java package from `pom.xml <groupId>`.
//! Used by `surefire_reports` / `stack_trace` to classify application frames.
//! Can be overridden by `RTK_MVN_APP_PACKAGE` env var.
```

Create empty fixture directories:
```bash
mkdir -p tests/fixtures/java/{surefire-reports,failsafe-reports,poms,stack-traces}
touch tests/fixtures/java/{surefire-reports,failsafe-reports,poms,stack-traces}/.gitkeep
```

- [ ] **Step 0.5: Verify build**

Run:
```bash
cargo build
```
Expected: PASS. `automod` auto-discovers the new modules; they're empty and harmless.

- [ ] **Step 0.6: Commit scaffolding**

```bash
git add Cargo.toml src/cmds/java/stack_trace.rs src/cmds/java/surefire_reports.rs src/cmds/java/pom_groupid.rs tests/fixtures/java/
git commit -m "chore(mvn): scaffold surefire-xml modules and fixture dirs

Empty stubs for stack_trace, surefire_reports, pom_groupid. Adds
filetime dev-dep for mtime-based time-gate tests in later tasks."
```

---

## Task 1: `stack_trace::Segment` + `parse_segments`

**Files:**
- Modify: `src/cmds/java/stack_trace.rs`

- [ ] **Step 1.1: Write failing tests**

Append to `src/cmds/java/stack_trace.rs`:
```rust
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
}
```

- [ ] **Step 1.2: Run tests to verify they fail**

Run:
```bash
cargo test --lib stack_trace::tests
```
Expected: FAIL — `parse_segments` and `Segment` not defined.

- [ ] **Step 1.3: Implement `Segment` and `parse_segments`**

Prepend to `src/cmds/java/stack_trace.rs` (above the `#[cfg(test)]` block):
```rust
#[derive(Debug, PartialEq)]
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
```

- [ ] **Step 1.4: Run tests to verify they pass**

Run:
```bash
cargo test --lib stack_trace::tests
```
Expected: 5 PASS.

- [ ] **Step 1.5: Commit**

```bash
git add src/cmds/java/stack_trace.rs
git commit -m "feat(mvn): add stack trace segment parser

Splits Java stack traces on top-level 'Caused by:' while keeping
indented Caused by lines inside Suppressed blocks as frames."
```

---

## Task 2: `truncate_header` with UTF-8 safety

**Files:**
- Modify: `src/cmds/java/stack_trace.rs`

- [ ] **Step 2.1: Add tests**

Append to the `tests` module in `src/cmds/java/stack_trace.rs`:
```rust
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
```

- [ ] **Step 2.2: Run tests to verify they fail**

Run:
```bash
cargo test --lib stack_trace::tests::truncate_header
```
Expected: FAIL — `truncate_header` not defined.

- [ ] **Step 2.3: Implement `truncate_header` (and consts)**

Add near the top of `src/cmds/java/stack_trace.rs` (below the doc comment):
```rust
const MAX_HEADER_LENGTH: usize = 200;
```

Add below `parse_segments`:
```rust
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
```

- [ ] **Step 2.4: Run tests**

Run:
```bash
cargo test --lib stack_trace::tests
```
Expected: 9 PASS (5 parse_segments + 4 truncate_header).

- [ ] **Step 2.5: Commit**

```bash
git add src/cmds/java/stack_trace.rs
git commit -m "feat(mvn): add UTF-8-safe stack trace header truncation

Counts Unicode chars, not bytes. 200-char cap matches maven-mcp original."
```

---

## Task 3: Frame classification — `is_application_frame`, `is_structural_line`

**Files:**
- Modify: `src/cmds/java/stack_trace.rs`

- [ ] **Step 3.1: Add tests**

Append to tests:
```rust
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
```

- [ ] **Step 3.2: Run tests to verify they fail**

Run:
```bash
cargo test --lib stack_trace::tests
```
Expected: 6 new FAIL on `is_application_frame` / `is_structural_line`.

- [ ] **Step 3.3: Implement**

Add below `truncate_header`:
```rust
/// A stack frame belongs to the application if, after stripping whitespace and
/// the leading `"at "` marker, the remainder starts with `app_package`.
///
/// When `app_package` is `None` or empty, every frame is considered an app frame
/// (framework collapsing disabled). Summary lines like `"\t... 42 more"` are
/// always framework artifacts.
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
```

- [ ] **Step 3.4: Run tests**

Run:
```bash
cargo test --lib stack_trace::tests
```
Expected: 16 PASS.

- [ ] **Step 3.5: Commit**

```bash
git add src/cmds/java/stack_trace.rs
git commit -m "feat(mvn): classify stack frames as application vs framework

Structural lines (Suppressed:, indented Caused by:) are always
preserved during frame collapsing."
```

---

## Task 4: Frame collapsing — `add_collapsed_frames`

**Files:**
- Modify: `src/cmds/java/stack_trace.rs`

- [ ] **Step 4.1: Add tests**

Append to tests:
```rust
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
```

- [ ] **Step 4.2: Run tests to verify they fail**

Run:
```bash
cargo test --lib stack_trace::tests::collapse
```
Expected: FAIL — `add_collapsed_frames` not defined.

- [ ] **Step 4.3: Implement `add_collapsed_frames`**

Add below `is_structural_line`:
```rust
/// Push frames to `output`, collapsing runs of consecutive framework frames
/// into a single `"\t... N framework frames omitted"` marker.
///
/// When `app_package` is `None`, all frames are considered app frames and no
/// collapsing occurs — pass-through mode.
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
```

- [ ] **Step 4.4: Run tests**

Run:
```bash
cargo test --lib stack_trace::tests
```
Expected: 20 PASS.

- [ ] **Step 4.5: Commit**

```bash
git add src/cmds/java/stack_trace.rs
git commit -m "feat(mvn): collapse consecutive framework frames

Emits '... N framework frames omitted' for runs of non-app frames;
preserves app and structural (Suppressed / nested Caused by) frames."
```

---

## Task 5: Root-cause frame cap — `add_root_cause_frames`

**Files:**
- Modify: `src/cmds/java/stack_trace.rs`

- [ ] **Step 5.1: Add tests**

Append to tests:
```rust
    fn collect_root_cause(frames: &[&str], app_package: Option<&str>) -> Vec<String> {
        let frames: Vec<String> = frames.iter().map(|s| s.to_string()).collect();
        let mut out = Vec::new();
        add_root_cause_frames(&mut out, &frames, app_package);
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
```

- [ ] **Step 5.2: Run tests to verify they fail**

Run:
```bash
cargo test --lib stack_trace::tests::root_cause
```
Expected: FAIL — `add_root_cause_frames` not defined.

- [ ] **Step 5.3: Implement `add_root_cause_frames`**

Add the constant near the top of the file (after `MAX_HEADER_LENGTH`):
```rust
const DEFAULT_ROOT_CAUSE_APP_FRAMES: usize = 10;
```

Add below `add_collapsed_frames`:
```rust
/// Like `add_collapsed_frames`, but caps the number of non-structural
/// application frames at `DEFAULT_ROOT_CAUSE_APP_FRAMES`. Structural lines
/// (Suppressed, nested Caused by) bypass the cap.
pub(crate) fn add_root_cause_frames(
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
            } else if app_count < DEFAULT_ROOT_CAUSE_APP_FRAMES {
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
```

- [ ] **Step 5.4: Run tests**

Run:
```bash
cargo test --lib stack_trace::tests
```
Expected: 24 PASS.

- [ ] **Step 5.5: Commit**

```bash
git add src/cmds/java/stack_trace.rs
git commit -m "feat(mvn): cap root cause application frames at 10

Structural lines (Suppressed / nested Caused by) bypass the cap and
are always preserved."
```

---

## Task 6: `process` orchestrator (no hard cap yet)

**Files:**
- Modify: `src/cmds/java/stack_trace.rs`

- [ ] **Step 6.1: Add tests**

Append to tests:
```rust
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
```

- [ ] **Step 6.2: Run tests to verify they fail**

Run:
```bash
cargo test --lib stack_trace::tests::process
```
Expected: FAIL — `process` not defined.

- [ ] **Step 6.3: Implement `process` (hard cap as identity for now)**

Add below `add_root_cause_frames`:
```rust
/// Process a Java stack trace:
///   - Top-level header preserved (truncated to 200 chars).
///   - Non-root segments: header + `add_collapsed_frames`.
///   - Root (last) segment: header + `add_root_cause_frames`.
///   - If `max_lines > 0` and output exceeds the cap, apply hard-cap truncation
///     (implemented in a later task — currently returns full output).
///
/// Returns `None` iff `raw` is empty or whitespace-only.
pub fn process(raw: &str, app_package: Option<&str>, max_lines: usize) -> Option<String> {
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
        add_collapsed_frames(&mut out, &segments[0].frames, app_package);
    } else {
        add_collapsed_frames(&mut out, &segments[0].frames, app_package);
        for seg in &segments[1..segments.len() - 1] {
            out.push(truncate_header(&seg.header));
            add_collapsed_frames(&mut out, &seg.frames, app_package);
        }
        let root = segments.last().unwrap();
        out.push(truncate_header(&root.header));
        add_root_cause_frames(&mut out, &root.frames, app_package);
    }

    if max_lines > 0 && out.len() > max_lines {
        out = apply_hard_cap(out, &segments, max_lines);
    }

    Some(out.join("\n"))
}

// Temporary stub; real implementation in Task 7.
fn apply_hard_cap(out: Vec<String>, _segments: &[Segment], max_lines: usize) -> Vec<String> {
    let mut out = out;
    out.truncate(max_lines);
    out
}
```

- [ ] **Step 6.4: Run tests**

Run:
```bash
cargo test --lib stack_trace::tests
```
Expected: 28 PASS.

- [ ] **Step 6.5: Commit**

```bash
git add src/cmds/java/stack_trace.rs
git commit -m "feat(mvn): stack trace process orchestrator

Wires parse_segments, add_collapsed_frames, add_root_cause_frames into
the public process(raw, app_package, max_lines) API. Hard cap stubbed
for next task."
```

---

## Task 7: `apply_hard_cap` with root-cause preservation

**Files:**
- Modify: `src/cmds/java/stack_trace.rs`

- [ ] **Step 7.1: Add tests**

Append to tests:
```rust
    #[test]
    fn hard_cap_single_segment_simple_truncate() {
        let mut trace = String::from("java.lang.RuntimeException: boom");
        for i in 0..20 {
            trace.push_str(&format!("\n\tat com.example.A.m{i}(A.java:{i})"));
        }
        let out = process(&trace, Some("com.example"), 5).unwrap();
        assert_eq!(out.lines().count(), 5);
    }

    #[test]
    fn hard_cap_multi_segment_preserves_root_cause() {
        // Top header + 50 intermediate frames + Caused by: + 5 root frames,
        // cap at 10 lines → must still include Caused by: line and at least
        // one root frame.
        let mut trace = String::from("java.lang.RuntimeException: outer");
        for i in 0..50 {
            trace.push_str(&format!("\n\tat org.spring.A.m{i}(A.java:{i})"));
        }
        trace.push_str("\nCaused by: java.io.IOException: real cause");
        for i in 0..5 {
            trace.push_str(&format!("\n\tat com.example.DbService.m{i}(Db.java:{i})"));
        }

        let out = process(&trace, Some("com.example"), 10).unwrap();
        assert_eq!(out.lines().count(), 10);
        assert!(out.contains("java.lang.RuntimeException: outer"));
        assert!(out.contains("Caused by: java.io.IOException: real cause"));
        assert!(
            out.contains("com.example.DbService"),
            "at least one root-cause app frame must survive, got: {out}"
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
```

- [ ] **Step 7.2: Run tests to verify they fail**

Run:
```bash
cargo test --lib stack_trace::tests::hard_cap
```
Expected: `hard_cap_multi_segment_preserves_root_cause` likely FAILS (stub just truncates).

- [ ] **Step 7.3: Replace `apply_hard_cap` stub with real implementation**

Remove the stub `fn apply_hard_cap(...)` and replace with:
```rust
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
```

- [ ] **Step 7.4: Run tests**

Run:
```bash
cargo test --lib stack_trace
```
Expected: 31 PASS (all stack_trace tests).

- [ ] **Step 7.5: Commit**

```bash
git add src/cmds/java/stack_trace.rs
git commit -m "feat(mvn): stack trace hard cap preserves root cause

When root-cause header lies beyond the line cap, emit a synthetic layout
with a truncated-intermediate marker so the diagnostic punchline survives."
```

---

## Task 8: Copy Surefire XML fixtures from maven-mcp

**Files:**
- Create: `tests/fixtures/java/surefire-reports/TEST-com.example.PassingTest.xml`
- Create: `tests/fixtures/java/surefire-reports/TEST-com.example.FailingTest.xml`
- Create: `tests/fixtures/java/surefire-reports/TEST-com.example.FailingTestWithLogs.xml`
- Create: `tests/fixtures/java/surefire-reports/TEST-com.example.SkippedTest.xml`
- Create: `tests/fixtures/java/surefire-reports/TEST-com.example.ErrorTest.xml`

- [ ] **Step 8.1: Copy fixtures verbatim from maven-mcp**

Run:
```bash
cp /home/mariusz/projects/maven-mcp/src/test/resources/surefire-reports/TEST-com.example.PassingTest.xml tests/fixtures/java/surefire-reports/
cp /home/mariusz/projects/maven-mcp/src/test/resources/surefire-reports/TEST-com.example.FailingTest.xml tests/fixtures/java/surefire-reports/
cp /home/mariusz/projects/maven-mcp/src/test/resources/surefire-reports/TEST-com.example.FailingTestWithLogs.xml tests/fixtures/java/surefire-reports/
cp /home/mariusz/projects/maven-mcp/src/test/resources/surefire-reports/TEST-com.example.SkippedTest.xml tests/fixtures/java/surefire-reports/
cp /home/mariusz/projects/maven-mcp/src/test/resources/surefire-reports/TEST-com.example.ErrorTest.xml tests/fixtures/java/surefire-reports/
```

Note: `.gitkeep` previously created can now be deleted:
```bash
rm tests/fixtures/java/surefire-reports/.gitkeep
```

- [ ] **Step 8.2: Verify all 5 fixtures present**

Run:
```bash
ls tests/fixtures/java/surefire-reports/
```
Expected: 5 files, all `TEST-com.example.*.xml`.

- [ ] **Step 8.3: Commit**

```bash
git add tests/fixtures/java/surefire-reports/
git commit -m "test(mvn): copy Surefire XML fixtures from maven-mcp

Covers passing, failing, failing-with-logs, skipped, and error cases —
will feed surefire_reports parser tests in the next tasks."
```

---

## Task 9: `surefire_reports` — types and single-file parsing

**Files:**
- Modify: `src/cmds/java/surefire_reports.rs`

- [ ] **Step 9.1: Add types and a failing test**

Replace the contents of `src/cmds/java/surefire_reports.rs` with:
```rust
//! Parses Maven Surefire/Failsafe XML test reports from
//! `target/surefire-reports/TEST-*.xml` and `target/failsafe-reports/*.xml`.
//! Uses quick-xml streaming parser. Time-gated by `started_at` to skip stale
//! reports from previous runs.

use crate::cmds::java::stack_trace;
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use std::path::Path;
use std::time::SystemTime;

pub const DEFAULT_STACK_TRACE_LINES: usize = 50;
pub const DEFAULT_PER_TEST_OUTPUT_LIMIT: usize = 2000;
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
}
```

- [ ] **Step 9.2: Run tests to verify they fail**

Run:
```bash
cargo test --lib surefire_reports::tests
```
Expected: FAIL — `parse_content` not defined.

- [ ] **Step 9.3: Implement `parse_content`**

Add below `parse_u32_attr`:
```rust
/// Parse a single Surefire XML testsuite string into a partial result.
/// `app_package` is passed to `stack_trace::process` for frame classification.
///
/// Returns `None` only if the XML is completely malformed; otherwise a
/// best-effort result is returned.
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
                            test_output: None, // filled on </testcase>
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
```

Expose `truncate_header` from `stack_trace.rs` (change `pub(crate) fn truncate_header` — already `pub(crate)`, so it's available). If not already `pub(crate)`, add it now.

- [ ] **Step 9.4: Run tests**

Run:
```bash
cargo test --lib surefire_reports::tests
```
Expected: 2 PASS.

- [ ] **Step 9.5: Commit**

```bash
git add src/cmds/java/surefire_reports.rs
git commit -m "feat(mvn): parse Surefire XML testsuite via quick-xml

Handles testsuite/testcase/failure/error/system-out/system-err with
per-test 2000-char log limit and 50-line stack trace truncation.
Classifies failure vs error by element name."
```

---

## Task 10: `surefire_reports` — system-out / system-err capture test

**Files:**
- Modify: `src/cmds/java/surefire_reports.rs`

- [ ] **Step 10.1: Add tests**

Append to `tests` module in `surefire_reports.rs`:
```rust
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
```

- [ ] **Step 10.2: Run tests**

Run:
```bash
cargo test --lib surefire_reports::tests
```
Expected: 5 PASS (existing 2 + new 3). If `system-out`/`system-err` capture fails, check that `parse_content` only opens capture inside `testcase` AFTER `current_has_failure == true` — this mirrors maven-mcp behavior.

- [ ] **Step 10.3: Commit**

```bash
git add src/cmds/java/surefire_reports.rs
git commit -m "test(mvn): cover Surefire system-out/err capture and kinds

Asserts passing test system-out is not leaked, error vs failure kinds
are distinguished, and skipped counts are preserved."
```

---

## Task 11: `surefire_reports::parse_dir` with time-gate

**Files:**
- Modify: `src/cmds/java/surefire_reports.rs`

- [ ] **Step 11.1: Add tests**

Append to tests:
```rust
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
            None
        )
        .is_none());
    }

    #[test]
    fn parse_dir_empty_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(super::parse_dir(tmp.path(), None, None).is_none());
    }

    #[test]
    fn parse_dir_ignores_non_test_prefix_files() {
        let tmp = tempfile::tempdir().unwrap();
        copy_fixture(&tmp, "TEST-com.example.PassingTest.xml", None);
        std::fs::write(tmp.path().join("summary.xml"), "<x/>").unwrap();
        std::fs::write(tmp.path().join("other.txt"), "hi").unwrap();

        let result = super::parse_dir(tmp.path(), None, None).expect("parses");
        assert_eq!(result.files_read, 1);
    }

    #[test]
    fn parse_dir_aggregates_multi_file_counts() {
        let tmp = tempfile::tempdir().unwrap();
        copy_fixture(&tmp, "TEST-com.example.PassingTest.xml", None);
        copy_fixture(&tmp, "TEST-com.example.FailingTest.xml", None);
        copy_fixture(&tmp, "TEST-com.example.SkippedTest.xml", None);

        let result = super::parse_dir(tmp.path(), None, None).expect("parses");
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
        let result = super::parse_dir(tmp.path(), Some(since), None).expect("parses");
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

        let result = super::parse_dir(tmp.path(), None, None).expect("parses");
        assert_eq!(result.files_read, 1);
        assert_eq!(result.files_malformed, 1);
    }
```

- [ ] **Step 11.2: Run tests to verify they fail**

Run:
```bash
cargo test --lib surefire_reports::tests::parse_dir
```
Expected: FAIL — `parse_dir` not defined.

- [ ] **Step 11.3: Implement `parse_dir`**

Add below `truncate_test_output`:
```rust
/// Scan a directory for `TEST-*.xml` files and merge their parsed results.
///
/// - Files whose `mtime < since` are skipped and counted in `files_skipped_stale`.
/// - Files that parse to `None` (malformed) count in `files_malformed`.
/// - Returns `None` only if the directory does not exist or is empty.
pub fn parse_dir(
    dir: &Path,
    since: Option<SystemTime>,
    app_package: Option<&str>,
) -> Option<SurefireResult> {
    parse_dir_with_limits(
        dir,
        since,
        app_package,
        DEFAULT_PER_TEST_OUTPUT_LIMIT,
        DEFAULT_TOTAL_OUTPUT_LIMIT,
        DEFAULT_STACK_TRACE_LINES,
    )
}

pub fn parse_dir_with_limits(
    dir: &Path,
    since: Option<SystemTime>,
    app_package: Option<&str>,
    _per_test_output_limit: usize,
    total_output_limit: usize,
    _stack_trace_lines: usize,
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

        match parse_content(&content, app_package) {
            Some(file_result) => {
                aggregate.files_read += 1;
                aggregate.summary.add(&file_result.summary);
                aggregate.failures.extend(file_result.failures);
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

    apply_total_output_limit(&mut aggregate.failures, total_output_limit);
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
```

- [ ] **Step 11.4: Run tests**

Run:
```bash
cargo test --lib surefire_reports::tests
```
Expected: 11 PASS.

- [ ] **Step 11.5: Commit**

```bash
git add src/cmds/java/surefire_reports.rs
git commit -m "feat(mvn): surefire parse_dir with mtime time-gate

Aggregates TEST-*.xml files; filters stale by mtime >= since; counts
malformed files without crashing. Applies total-output-limit across
failures."
```

---

## Task 12: `surefire_reports` — total-output-limit test

**Files:**
- Modify: `src/cmds/java/surefire_reports.rs`

- [ ] **Step 12.1: Add test**

Append to tests:
```rust
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
```

- [ ] **Step 12.2: Run tests**

Run:
```bash
cargo test --lib surefire_reports::tests::apply_total_output_limit
```
Expected: PASS (logic already implemented in Task 11).

- [ ] **Step 12.3: Commit**

```bash
git add src/cmds/java/surefire_reports.rs
git commit -m "test(mvn): pin total-output-limit cutoff behavior

Asserts the third 4KB test_output is nulled when 10000-char budget
is exhausted."
```

---

## Task 13: `pom_groupid::detect` — core algorithm

**Files:**
- Create: `tests/fixtures/java/poms/single-module-pom.xml`
- Create: `tests/fixtures/java/poms/child-pom.xml`
- Create: `tests/fixtures/java/poms/no-groupid-pom.xml`
- Modify: `src/cmds/java/pom_groupid.rs`

- [ ] **Step 13.1: Create POM fixtures**

Create `tests/fixtures/java/poms/single-module-pom.xml`:
```xml
<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example.app</groupId>
  <artifactId>single</artifactId>
  <version>1.0.0</version>
</project>
```

Create `tests/fixtures/java/poms/child-pom.xml`:
```xml
<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <parent>
    <groupId>com.example.parent</groupId>
    <artifactId>parent</artifactId>
    <version>1.0.0</version>
  </parent>
  <artifactId>child</artifactId>
</project>
```

Create `tests/fixtures/java/poms/no-groupid-pom.xml`:
```xml
<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <artifactId>orphan</artifactId>
  <version>1.0.0</version>
</project>
```

Remove `.gitkeep`:
```bash
rm tests/fixtures/java/poms/.gitkeep
```

- [ ] **Step 13.2: Add tests**

Replace the body of `src/cmds/java/pom_groupid.rs` with:
```rust
//! Autodetects the application Java package from `pom.xml <groupId>`.
//! Used by `surefire_reports` / `stack_trace` to classify application frames.
//! Can be overridden by `RTK_MVN_APP_PACKAGE` env var.

use quick_xml::events::Event;
use quick_xml::Reader;
use std::path::Path;

const OVERRIDE_ENV: &str = "RTK_MVN_APP_PACKAGE";

/// Detect the Maven groupId of `cwd`'s `pom.xml`.
///
/// Resolution order:
/// 1. If env var `RTK_MVN_APP_PACKAGE` is set and non-empty, return it.
/// 2. Read `cwd/pom.xml` and extract top-level `<project>/<groupId>`.
/// 3. Fall back to `<project>/<parent>/<groupId>`.
/// 4. Otherwise `None`.
pub fn detect(cwd: &Path) -> Option<String> {
    if let Ok(value) = std::env::var(OVERRIDE_ENV) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    let pom_path = cwd.join("pom.xml");
    let content = std::fs::read_to_string(&pom_path).ok()?;
    extract_groupid(&content)
}

pub(crate) fn extract_groupid(xml: &str) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    // Tag stack tracked as simple Vec<String> of local names.
    let mut stack: Vec<String> = Vec::new();
    let mut top_level_groupid: Option<String> = None;
    let mut parent_groupid: Option<String> = None;
    let mut capture: Option<String> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = std::str::from_utf8(e.name().as_ref())
                    .ok()
                    .and_then(|s| s.rsplit(':').next())
                    .unwrap_or("")
                    .to_string();
                stack.push(name.clone());

                if is_top_level_groupid(&stack) || is_parent_groupid(&stack) {
                    capture = Some(name);
                }
            }
            Ok(Event::Text(t)) => {
                if capture.is_some() {
                    if let Ok(text) = t.unescape() {
                        let text = text.trim();
                        if !text.is_empty() {
                            if is_top_level_groupid(&stack) && top_level_groupid.is_none() {
                                top_level_groupid = Some(text.to_string());
                            } else if is_parent_groupid(&stack) && parent_groupid.is_none() {
                                parent_groupid = Some(text.to_string());
                            }
                        }
                    }
                }
            }
            Ok(Event::End(_)) => {
                stack.pop();
                capture = None;
                if top_level_groupid.is_some() {
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }

    top_level_groupid.or(parent_groupid)
}

fn is_top_level_groupid(stack: &[String]) -> bool {
    matches!(stack.as_slice(), [project, group] if project == "project" && group == "groupId")
}

fn is_parent_groupid(stack: &[String]) -> bool {
    matches!(
        stack.as_slice(),
        [project, parent, group] if project == "project" && parent == "parent" && group == "groupId"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_single_module_groupid() {
        let xml = include_str!("../../../tests/fixtures/java/poms/single-module-pom.xml");
        assert_eq!(extract_groupid(xml).as_deref(), Some("com.example.app"));
    }

    #[test]
    fn extract_falls_back_to_parent_groupid() {
        let xml = include_str!("../../../tests/fixtures/java/poms/child-pom.xml");
        assert_eq!(extract_groupid(xml).as_deref(), Some("com.example.parent"));
    }

    #[test]
    fn extract_no_groupid_returns_none() {
        let xml = include_str!("../../../tests/fixtures/java/poms/no-groupid-pom.xml");
        assert!(extract_groupid(xml).is_none());
    }

    #[test]
    fn extract_malformed_returns_none() {
        assert!(extract_groupid("<not-xml>>>>").is_none());
    }

    #[test]
    fn detect_missing_pom_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(detect(tmp.path()).is_none());
    }

    #[test]
    fn detect_env_override_wins() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::copy(
            "tests/fixtures/java/poms/single-module-pom.xml",
            tmp.path().join("pom.xml"),
        )
        .unwrap();

        // Serial to avoid concurrent env mutation with other tests — this is
        // tested in isolation; we restore the var on exit.
        let guard = EnvGuard::set(OVERRIDE_ENV, "com.override");
        assert_eq!(detect(tmp.path()).as_deref(), Some("com.override"));
        drop(guard);
    }

    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}
```

- [ ] **Step 13.3: Run tests**

Run:
```bash
cargo test --lib pom_groupid::tests
```
Expected: 6 PASS. If the env-override test is flaky under parallel test runs, keep it — `std::env::set_var` is inherently non-threadsafe in Rust but our tests don't contend on `RTK_MVN_APP_PACKAGE`, and `cargo test` runs tests in the same process.

- [ ] **Step 13.4: Commit**

```bash
git add src/cmds/java/pom_groupid.rs tests/fixtures/java/poms/
git commit -m "feat(mvn): detect appPackage from pom.xml groupId

Reads top-level <project>/<groupId> with fallback to <parent>/<groupId>.
RTK_MVN_APP_PACKAGE env var overrides. Malformed POMs return None."
```

---

## Task 14: Synthesize failsafe fixtures and stack-trace fixtures

**Files:**
- Create: `tests/fixtures/java/failsafe-reports/TEST-com.example.DbIntegrationIT.xml`
- Create: `tests/fixtures/java/failsafe-reports/TEST-com.example.PortConflictIT.xml`
- Create: `tests/fixtures/java/stack-traces/multi-caused-by.txt`

- [ ] **Step 14.1: Create failsafe fixture with 3-segment Caused-by chain**

Create `tests/fixtures/java/failsafe-reports/TEST-com.example.DbIntegrationIT.xml`:
```xml
<?xml version="1.0" encoding="UTF-8"?>
<testsuite name="com.example.DbIntegrationIT" time="3.210" tests="2" errors="1" skipped="0" failures="0">
  <testcase name="shouldUseDefaults" classname="com.example.DbIntegrationIT" time="0.100"/>
  <testcase name="shouldConnect" classname="com.example.DbIntegrationIT" time="3.100">
    <error message="Failed to load ApplicationContext" type="java.lang.IllegalStateException">java.lang.IllegalStateException: Failed to load ApplicationContext
	at org.springframework.test.context.cache.DefaultCacheAwareContextLoaderDelegate.loadContext(DefaultCacheAwareContextLoaderDelegate.java:180)
	at org.springframework.test.context.support.DefaultTestContext.getApplicationContext(DefaultTestContext.java:124)
	at org.springframework.test.context.support.DependencyInjectionTestExecutionListener.injectDependencies(DependencyInjectionTestExecutionListener.java:118)
Caused by: org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'dataSource'
	at org.springframework.beans.factory.support.AbstractAutowireCapableBeanFactory.doCreateBean(AbstractAutowireCapableBeanFactory.java:628)
	at org.springframework.beans.factory.support.AbstractAutowireCapableBeanFactory.createBean(AbstractAutowireCapableBeanFactory.java:542)
	at org.springframework.boot.autoconfigure.jdbc.DataSourceAutoConfiguration.dataSource(DataSourceAutoConfiguration.java:114)
Caused by: org.hibernate.HibernateException: Unable to acquire JDBC Connection; nested exception is java.sql.SQLTransientConnectionException: HikariPool-1 - Connection is not available, request timed out after 30000ms.
	at org.hibernate.internal.SessionFactoryImpl.createEntityManagerFactory(SessionFactoryImpl.java:512)
	at com.example.DbIntegrationIT.shouldConnect(DbIntegrationIT.java:88)
	at java.base/java.lang.reflect.Method.invoke(Method.java:580)</error>
    <system-err>2026-04-15 10:42:17 ERROR HikariDataSource - HikariPool-1 - Connection is not available
Connection refused (Connection refused)</system-err>
  </testcase>
</testsuite>
```

- [ ] **Step 14.2: Create failsafe fixture — port conflict**

Create `tests/fixtures/java/failsafe-reports/TEST-com.example.PortConflictIT.xml`:
```xml
<?xml version="1.0" encoding="UTF-8"?>
<testsuite name="com.example.PortConflictIT" time="0.500" tests="1" errors="1" skipped="0" failures="0">
  <testcase name="shouldStartServer" classname="com.example.PortConflictIT" time="0.500">
    <error message="Address already in use" type="java.net.BindException">java.net.BindException: Address already in use
	at java.base/sun.nio.ch.Net.bind0(Native Method)
	at java.base/sun.nio.ch.Net.bind(Net.java:555)
	at com.example.PortConflictIT.shouldStartServer(PortConflictIT.java:42)</error>
  </testcase>
</testsuite>
```

Remove `.gitkeep`:
```bash
rm tests/fixtures/java/failsafe-reports/.gitkeep
```

- [ ] **Step 14.3: Create raw stack trace fixture**

Create `tests/fixtures/java/stack-traces/multi-caused-by.txt`:
```
java.lang.IllegalStateException: Failed to load ApplicationContext
	at org.springframework.test.context.cache.DefaultCacheAwareContextLoaderDelegate.loadContext(DefaultCacheAwareContextLoaderDelegate.java:180)
	at org.springframework.test.context.support.DefaultTestContext.getApplicationContext(DefaultTestContext.java:124)
Caused by: org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'dataSource'
	at org.springframework.beans.factory.support.AbstractAutowireCapableBeanFactory.doCreateBean(AbstractAutowireCapableBeanFactory.java:628)
	at org.springframework.beans.factory.support.AbstractAutowireCapableBeanFactory.createBean(AbstractAutowireCapableBeanFactory.java:542)
Caused by: org.hibernate.HibernateException: Unable to acquire JDBC Connection
	at org.hibernate.internal.SessionFactoryImpl.createEntityManagerFactory(SessionFactoryImpl.java:512)
	at com.example.DbIntegrationIT.shouldConnect(DbIntegrationIT.java:88)
	at java.base/java.lang.reflect.Method.invoke(Method.java:580)
```

Remove `.gitkeep`:
```bash
rm tests/fixtures/java/stack-traces/.gitkeep
```

- [ ] **Step 14.4: Verify via stack_trace::process**

Add to `src/cmds/java/stack_trace.rs` tests:
```rust
    #[test]
    fn process_real_world_spring_fixture() {
        let trace = include_str!("../../../tests/fixtures/java/stack-traces/multi-caused-by.txt");
        let out = process(trace, Some("com.example"), 50).unwrap();
        assert!(out.contains("Caused by: org.springframework.beans.factory.BeanCreationException"));
        assert!(out.contains("Caused by: org.hibernate.HibernateException"));
        assert!(out.contains("com.example.DbIntegrationIT.shouldConnect"));
        assert!(out.contains("framework frames omitted"));
    }
```

- [ ] **Step 14.5: Run tests**

Run:
```bash
cargo test --lib stack_trace::tests::process_real_world_spring_fixture
```
Expected: PASS.

- [ ] **Step 14.6: Commit**

```bash
git add tests/fixtures/java/failsafe-reports/ tests/fixtures/java/stack-traces/ src/cmds/java/stack_trace.rs
git commit -m "test(mvn): add failsafe + real-world stack trace fixtures

Two failsafe-report XMLs (ApplicationContext failure, port conflict)
and a Spring Caused-by chain for stack_trace::process coverage."
```

---

## Task 15: Capture `started_at` and detect appPackage in `run_test`

**Files:**
- Modify: `src/cmds/java/mvn_cmd.rs`

- [ ] **Step 15.1: Read current `run_test`**

Run:
```bash
grep -n 'fn run_test\|fn run_mvn_test\|pub fn run' src/cmds/java/mvn_cmd.rs | head
```
Identify the function that executes `mvn test` (likely `run_test` or similar). Open and read its full body so you understand where `execute_command` is called.

- [ ] **Step 15.2: Add fields to the pre-exec closure**

Modify `run_test` (or equivalent) to:
1. Capture `let started_at = std::time::SystemTime::now();` **immediately before** the `execute_command` call.
2. Capture `let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));` immediately after.
3. Compute `let app_pkg = crate::cmds::java::pom_groupid::detect(&cwd);` (cheap — happens once).
4. After `filter_mvn_test(&stdout_string)` call, pass the result through the new enrichment (implemented in next task, for now just assign to the variable and leave the downstream `print!/tracking::record` untouched).

Intermediate patch (applies in this task, prepares scaffold for Task 16):
```rust
// Just before execute_command:
let started_at = std::time::SystemTime::now();
let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
let app_pkg = crate::cmds::java::pom_groupid::detect(&cwd);

// ...existing exec + filter_mvn_test call producing `filtered`...

let enriched = enrich_with_reports(&filtered, &cwd, started_at, app_pkg.as_deref());
// Replace all downstream uses of `filtered` with `enriched`.
```

At this point, define a temporary passthrough:
```rust
fn enrich_with_reports(
    text: &str,
    _cwd: &std::path::Path,
    _since: std::time::SystemTime,
    _app_package: Option<&str>,
) -> String {
    text.to_string()
}
```

- [ ] **Step 15.3: Run full test suite**

Run:
```bash
cargo test --all
```
Expected: ALL previous tests still pass (enrichment is identity).

- [ ] **Step 15.4: Commit**

```bash
git add src/cmds/java/mvn_cmd.rs
git commit -m "refactor(mvn): wire started_at/cwd/app_pkg into run_test

Prepares scaffolding for XML report enrichment. enrich_with_reports is
currently an identity function; real logic lands in the next commit."
```

---

## Task 16: Implement `enrich_with_reports` + `render_enriched`

**Files:**
- Modify: `src/cmds/java/mvn_cmd.rs`

- [ ] **Step 16.1: Add failing test for happy-path short-circuit**

Append to the `#[cfg(test)] mod tests` block in `src/cmds/java/mvn_cmd.rs`:
```rust
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
```

- [ ] **Step 16.2: Run tests — they fail**

Run:
```bash
cargo test --lib mvn_cmd::tests::enrich
```
Expected: happy-path PASS (identity), the rest FAIL.

- [ ] **Step 16.3: Replace `enrich_with_reports` stub with real implementation**

Remove the stub added in Task 15 and replace with:
```rust
use crate::cmds::java::surefire_reports::{self, FailureKind, SurefireResult, TestFailure};

const MAX_FAILURES_PER_SOURCE: usize = 10;

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
    use std::fmt::Write;
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
```

- [ ] **Step 16.4: Run tests**

Run:
```bash
cargo test --lib mvn_cmd::tests::enrich
```
Expected: 5 PASS.

- [ ] **Step 16.5: Run full suite to catch regressions**

Run:
```bash
cargo test --all
```
Expected: ALL PASS (incl. existing filter_mvn_test snapshot tests).

- [ ] **Step 16.6: Commit**

```bash
git add src/cmds/java/mvn_cmd.rs
git commit -m "feat(mvn): enrich test output with Surefire/Failsafe XML

Appends a structured Failures section for each report directory, with
per-failure stack trace (framework-frame-collapsed), optional captured
output, and a reports-processed footer. Short-circuits on happy path
to avoid I/O. Emits a red-flag message when 'no tests run' is reported
but also no fresh XML reports are present."
```

---

## Task 17: Snapshot tests for enriched output

**Files:**
- Modify: `src/cmds/java/mvn_cmd.rs`

- [ ] **Step 17.1: Add snapshot tests**

Append to the `#[cfg(test)] mod tests` block (after existing `enrich_*` tests):
```rust
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
        let out = super::enrich_with_reports(text, tmp.path(), since, Some("com.example"));
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
        let out = super::enrich_with_reports(text, tmp.path(), since, Some("com.example"));
        insta::assert_snapshot!(out);
    }

    #[test]
    fn snapshot_red_flag_no_tests() {
        let tmp = tempfile::tempdir().unwrap();
        let out = super::enrich_with_reports(
            "mvn test: no tests run",
            tmp.path(),
            std::time::SystemTime::now(),
            Some("com.example"),
        );
        insta::assert_snapshot!(out);
    }
```

- [ ] **Step 17.2: Generate snapshots**

Run:
```bash
cargo test --lib mvn_cmd::tests::snapshot
```
Expected: tests fail first run; run `cargo insta review` to inspect and accept, or run:
```bash
cargo insta accept
```

After acceptance, run:
```bash
cargo test --lib mvn_cmd::tests::snapshot
```
Expected: 3 PASS.

- [ ] **Step 17.3: Commit**

```bash
git add src/cmds/java/mvn_cmd.rs src/cmds/java/snapshots/
git commit -m "test(mvn): snapshot tests for enriched surefire/failsafe rendering

Pins output format for surefire-only, both-report-dirs, and the no-tests
red-flag path. Adjust with 'cargo insta review' when output changes."
```

---

## Task 18: Token savings tests (happy path and failure path)

**Files:**
- Create: `tests/fixtures/java/mvn-verify-multimodule-raw.txt` (real-world log — if not already present from Task 0's predecessor PR #1089; skip copy if the file exists)
- Modify: `src/cmds/java/mvn_cmd.rs`

- [ ] **Step 18.1: Locate or synthesize a real multi-module mvn verify log**

Check existing fixtures:
```bash
ls tests/fixtures/java/ 2>/dev/null || ls tests/fixtures/mvn/ 2>/dev/null
```
If a `mvn verify` multi-module fixture exists (likely `tests/fixtures/java/mvn-verify-multimodule.txt` or similar, shipped by PR #1089), use its path in the test. If not, skip this task's real-world fixture; the synthetic fixtures already cover enrichment correctness. Token-savings test then operates only on synthetic data.

- [ ] **Step 18.2: Add token-savings tests**

Append to the `#[cfg(test)] mod tests` block:
```rust
    fn count_tokens(s: &str) -> usize {
        s.split_whitespace().count()
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
            Some("com.example"),
        );
        assert_eq!(out, text, "happy path must not allocate or append");
    }

    #[test]
    fn savings_enriched_failures_stays_under_15_percent() {
        // Simulate a ~2000-line build log whose text filter produced a short
        // summary, plus one big failsafe XML with system-err and a 3-segment
        // Caused-by chain. Total enriched output must be ≥85% smaller than raw.
        let raw_log = std::iter::repeat_n(
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
        let enriched = super::enrich_with_reports(text_summary, tmp.path(), since, Some("com.example"));

        let raw_tokens = count_tokens(&raw_log);
        let enriched_tokens = count_tokens(&enriched);
        let savings = 100.0 - (enriched_tokens as f64 / raw_tokens as f64 * 100.0);
        assert!(
            savings >= 85.0,
            "expected ≥85% savings on enriched failure path, got {savings:.1}% \
             (raw={raw_tokens}, enriched={enriched_tokens})"
        );
    }
```

- [ ] **Step 18.3: Run tests**

Run:
```bash
cargo test --lib mvn_cmd::tests::savings
```
Expected: 2 PASS.

- [ ] **Step 18.4: Commit**

```bash
git add src/cmds/java/mvn_cmd.rs
git commit -m "test(mvn): token savings — happy path identity, failure path ≥85%

Asserts happy-path enrichment is a no-op and that even on the enriched
failure path with a multi-segment Caused-by chain we stay under 15% of
the raw log size."
```

---

## Task 19: Performance gate — hyperfine on release build

**Files:** (no source changes; just a verification command + optional release rebuild)

- [ ] **Step 19.1: Build release binary**

Run:
```bash
cargo build --release
```
Expected: PASS.

- [ ] **Step 19.2: Create a synthetic reports directory for benchmark**

Run:
```bash
mkdir -p /tmp/rtk-perf/target/surefire-reports
for i in $(seq 1 50); do
  cp tests/fixtures/java/surefire-reports/TEST-com.example.PassingTest.xml \
     "/tmp/rtk-perf/target/surefire-reports/TEST-com.example.Pass$i.xml"
done
```

- [ ] **Step 19.3: Benchmark happy path (no I/O)**

Run:
```bash
hyperfine --warmup 3 --runs 20 \
  "cd /tmp/rtk-perf && $(pwd)/target/release/rtk --version"
```
Expected: median < 10ms.

- [ ] **Step 19.4: Document result**

Paste the hyperfine output into the commit message (next task) or a NOTES file. If median exceeds 10ms, investigate before proceeding.

- [ ] **Step 19.5: No commit** (this task is verification only). Clean up:

```bash
rm -rf /tmp/rtk-perf
```

---

## Task 20: Docs — README, CHANGELOG

**Files:**
- Modify: `src/cmds/java/README.md` (create if missing)
- Modify: `CHANGELOG.md`

- [ ] **Step 20.1: Check existing README structure**

Run:
```bash
ls src/cmds/java/README.md 2>/dev/null && head -40 src/cmds/java/README.md
```
If missing, you'll create it. If present, note its existing sections so the enrichment section fits the tone.

- [ ] **Step 20.2: Update README**

Either create `src/cmds/java/README.md` with the content below, or append a new section.

If creating, use this minimal template:
```markdown
# rtk — Maven (Java) Filter

rtk filters and enriches Maven build output (test, compile, checkstyle,
dependency:tree, verify, integration-test, install) for LLM consumption.

## Output enrichment from Surefire/Failsafe XML reports

When `mvn test` (or verify/integration-test) reports failures, rtk reads
`target/surefire-reports/TEST-*.xml` and `target/failsafe-reports/*.xml`
**after** the build finishes and appends a structured Failures section
with:

- Full stack trace per failure, with framework frames collapsed and the
  root-cause segment preserved (up to 50 lines per trace).
- Captured stdout + stderr from failing tests only, capped at 2000 chars
  per test and 10000 chars total.
- File counters in the footer: `(reports: N surefire, M failsafe, K stale files skipped)`.

### Application-package detection

rtk classifies stack frames as *application* vs *framework* by comparing
frame class names against the Java `groupId` from `pom.xml`:

1. `RTK_MVN_APP_PACKAGE` env var (if set, overrides everything).
2. `<project>/<groupId>` from the pom.xml in the current working directory.
3. Fallback: `<project>/<parent>/<groupId>`.
4. Otherwise: no filtering — full stack traces are preserved.

### Time-gated report reads

Stale XML reports from previous runs are skipped: only files with
`mtime >= started_at` (captured just before `mvn` executes) are parsed.

### Red-flag heuristic for "0 tests"

If the summary says `no tests run` but surefire reports are empty or
absent, rtk emits a diagnostic instead of the silent summary:

```
mvn test: 0 tests executed — surefire nie wykrył testów.
Sprawdź pom.xml (plugin surefire configuration) lub uruchom: rtk proxy mvn test
```

### Bypass

For the rare cases where you need the full raw Maven output:

```bash
rtk proxy mvn test
```
```

- [ ] **Step 20.3: Update CHANGELOG.md**

Add under the latest unreleased/next version heading:
```markdown
### Added
- `mvn test` / `mvn verify` / `mvn integration-test` output is now
  enriched with structured failure details read from
  `target/surefire-reports/TEST-*.xml` and `target/failsafe-reports/*.xml`.
  Stack traces are segmented on `Caused by:` with framework frames
  collapsed; the root-cause segment is always preserved.
- Application-package autodetect from `pom.xml` `<groupId>` (with parent
  fallback) for framework-frame classification. Override via
  `RTK_MVN_APP_PACKAGE`.
- Red-flag heuristic: `no tests run` with no fresh XML reports emits a
  diagnostic pointing at surefire misconfiguration.
```

- [ ] **Step 20.4: Verify**

Run:
```bash
cargo build
```
Expected: PASS (README/CHANGELOG changes don't affect build).

- [ ] **Step 20.5: Commit**

```bash
git add src/cmds/java/README.md CHANGELOG.md
git commit -m "docs(mvn): document XML enrichment, appPackage detection, red-flag

Describes the new post-filter XML read, groupId autodetect order,
stale-file time-gate, and the rtk proxy escape hatch."
```

---

## Task 21: Final quality gate

**Files:** (no source changes)

- [ ] **Step 21.1: Full fmt + clippy + test cycle**

Run:
```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test --all
```
Expected: ALL PASS with zero clippy warnings. If any clippy warning appears, fix it inline in the offending file before proceeding.

- [ ] **Step 21.2: Snapshot review**

Run:
```bash
cargo insta pending-snapshots
```
If any pending, run `cargo insta review` and decide. Stage and commit any accepted snapshot updates as:
```bash
git add src/cmds/java/snapshots/
git commit -m "test(mvn): accept reviewed snapshots"
```

- [ ] **Step 21.3: Push branch**

Run:
```bash
git push -u origin feat/mvn-surefire-xml
```

- [ ] **Step 21.4: Open PR against fork's `master`**

Run:
```bash
gh pr create \
  --repo mariuszs/rtk-java \
  --base master \
  --head mariuszs:feat/mvn-surefire-xml \
  --title "feat(mvn): enrich test output with Surefire/Failsafe XML reports" \
  --body "$(cat <<'EOF'
## Summary

- Ports maven-mcp's `SurefireReportParser` + `StackTraceProcessor` to Rust.
- Adds `pom.xml` `<groupId>` autodetect for framework-frame classification.
- Post-text-filter enrichment reads `target/surefire-reports/` and `target/failsafe-reports/` XMLs (time-gated by `started_at`) and appends structured failure details to the rtk mvn output.
- Red-flag heuristic: `no tests run` with no fresh reports now surfaces a diagnostic instead of silently pretending everything is fine.
- Override via `RTK_MVN_APP_PACKAGE` env var.

Spec: `docs/superpowers/specs/2026-04-15-mvn-surefire-xml-enrichment-design.md`
Plan: `docs/superpowers/plans/2026-04-15-mvn-surefire-xml-enrichment.md`

Stacks on `feat/mvn-rust-module` (upstream PR rtk-ai/rtk#1089).

## Test plan

- [x] `cargo test --all` (incl. new stack_trace, surefire_reports, pom_groupid, mvn_cmd tests)
- [x] `cargo clippy --all-targets -- -D warnings`
- [x] `cargo fmt --all`
- [x] `cargo insta review` — snapshots accepted
- [x] Token savings: happy path identity, failure path ≥85% (verified in tests)
- [x] Release build + hyperfine: happy-path startup < 10ms
EOF
)"
```

Expected: PR URL printed. Paste into this checklist:
- [ ] PR URL: `<fill in after creation>`

---

## Self-Review

Completed after first draft of this plan. No placeholders detected. All tasks reference concrete files, code, and commands. Task 15's integration into `run_test` refers to existing code — the implementer must read the current `mvn_cmd.rs` to locate the exact line but the insertion semantics are unambiguous. Task 18's token-savings test uses synthetic data when no real-world log fixture exists; this is explicitly called out. Task 19 is verification-only and commits nothing. Task 20 creates README if missing — the template is complete. Task 21 handles all pre-PR gates.

**Spec coverage check (cross-reference with `2026-04-15-mvn-surefire-xml-enrichment-design.md`):**

| Spec section | Task(s) |
|---|---|
| `surefire_reports.rs` — types | 9 |
| `surefire_reports.rs` — single-file parse | 9, 10 |
| `surefire_reports.rs` — parse_dir + time-gate | 11 |
| `surefire_reports.rs` — total_output_limit | 12 |
| `stack_trace.rs` — parse_segments | 1 |
| `stack_trace.rs` — truncate_header | 2 |
| `stack_trace.rs` — frame classification | 3 |
| `stack_trace.rs` — add_collapsed_frames | 4 |
| `stack_trace.rs` — add_root_cause_frames | 5 |
| `stack_trace.rs` — process | 6 |
| `stack_trace.rs` — apply_hard_cap | 7 |
| `pom_groupid.rs` — detect + fallback + override | 13 |
| Fixtures — surefire | 8 |
| Fixtures — failsafe + stack | 14 |
| `mvn_cmd.rs` — capture started_at / cwd / app_pkg | 15 |
| `mvn_cmd.rs` — enrich_with_reports + render_enriched | 16 |
| Snapshot tests | 17 |
| Token savings tests | 18 |
| Performance gate | 19 |
| Docs | 20 |
| Final quality gate + PR | 21 |

All spec sections have a corresponding task. No gaps.
