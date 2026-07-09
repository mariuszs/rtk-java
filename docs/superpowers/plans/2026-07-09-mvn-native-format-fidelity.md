# mvn native-format fidelity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reshape RTK's `mvn` output into a prefix-preserving subset of Maven's own lines (plus XML enrichment in Maven's shape), deleting every synthetic `mvn <goal>:` headline and RTK-invented header.

**Architecture:** Single module `src/cmds/jvm/mvn_cmd.rs` composes each goal's summary. Today it *synthesizes* RTK one-liners and *strips* `[INFO]`/`[ERROR]` prefixes; the compile path is the exception (already a prefix-preserving subset). This plan aligns every surface to the compile path: keep Maven's log-level prefixes verbatim, emit Maven's own aggregate/BUILD/Failures lines, and render XML enrichment in surefire's line shape. The guard (`never_worse`) already wraps every emit path via `runner::run_filtered` — unchanged.

**Tech Stack:** Rust, `insta` snapshots, `lazy_static` regex. No new dependencies.

## Global Constraints

- **No `unwrap()`** in production code; `.context("…")?` on `Result`. Tests use `expect(...)`.
- **Lazy regex only** — `lazy_static!`, never `Regex::new` in a hot path.
- **Fidelity:** filtered stdout must contain NO synthetic `mvn <goal>:` line, NO `(from surefire-reports/)` / `(from failsafe-reports/)` / `(multi-goal)` / `mvn: ok` / `classes:` marker, and NO literal `rtk`.
- **Prefixes preserved:** retained Maven lines keep their `[INFO]`/`[ERROR]`/`[WARNING]` prefix verbatim (agents anchor greps on `^\[INFO\]`/`^\[ERROR\]` — see spec).
- **No `Total time`** on any path (including the compile subset that retains it today).
- **Guard unchanged:** do not touch `runner::run_filtered`, `guard::never_worse`, or the tee wiring.
- **Gate after every task:** `cargo fmt --all && cargo clippy --all-targets && cargo test --bin rtk cmds::jvm`. (Full `cargo test --all` has 18 pre-existing `hooks::*` env-failures, green on CI — ignore those, not a regression.)
- Branch: `feat/mvn-native-fidelity` (fix #1 + spec already committed).

## File Structure

- `src/cmds/jvm/mvn_cmd.rs` — all changes: the test-summary composer (`filter_mvn_tests_with_goal`), enrichment renderers (`render_pass_inline`, `render_classes_digest`, `render_enriched`, `render_failure_block`, `strip_text_failures_block`, `finalize_enriched`, `enrich_with_reports`), the compile keep-rule, `filter_mvn_clean`, `filter_mvn_checkstyle`, `filter_mvn_dependency_tree`/`_list`, and the multi-goal layer (`extract_build_block`, `compose_multi`).
- `src/cmds/jvm/snapshots/*.snap` — regenerated via `cargo insta`.

**Contract shared between Task 1 (frame) and Task 2 (enrichment consumers).** `enrich_with_reports` receives `text_summary` and parses its shape. New contract Task 1 produces, Task 2 consumes:

| run | new `text_summary` |
|---|---|
| pass | `[INFO] Tests run: N, Failures: 0, Errors: 0, Skipped: S\n[INFO] BUILD SUCCESS` |
| fail | `[ERROR] Tests run: N, Failures: F, Errors: E, Skipped: S\n[INFO] BUILD FAILURE\n\n[ERROR] Failures:\n<lines>` |
| no tests | `[WARNING] No tests were executed!` |

Anchors that Task 2's consumers must match: pass suffix `\n[INFO] BUILD SUCCESS`; fail failures header `\n[ERROR] Failures:\n`; zero-tests via the new `[WARNING] No tests were executed!` string.

---

### Task 1: Prefix-preserving test-summary frame

**Files:**
- Modify: `src/cmds/jvm/mvn_cmd.rs` — `filter_mvn_tests_with_goal` (no-tests ~1558, aggregate ~1580, pass ~1585, fail ~1596); update the two anchor sites in `render_pass_inline` (~1234) and `strip_text_failures_block` (~1306) and the `enrich_with_reports` zero-tests detection (~1065) in lockstep.
- Test: same file, `#[cfg(test)] mod tests`.

**Interfaces:**
- Produces: `filter_mvn_tests_with_goal(output, goal, app_packages) -> String` now emits the prefix-preserving frame in the contract table above.
- Consumes: nothing new.

- [ ] **Step 1: Pin Maven's exact BUILD-FAILURE / aggregate prefixes from a real fixture**

Run: `grep -nE '^\[[A-Z]+\] (BUILD (SUCCESS|FAILURE)|Tests run:)' tests/fixtures/mvn_test_reactor_fail.txt tests/fixtures/mvn_test_pass_mavenmcp.txt`
Expected: confirms `[INFO] BUILD SUCCESS`, `[INFO] BUILD FAILURE`, and `[INFO] Tests run:` (pass) / `[ERROR] Tests run:` (fail). Use whatever the fixtures actually show as the literal strings in the steps below. (If a fixture disagrees, its literal wins.)

- [ ] **Step 2: Write failing tests for the new frame**

```rust
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
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --bin rtk cmds::jvm::mvn_cmd::tests::pass_frame_is_prefixed_maven_subset cmds::jvm::mvn_cmd::tests::fail_frame_is_prefixed_maven_subset cmds::jvm::mvn_cmd::tests::no_tests_uses_native_surefire_warning`
Expected: FAIL (current output still says `mvn test: 183 passed`, bare `Tests run:`, `mvn test: No tests run`).

- [ ] **Step 4: Rewrite the no-tests branch (~1558)**

Replace:
```rust
        // Maven casing ("No tests…") — agents grep with `No tests` verbatim.
        return format!("mvn {goal}: No tests run");
```
with:
```rust
        // Surefire's own native line — no synthetic `mvn <goal>:` prose.
        let _ = goal; // goal no longer interpolated into the no-tests line
        return "[WARNING] No tests were executed!".to_string();
```

- [ ] **Step 5: Rewrite the aggregate + pass + fail composition (~1580–1634)**

Replace the block from `let aggregate = format!(` through the end of the fail assembly (`result.trim().to_string()`) with the prefix-preserving version:
```rust
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
```
(Deletes the `time_str` usage in these lines; if `time_str` is now unused, delete its binding at ~1562 and the `parse_total_time`/`total_time` plumbing only if the compiler flags it unused — otherwise leave for other branches.)

- [ ] **Step 6: Update the two enrichment anchors + zero-tests detection (lockstep)**

In `render_pass_inline` (~1234):
```rust
    let (mut out, build_footer) = match text_summary.strip_suffix("\n[INFO] BUILD SUCCESS") {
        Some(head) => (head.to_string(), true),
        None => (text_summary.to_string(), false),
    };
```
and its footer re-append (~1258):
```rust
    if build_footer {
        out.push_str("\n[INFO] BUILD SUCCESS");
    }
```
In `strip_text_failures_block` (~1306):
```rust
    match text_summary.find("\n[ERROR] Failures:\n") {
```
In `enrich_with_reports` zero-tests detection (~1065): the summary no longer starts with `mvn ` and no longer ends with `: No tests run`. Replace the guard + detection:
```rust
    if !text_summary.starts_with("[INFO]")
        && !text_summary.starts_with("[ERROR]")
        && !text_summary.starts_with("[WARNING]")
    {
        return passthrough(text_summary.to_string());
    }

    let zero_tests = text_summary == "[WARNING] No tests were executed!";
    let has_failures = text_summary.contains("BUILD FAILURE");
```

- [ ] **Step 7: Run the new tests — expect PASS**

Run: `cargo test --bin rtk cmds::jvm::mvn_cmd::tests::pass_frame_is_prefixed_maven_subset cmds::jvm::mvn_cmd::tests::fail_frame_is_prefixed_maven_subset cmds::jvm::mvn_cmd::tests::no_tests_uses_native_surefire_warning`
Expected: PASS. Other jvm tests/snapshots will fail — fixed in Task 2 and Task 7.

- [ ] **Step 8: Commit**

```bash
git add src/cmds/jvm/mvn_cmd.rs
git commit -m "refactor(mvn): prefix-preserving test-summary frame

Emit Maven's own [INFO]/[ERROR] Tests run: + BUILD lines and the native
[WARNING] No tests were executed! instead of synthetic mvn <goal>: prose.
Drops Total time from the test path."
```

---

### Task 2: Enrichment in Maven's line shape

**Files:**
- Modify: `src/cmds/jvm/mvn_cmd.rs` — `render_pass_inline` breakdown lines (~1238), `render_classes_digest` header + lines (~1177), `render_enriched` headers (~1282/1289), `render_failure_block` (~1312), `finalize_enriched` (~1116), the two `enrich_with_reports` no-report branches (~1093).
- Test: same file.

**Interfaces:**
- Consumes: the Task 1 contract (`text_summary` shapes).
- Produces: enriched `text` where breakdown lines read `[INFO] Tests run: N -- in <FQCN>`, failure sections use `[ERROR] Failures:` / `[ERROR] Integration failures:`, digest reference is `[full per-class report: <path>]`.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn breakdown_uses_surefire_line_shape() {
    let tmp = tmp_with_reports(2);
    let since = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    let out = super::enrich_with_reports(
        "[INFO] Tests run: 24, Failures: 0, Errors: 0, Skipped: 0\n[INFO] BUILD SUCCESS",
        tmp.path(), since, &pkgs("com.example"), "test");
    assert!(out.text.contains("[INFO] Tests run: 12 -- in com.example.Suite0Test"),
        "surefire-shaped breakdown missing:\n{}", out.text);
    assert_eq!(out.text.lines().last(), Some("[INFO] BUILD SUCCESS"), "\n{}", out.text);
    assert!(!out.text.contains("Suite0Test:"), "old compact form leaked:\n{}", out.text);
}

#[test]
fn enriched_failures_header_is_native() {
    let out = super::render_enriched(
        "[ERROR] Tests run: 2, Failures: 2, Errors: 0, Skipped: 0\n[INFO] BUILD FAILURE",
        Some(&sf_result_with_two_failures()), None);
    assert!(out.contains("[ERROR] Failures:"), "\n{out}");
    assert!(!out.contains("(from surefire-reports/)"), "path-tell leaked:\n{out}");
    assert!(!out.contains("1. "), "RTK numbering leaked:\n{out}");
}

#[test]
fn digest_reference_uses_tee_hint_style() {
    let tmp = tmp_with_reports(8); // > MAX_INLINE_CLASSES -> reference line
    let since = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    let out = super::enrich_with_reports(
        "[INFO] Tests run: 96, Failures: 0, Errors: 0, Skipped: 0\n[INFO] BUILD SUCCESS",
        tmp.path(), since, &pkgs("com.example"), "test");
    let text = super::finalize_enriched(out, "mvn_test");
    assert!(text.contains("[full per-class report:"), "tee-hint-style ref missing:\n{text}");
    assert!(!text.contains("classes:"), "old RTK ref leaked:\n{text}");
}
```
(`sf_result_with_two_failures()` — reuse an existing test helper if present; otherwise build a `SurefireResult` with two `TestFailure`s using the same construction the neighbouring `render_failure_block` tests use.)

- [ ] **Step 2: Run — verify FAIL**

Run: `cargo test --bin rtk cmds::jvm::mvn_cmd::tests::breakdown_uses_surefire_line_shape cmds::jvm::mvn_cmd::tests::enriched_failures_header_is_native cmds::jvm::mvn_cmd::tests::digest_reference_uses_tee_hint_style`
Expected: FAIL.

- [ ] **Step 3: Breakdown → surefire line shape (`render_pass_inline` ~1238)**

Replace the inline-suite loop:
```rust
    if !suites.is_empty() && suites.len() <= MAX_INLINE_CLASSES {
        for s in &suites {
            // Reconstruct surefire's own per-class line (reactor-suppressed),
            // trimming zero-valued fields on a clean pass.
            write!(out, "\n[INFO] Tests run: {} -- in {}", s.tests, s.class_name).ok();
        }
    }
```
And the skipped loop (~1250) — keep it maven-plausible, drop the RTK `skipped:` prefix:
```rust
    if !skipped.is_empty() && skipped.len() <= MAX_INLINE_SKIPPED {
        for st in &skipped {
            write!(out, "\n[INFO] Tests run: 0, Skipped: 1 -- in {}", st.class).ok();
        }
    }
```

- [ ] **Step 4: Failure sections → native headers (`render_enriched` ~1282, ~1289)**

```rust
    if let Some(sf) = surefire {
        if !sf.failures.is_empty() {
            out.push_str("\n\n[ERROR] Failures:\n");
            render_failure_block(&mut out, &sf.failures);
        }
    }
    if let Some(fs) = failsafe {
        if !fs.failures.is_empty() {
            out.push_str("\n\n[ERROR] Integration failures:\n");
            render_failure_block(&mut out, &fs.failures);
        }
    }
```

- [ ] **Step 5: `render_failure_block` — drop numbering, prefix lines (~1312)**

```rust
fn render_failure_block(out: &mut String, failures: &[TestFailure]) {
    let shown = failures.iter().take(MAX_FAILURES_PER_SOURCE);
    for f in shown {
        writeln!(out, "[ERROR]   {}.{} <<< FAILURE!", f.test_class, f.test_method).ok();
        if let Some(kind_label) = failure_kind_label(f) {
            writeln!(out, "[ERROR]     {kind_label}").ok();
        }
        if let Some(trace) = &f.stack_trace {
            for line in trace.lines() {
                writeln!(out, "[ERROR]       {line}").ok();
            }
        }
        if let Some(output) = f.test_output.as_deref().filter(|s| !s.is_empty()) {
            writeln!(out, "[ERROR]     captured output:").ok();
            for line in output.lines() {
                writeln!(out, "[ERROR]       {line}").ok();
            }
        }
        out.push('\n');
    }
    if failures.len() > MAX_FAILURES_PER_SOURCE {
        writeln!(out, "... +{} more failures", failures.len() - MAX_FAILURES_PER_SOURCE).ok();
    }
}
```

- [ ] **Step 6: `render_classes_digest` header (~1177) + `finalize_enriched` reference (~1116)**

Digest header — keep maven-shaped, drop the `# … (from XML reports)` RTK label:
```rust
    let mut out = format!(
        "[INFO] Tests run: {}, Failures: {}, Errors: {}, Skipped: {} -- full per-class report\n",
        summary.run, summary.failures, summary.errors, summary.skipped
    );
```
`finalize_enriched` reference line:
```rust
        Some(path) if enriched.reference => format!("{}\n[full per-class report: {}]", enriched.text, path),
```

- [ ] **Step 7: The two no-report branches (`enrich_with_reports` ~1093)**

```rust
    match (zero_tests, &sf, &fs) {
        (true, None, None) => passthrough(
            "[WARNING] No tests were executed! (0 tests — check the surefire plugin \
             configuration in pom.xml)".to_string(),
        ),
        (false, None, None) => passthrough(format!(
            "{text_summary}\n[WARNING] no XML reports under target/surefire-reports/"
        )),
        _ => Enriched {
            text: render_enriched(text_summary, sf.as_ref(), fs.as_ref()),
            reference: digest.is_some(),
            digest,
        },
    }
```

- [ ] **Step 8: Run the Task-2 tests — PASS**

Run: `cargo test --bin rtk cmds::jvm::mvn_cmd::tests::breakdown_uses_surefire_line_shape cmds::jvm::mvn_cmd::tests::enriched_failures_header_is_native cmds::jvm::mvn_cmd::tests::digest_reference_uses_tee_hint_style`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add src/cmds/jvm/mvn_cmd.rs
git commit -m "refactor(mvn): render XML enrichment in Maven line shape"
```

---

### Task 3: Compile path — drop Total time

**Files:**
- Modify: `src/cmds/jvm/mvn_cmd.rs` — the compile keep-rule `|| stripped.starts_with("Total time:")` (~2410); invert the assertion at ~3405.

- [ ] **Step 1: Flip the existing "keep Total time" test (~3405)**

Replace:
```rust
        assert!(
            output.contains("Total time:"),
            "should keep Total time"
        );
```
with:
```rust
        assert!(
            !output.contains("Total time:"),
            "Total time must be dropped:\n{output}"
        );
```

- [ ] **Step 2: Run — verify FAIL**

Run: `cargo test --bin rtk cmds::jvm 2>&1 | grep -E "Total time|test result"`
Expected: the flipped test FAILs (output still contains Total time).

- [ ] **Step 3: Remove the keep-rule clause (~2410)**

Delete the line:
```rust
                || stripped.starts_with("Total time:")
```
(Adjust the surrounding `||` chain so it still compiles — the preceding clause keeps its `||` only if another clause follows.)

- [ ] **Step 4: Run — PASS**

Run: `cargo test --bin rtk cmds::jvm 2>&1 | grep -E "test result"`
Expected: the compile Total-time test passes (compile snapshots regenerated in Task 7).

- [ ] **Step 5: Commit**

```bash
git add src/cmds/jvm/mvn_cmd.rs
git commit -m "refactor(mvn): drop Total time from compile subset"
```

---

### Task 4: clean / checkstyle → hard subset

**Files:**
- Modify: `src/cmds/jvm/mvn_cmd.rs` — `filter_mvn_clean` summary tail (~2244–2262), `filter_mvn_checkstyle` empty case (~2357).

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn clean_pass_is_native_build_line() {
    let out = filter_mvn_clean("[INFO] Deleting /p/target\n[INFO] BUILD SUCCESS\n[INFO] Total time: 0.4 s\n");
    assert_eq!(out, "[INFO] BUILD SUCCESS");
}

#[test]
fn checkstyle_pass_is_native_build_line() {
    let out = filter_mvn_checkstyle("[INFO] Starting audit...\nAudit done.\n[INFO] BUILD SUCCESS\n");
    assert_eq!(out, "[INFO] BUILD SUCCESS");
}
```

- [ ] **Step 2: Run — verify FAIL**

Run: `cargo test --bin rtk cmds::jvm::mvn_cmd::tests::clean_pass_is_native_build_line cmds::jvm::mvn_cmd::tests::checkstyle_pass_is_native_build_line`
Expected: FAIL (`mvn clean: deleted …`, `mvn checkstyle: ok`).

- [ ] **Step 3: Rewrite `filter_mvn_clean` tail (~2244)**

Replace the `if build_failure { … } match deleted_count { … }` tail with:
```rust
    if build_failure {
        let mut result = String::from("[INFO] BUILD FAILURE");
        for err in &error_lines {
            result.push_str("\n[ERROR]   ");
            result.push_str(&truncate(err, MAX_LINE_LENGTH));
        }
        return result;
    }
    // Hard subset: clean has no native compact line other than the build result.
    let _ = (deleted_count, first_deleted, time_str);
    "[INFO] BUILD SUCCESS".to_string()
```
(Remove now-unused bindings the compiler flags: `deleted_count`, `first_deleted`, `time_str` if truly unused after this.)

- [ ] **Step 4: Rewrite `filter_mvn_checkstyle` empty case (~2357)**

```rust
    if result.is_empty() {
        return "[INFO] BUILD SUCCESS".to_string();
    }
```

- [ ] **Step 5: Run — PASS**

Run: `cargo test --bin rtk cmds::jvm::mvn_cmd::tests::clean_pass_is_native_build_line cmds::jvm::mvn_cmd::tests::checkstyle_pass_is_native_build_line`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/cmds/jvm/mvn_cmd.rs
git commit -m "refactor(mvn): hard subset for clean and checkstyle"
```

---

### Task 5: dependency:tree / dependency:list — drop synthetic lines

**Files:**
- Modify: `src/cmds/jvm/mvn_cmd.rs` — `filter_mvn_dependency_tree` empty case (~2430), `filter_mvn_dependency_list` summary header + empty cases (~2518, ~2557, ~2567).

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn dep_list_has_no_synthetic_headline() {
    let input = include_str!("../../../tests/fixtures/mvn_dependency_list.txt"); // reuse existing dep:list fixture
    let out = filter_mvn_dependency_list(input);
    assert!(!out.contains("mvn dependency:list:"), "synthetic headline leaked:\n{out}");
    assert!(out.contains(":compile") || out.contains(":test"), "native dep lines missing:\n{out}");
}

#[test]
fn dep_tree_empty_is_native_build_line() {
    let out = filter_mvn_dependency_tree("[INFO] BUILD SUCCESS\n");
    assert_eq!(out, "[INFO] BUILD SUCCESS");
}
```
(If no `mvn_dependency_list.txt` fixture exists, use the fixture name the existing dep:list tests already `include_str!`.)

- [ ] **Step 2: Run — verify FAIL**

Run: `cargo test --bin rtk cmds::jvm::mvn_cmd::tests::dep_list_has_no_synthetic_headline cmds::jvm::mvn_cmd::tests::dep_tree_empty_is_native_build_line`
Expected: FAIL.

- [ ] **Step 3: dep:tree empty case (~2430)**

```rust
    if tree_lines.is_empty() {
        return "[INFO] BUILD SUCCESS".to_string();
    }
```

- [ ] **Step 4: dep:list — drop the synthetic headline (~2567) and empty cases (~2518, ~2557)**

Replace the empty/no-deps returns:
```rust
        return "[INFO] BUILD SUCCESS".to_string();   // both "no output" and "no dependencies found"
```
Replace the headline block:
```rust
    let mut out = String::with_capacity(clean.len() / 4);
    if total > 0 {
        // No synthetic "N unique deps" headline — emit the native grouped list.
        for (scope, deps) in &mut groups {
            deps.sort();
            let _ = write!(out, "[INFO] {scope} ({}):", deps.len());
            for dep in deps.iter() {
                let _ = write!(out, "\n[INFO]   {dep}");
            }
            out.push('\n');
        }
    }
```
(Keep the trailing `error_lines`/`build_failure` handling but prefix the appended `BUILD FAILURE` as `[INFO] BUILD FAILURE`.)

- [ ] **Step 5: Run — PASS**

Run: `cargo test --bin rtk cmds::jvm::mvn_cmd::tests::dep_list_has_no_synthetic_headline cmds::jvm::mvn_cmd::tests::dep_tree_empty_is_native_build_line`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/cmds/jvm/mvn_cmd.rs
git commit -m "refactor(mvn): native subset for dependency:tree/list"
```

---

### Task 6: multi-goal — native Reactor Summary, keep prefixes

**Files:**
- Modify: `src/cmds/jvm/mvn_cmd.rs` — `extract_build_block` (~609), `compose_multi` (~684).

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn multi_goal_has_no_rtk_markers() {
    let input = include_str!("../../../tests/fixtures/mvn_multi_verify_fail.txt"); // reuse the multi-goal fixture the existing multi test uses
    let out = filter_mvn_multi(input, "clean verify");
    assert!(!out.contains("(multi-goal)"), "(multi-goal) marker leaked:\n{out}");
    assert!(!out.contains("mvn: ok"), "mvn: ok marker leaked:\n{out}");
    assert!(!out.contains("Total time"), "Total time leaked:\n{out}");
    assert!(out.contains("[INFO] BUILD FAILURE") || out.contains("[INFO] BUILD SUCCESS"),
        "native BUILD line missing:\n{out}");
}
```

- [ ] **Step 2: Run — verify FAIL**

Run: `cargo test --bin rtk cmds::jvm::mvn_cmd::tests::multi_goal_has_no_rtk_markers`
Expected: FAIL.

- [ ] **Step 3: `extract_build_block` — keep prefixes, drop Total time (~609)**

```rust
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
                out.push(st.to_string()); // keep prefix verbatim
            }
        }
        if st.contains("BUILD SUCCESS") || st.contains("BUILD FAILURE") {
            out.push(if failed { "[INFO] BUILD FAILURE".to_string() } else { "[INFO] BUILD SUCCESS".to_string() });
        }
        // Total time intentionally dropped.
    }
    out.join("\n")
}
```

- [ ] **Step 4: `compose_multi` — drop the `(multi-goal)` header (~684)**

Delete the header line:
```rust
    let _ = writeln!(out, "mvn {goals_header} (multi-goal)");
```
(Leave `goals_header` param in place — still used by callers/signature. Prefix it with `let _ = goals_header;` if the compiler flags it unused.) The per-goal `mvn: ok` marker is not emitted by `compose_multi`; confirm via `grep -n '"mvn: ok"' src/cmds/jvm/mvn_cmd.rs` — it comes from a sub-filter's success case (checkstyle/dep) already rewritten in Tasks 4–5, so no further change here.

- [ ] **Step 5: Run — PASS**

Run: `cargo test --bin rtk cmds::jvm::mvn_cmd::tests::multi_goal_has_no_rtk_markers`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/cmds/jvm/mvn_cmd.rs
git commit -m "refactor(mvn): native Reactor Summary for multi-goal"
```

---

### Task 7: Regression fences, snapshot regen, savings re-baseline

**Files:**
- Modify: `src/cmds/jvm/mvn_cmd.rs` — add fence tests; adjust any `*_savings` thresholds the reshape moves.
- Regenerate: `src/cmds/jvm/snapshots/*.snap`.

- [ ] **Step 1: Add principle-fence tests**

```rust
#[test]
fn no_surface_emits_synthetic_or_rtk_markers() {
    let cases: &[&str] = &[
        include_str!("../../../tests/fixtures/mvn_test_pass_mavenmcp.txt"),
        include_str!("../../../tests/fixtures/mvn_test_reactor_fail.txt"),
        include_str!("../../../tests/fixtures/mvn_test_compile_failure.txt"),
    ];
    for raw in cases {
        let out = filter_mvn_test(raw);
        for banned in ["mvn test:", "(from surefire-reports/)", "(from failsafe-reports/)",
                       "(multi-goal)", "mvn: ok", "\nclasses:", "rtk", "Total time"] {
            assert!(!out.contains(banned), "banned marker `{banned}` in:\n{out}");
        }
    }
}

#[test]
fn retained_lines_keep_maven_prefixes() {
    let out = filter_mvn_test(include_str!("../../../tests/fixtures/mvn_test_reactor_fail.txt"));
    assert!(out.lines().any(|l| l.starts_with("[ERROR] Tests run:")), "\n{out}");
    assert!(out.lines().any(|l| l.starts_with("[INFO] BUILD FAILURE")), "\n{out}");
}
```

- [ ] **Step 2: Run — verify FAIL where applicable, then regenerate snapshots**

Run: `cargo test --bin rtk cmds::jvm 2>&1 | grep -E "test result|FAILED"`
Then: `cargo insta test --bin rtk --review` (accept every snapshot that now shows the prefix-preserving shape; reject anything still carrying a synthetic `mvn <goal>:` line — that would signal a missed surface).
Accept with: `cargo insta accept` once reviewed.

- [ ] **Step 3: Re-baseline savings thresholds**

Run: `cargo test --bin rtk cmds::jvm 2>&1 | grep -iE "savings|expected >="`
For each failing `*_savings` assertion, update the threshold to the new measured value and add a one-line comment stating why it moved (prefix bytes / dropped synthetic line). Test paths should stay ≥94%; clean/checkstyle stay ≫60%.

- [ ] **Step 4: Full gate**

Run: `cargo fmt --all && cargo clippy --all-targets && cargo test --bin rtk cmds::jvm`
Expected: clippy clean, all jvm tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/cmds/jvm/
git commit -m "test(mvn): fence native-format fidelity + regenerate snapshots"
```

---

## Self-Review

**Spec coverage:** test pass/fail frame → Task 1; no-tests native warning → Task 1; breakdown option-c → Task 2; Failures/Integration headers → Task 2; digest reference → Task 2; compile Total-time drop → Task 3; clean/checkstyle hard subset → Task 4; dep:tree/list → Task 5; multi-goal Reactor Summary → Task 6; guard unchanged → respected (no runner/guard edits); fences + snapshots + savings → Task 7. All spec sections covered.

**Placeholder scan:** every code step shows real before/after against lines read from the current module. Two spots depend on existing test helpers/fixtures (`sf_result_with_two_failures`, dep:list/multi fixtures) — the step names the fallback (reuse the fixture the neighbouring test already `include_str!`s). Step 1 of Task 1 pins the exact BUILD/aggregate prefixes from a real fixture rather than guessing.

**Type consistency:** the `text_summary` contract (Task 1 produces / Task 2 consumes) is stated in the File Structure block and matched in both tasks (`\n[INFO] BUILD SUCCESS`, `\n[ERROR] Failures:\n`, `[WARNING] No tests were executed!`). `filter_mvn_test`/`filter_mvn_tests_with_goal`/`filter_mvn_clean`/`filter_mvn_checkstyle`/`filter_mvn_dependency_list`/`filter_mvn_multi`/`render_enriched`/`finalize_enriched` names match their definitions in the module.
