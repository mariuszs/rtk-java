# mvn Pass-Run Enrichment Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enrich passing `mvn test`-like runs with a per-class breakdown: inline when small (≤5 classes), otherwise via a digest file next to the tee log that Claude can read on demand.

**Architecture:** Extend the existing surefire XML parser (`surefire_reports.rs`) to collect per-suite stats and skipped-test names. `enrich_with_reports` (in `mvn_cmd.rs`) loses its clean-run early return and now returns a struct `{text, digest, reference}`; a small `finalize_enriched` helper at the two call sites writes the digest through the tee infrastructure and appends a `classes: <path>` line when needed. All rendering is pure and snapshot-tested; filesystem writes stay at the run layer.

**Tech Stack:** Rust, quick-xml (already used), insta snapshots, tempfile (dev-dep, already used).

Spec: `docs/superpowers/specs/2026-07-08-mvn-pass-enrichment-design.md`

## Global Constraints

- No async; no `unwrap()` in production code (`.context()?` or graceful fallback); tests may use `expect()`.
- Fallback pattern: any enrichment problem → return the text summary unchanged. The happy path must never get worse.
- Fixtures are REAL command output, sanitized: `com.devskiller` → `com.example`, `/home/mariusz` → `/home/user`, hostnames/env stripped.
- Quality gate before EVERY commit: `cargo fmt --all && cargo clippy --all-targets && cargo test --all`.
- Local baseline: ~16 pre-existing failures in `hooks::rewrite_cmd` permission tests (env-dependent, green on CI). Before starting, record the baseline (`cargo test --all 2>&1 | grep -c FAILED` or the failing test list); after each task the failure set must be IDENTICAL to baseline.
- Conventional Commits; no AI attribution, no Co-Authored-By; use `--no-verify` if a hook demands a JIRA number.
- Constants: `MAX_INLINE_CLASSES = 5`, `MAX_INLINE_SKIPPED = 3`.
- Working directory: `/home/mariusz/projects/rtk-java/.claude/worktrees/merry-soaring-quokka` (git worktree, branch `feat/mvn-usage-driven-improvements`). Verify with `pwd` before starting.

---

### Task 1: Real XML fixtures

**Files:**
- Create: `tests/fixtures/surefire_xml/TEST-com.example.auth.user.UsersTest.xml`
- Create: `tests/fixtures/surefire_xml/TEST-com.example.auth.user.password.PasswordPolicyServiceTest.xml`
- Create: `tests/fixtures/surefire_xml/TEST-com.example.auth.partners.entraid.MicrosoftEntraIdClient2Test.xml`

**Interfaces:**
- Consumes: real surefire reports at `/home/mariusz/git/auth/target/surefire-reports/` (197 files exist from real runs; `MicrosoftEntraIdClient2Test` has 8 `<skipped>` entries).
- Produces: three sanitized XML fixture files that Tasks 2, 4, 5 load via `include_str!("../../../tests/fixtures/surefire_xml/<name>.xml")` (same relative-path convention as existing fixtures in `src/cmds/jvm/mvn_cmd.rs`).

- [ ] **Step 1: Copy the three real reports**

```bash
mkdir -p tests/fixtures/surefire_xml
SRC=/home/mariusz/git/auth/target/surefire-reports
cp "$SRC/TEST-com.devskiller.auth.user.UsersTest.xml" \
   tests/fixtures/surefire_xml/TEST-com.example.auth.user.UsersTest.xml
cp "$SRC/TEST-com.devskiller.auth.user.password.PasswordPolicyServiceTest.xml" \
   tests/fixtures/surefire_xml/TEST-com.example.auth.user.password.PasswordPolicyServiceTest.xml
cp "$SRC/TEST-com.devskiller.auth.partners.entraid.MicrosoftEntraIdClient2Test.xml" \
   tests/fixtures/surefire_xml/TEST-com.example.auth.partners.entraid.MicrosoftEntraIdClient2Test.xml
```

- [ ] **Step 2: Sanitize**

The `<properties>` block contains the full environment (paths, hostname, user) and is irrelevant to parsing — delete it entirely. Then rename packages and paths:

```bash
cd tests/fixtures/surefire_xml
sed -i '/<properties>/,/<\/properties>/d' TEST-*.xml
sed -i 's/com\.devskiller/com.example/g; s|/home/mariusz|/home/user|g' TEST-*.xml
```

- [ ] **Step 3: Manually review each fixture**

Read all three files end-to-end. Verify: no `devskiller`, no real hostname/username, no internal URLs or secrets in `<system-out>`/`<system-err>` (if any leak, delete those elements' content). Verify the Entra fixture still contains `<skipped` elements with `message` attributes, and each file still has a `<testsuite name="com.example..." tests="N" skipped="K" time="T">` root.

```bash
grep -c '<skipped' TEST-com.example.auth.partners.entraid.MicrosoftEntraIdClient2Test.xml   # expect 8
grep -o 'devskiller\|mariusz' TEST-*.xml | head   # expect empty
```

- [ ] **Step 4: Commit**

```bash
cd /home/mariusz/projects/rtk-java/.claude/worktrees/merry-soaring-quokka
git add tests/fixtures/surefire_xml/
git commit --no-verify -m "test(mvn): add real sanitized surefire XML fixtures for pass-run enrichment"
```

---

### Task 2: Parser extension — SuiteStat and SkippedTest

**Files:**
- Modify: `src/cmds/jvm/surefire_reports.rs` (structs at lines ~16-57, `parse_content` at ~90-232, `parse_dir` aggregate at ~314-325)

**Interfaces:**
- Consumes: fixture files from Task 1.
- Produces (used by Tasks 3-5):

```rust
#[derive(Debug, PartialEq, Clone)]
pub struct SuiteStat {
    pub class_name: String,       // FQCN from <testsuite name="...">
    pub tests: u32,
    pub skipped: u32,
    pub time_secs: f64,
    pub module: Option<String>,   // set later by collect_reports (Task 3)
}

#[derive(Debug, PartialEq, Clone)]
pub struct SkippedTest {
    pub class: String,            // FQCN
    pub method: String,
    pub reason: Option<String>,   // <skipped message="...">
}

// SurefireResult gains:
pub struct SurefireResult {
    // ... existing fields ...
    pub suites: Vec<SuiteStat>,
    pub skipped_tests: Vec<SkippedTest>,
}
```

- [ ] **Step 1: Write failing tests** (append to the existing `#[cfg(test)] mod tests` in `surefire_reports.rs`)

```rust
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
```

If the assertions on counts don't match the fixture (check the file), adjust the EXPECTED VALUES to the fixture's real content — never the other way around.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p rtk parse_content_collects 2>&1 | tail -5`
Expected: compile error — `no field 'suites' on SurefireResult`.

- [ ] **Step 3: Implement**

(a) Add the two structs (shown in Interfaces above) after `TestFailure`, and the two `Vec` fields to `SurefireResult` (it derives `Default` — Vecs are fine).

(b) In `parse_content`, extend the `b"testsuite"` branch (currently lines ~120-129) — build the `SuiteStat` from the same attributes before `summary.add`:

```rust
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
```

(c) Add a `b"skipped"` arm in the same `Start | Empty` match (surefire emits `<skipped message="..."/>` as an Empty event inside `<testcase>`):

```rust
b"skipped" => {
    result.skipped_tests.push(SkippedTest {
        class: current_class.clone().unwrap_or_default(),
        method: current_method.clone().unwrap_or_default(),
        reason: extract_attr(&reader, &e, b"message").filter(|s| !s.is_empty()),
    });
}
```

(d) In `parse_dir`'s merge (currently `aggregate.summary.add(...); aggregate.failures.extend(...)` at ~316-318), also merge the new fields:

```rust
aggregate.suites.extend(file_result.suites);
aggregate.skipped_tests.extend(file_result.skipped_tests);
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p rtk parse_content_collects`
Expected: 2 passed. Then `cargo test --all 2>&1 | tail -3` — failure set identical to baseline (struct literal updates may be needed in existing tests that construct `SurefireResult` by hand; fix with `..Default::default()`).

- [ ] **Step 5: Gate + commit**

```bash
cargo fmt --all && cargo clippy --all-targets && cargo test --all
git add src/cmds/jvm/surefire_reports.rs
git commit --no-verify -m "feat(mvn): collect per-suite stats and skipped test names from surefire XML"
```

---

### Task 3: Module attachment in collect_reports

**Files:**
- Modify: `src/cmds/jvm/mvn_cmd.rs` — `collect_reports` (~line 979) and its two call sites in `enrich_with_reports` (~1029-1031)

**Interfaces:**
- Consumes: `SuiteStat.module` field from Task 2.
- Produces: `fn collect_reports(dirs: &[PathBuf], since: SystemTime, app_packages: &[String], cwd: &Path) -> Option<SurefireResult>` — suites carry `module: Some("<first path component>")` for reactor modules, `None` for the root `target/`.

- [ ] **Step 1: Write failing test** (in `mvn_cmd.rs` tests module; follow the existing tempdir enrichment tests around line ~3950 for the tempdir + XML pattern)

```rust
#[test]
fn collect_reports_attaches_module_from_dir_layout() {
    let tmp = tempfile::tempdir().unwrap();
    let xml = include_str!(
        "../../../tests/fixtures/surefire_xml/TEST-com.example.auth.user.UsersTest.xml"
    );
    let root_dir = tmp.path().join("target/surefire-reports");
    let mod_dir = tmp.path().join("services/target/surefire-reports");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&mod_dir).unwrap();
    std::fs::write(root_dir.join("TEST-com.example.A.xml"), xml).unwrap();
    std::fs::write(mod_dir.join("TEST-com.example.B.xml"), xml).unwrap();

    let since = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    let r = super::collect_reports(
        &[root_dir, mod_dir],
        since,
        &[],
        tmp.path(),
    )
    .expect("reports must parse");
    let modules: Vec<Option<String>> =
        r.suites.iter().map(|s| s.module.clone()).collect();
    assert_eq!(modules, vec![None, Some("services".to_string())]);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p rtk collect_reports_attaches_module 2>&1 | tail -5`
Expected: compile error — `collect_reports` takes 3 arguments.

- [ ] **Step 3: Implement**

Add helper + extend `collect_reports`:

```rust
/// Module name for a report dir: first path component of `dir` relative to
/// `cwd` ("services/target/surefire-reports" -> "services"); `None` for the
/// root-level `target/` or when the dir is outside `cwd`.
fn module_for_dir(dir: &Path, cwd: &Path) -> Option<String> {
    let rel = dir.strip_prefix(cwd).ok()?;
    let first = rel.components().next()?;
    let name = first.as_os_str().to_str()?;
    if name == "target" {
        None
    } else {
        Some(name.to_string())
    }
}
```

In `collect_reports`, add the `cwd: &Path` parameter and, inside the loop right after `parse_dir` succeeds, before merging:

```rust
let module = module_for_dir(dir, cwd);
let mut r = r;
for s in r.suites.iter_mut() {
    s.module = module.clone();
}
```

Also extend the existing `merged` arm with the new fields:

```rust
acc.suites.extend(r.suites);
acc.skipped_tests.extend(r.skipped_tests);
```

Update the two call sites in `enrich_with_reports`:

```rust
let sf = collect_reports(&sf_dirs, since, app_packages, cwd);
let fs = collect_reports(&fs_dirs, since, app_packages, cwd);
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p rtk collect_reports_attaches_module` → PASS; `cargo test --all 2>&1 | tail -3` → baseline delta zero.

- [ ] **Step 5: Gate + commit**

```bash
cargo fmt --all && cargo clippy --all-targets && cargo test --all
git add src/cmds/jvm/mvn_cmd.rs
git commit --no-verify -m "feat(mvn): attach reactor module names to parsed suite stats"
```

---

### Task 4: Pure renderers — digest and hybrid inline

**Files:**
- Modify: `src/cmds/jvm/mvn_cmd.rs` — new constants + three pure functions near `render_enriched` (~line 1047); tests + insta snapshots in the same file's tests module

**Interfaces:**
- Consumes: `SurefireResult { suites, skipped_tests, summary }` from Tasks 2-3.
- Produces (used by Task 5):
  - `fn render_classes_digest(goal: &str, surefire: Option<&SurefireResult>, failsafe: Option<&SurefireResult>) -> Option<String>` — `None` when no suites parsed.
  - `fn render_pass_inline(text_summary: &str, surefire: Option<&SurefireResult>, failsafe: Option<&SurefireResult>) -> (String, bool)` — enriched text + `needs_reference` flag.
  - `const MAX_INLINE_CLASSES: usize = 5;`, `const MAX_INLINE_SKIPPED: usize = 3;`
  - `fn short_class(fqcn: &str) -> &str`

- [ ] **Step 1: Write failing tests** (snapshots + behavior; in `mvn_cmd.rs` tests module)

```rust
fn parsed_fixture(xml: &str) -> super::surefire_reports::SurefireResult {
    super::surefire_reports::parse_content(xml, &[]).expect("fixture must parse")
}

#[test]
fn digest_snapshot_from_real_fixtures() {
    let mut sf = parsed_fixture(include_str!(
        "../../../tests/fixtures/surefire_xml/TEST-com.example.auth.user.UsersTest.xml"
    ));
    let entra = parsed_fixture(include_str!(
        "../../../tests/fixtures/surefire_xml/TEST-com.example.auth.partners.entraid.MicrosoftEntraIdClient2Test.xml"
    ));
    sf.suites.extend(entra.suites.clone());
    sf.skipped_tests.extend(entra.skipped_tests.clone());
    sf.summary.add(&entra.summary);
    sf.suites[0].module = Some("services".to_string());

    let digest = super::render_classes_digest("test", Some(&sf), None)
        .expect("suites present -> digest");
    insta::assert_snapshot!("pass_digest_snapshot", digest);
}

#[test]
fn digest_none_without_suites() {
    assert_eq!(super::render_classes_digest("test", None, None), None);
    let empty = super::surefire_reports::SurefireResult::default();
    assert_eq!(super::render_classes_digest("test", Some(&empty), None), None);
}

#[test]
fn pass_inline_small_run_lists_classes() {
    let sf = parsed_fixture(include_str!(
        "../../../tests/fixtures/surefire_xml/TEST-com.example.auth.user.UsersTest.xml"
    ));
    let (out, needs_ref) =
        super::render_pass_inline("mvn test: 12 passed (3.1 s)", Some(&sf), None);
    assert!(out.starts_with("mvn test: 12 passed (3.1 s)\n"));
    assert!(out.contains("UsersTest:"), "short class name inline, got: {out}");
    assert!(!needs_ref, "1 class <= MAX_INLINE_CLASSES");
    insta::assert_snapshot!("pass_inline_small_snapshot", out);
}

#[test]
fn pass_inline_large_run_defers_to_reference() {
    // Build >MAX_INLINE_CLASSES suites from the real fixture by cloning
    // its (real) suite stat under distinct class names.
    let base = parsed_fixture(include_str!(
        "../../../tests/fixtures/surefire_xml/TEST-com.example.auth.user.UsersTest.xml"
    ));
    let mut sf = super::surefire_reports::SurefireResult::default();
    for i in 0..6 {
        let mut s = base.suites[0].clone();
        s.class_name = format!("com.example.auth.Suite{i}Test");
        sf.suites.push(s);
        sf.summary.add(&base.summary);
    }
    let (out, needs_ref) =
        super::render_pass_inline("mvn test: 72 passed (9.0 s)", Some(&sf), None);
    assert_eq!(out, "mvn test: 72 passed (9.0 s)", "no inline list for >5 classes");
    assert!(needs_ref);
}

#[test]
fn pass_inline_many_skipped_forces_reference() {
    let sf = parsed_fixture(include_str!(
        "../../../tests/fixtures/surefire_xml/TEST-com.example.auth.partners.entraid.MicrosoftEntraIdClient2Test.xml"
    ));
    // 8 skipped > MAX_INLINE_SKIPPED: names go to the digest only.
    let (out, needs_ref) =
        super::render_pass_inline("mvn test: 5 passed, 8 skipped (2.0 s)", Some(&sf), None);
    assert!(needs_ref, "skipped names beyond inline cap require the digest reference");
    assert!(
        !out.contains("skipped: "),
        "no skipped-name lines inline when count > cap, got: {out}"
    );
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p rtk pass_inline 2>&1 | tail -5`
Expected: compile error — `render_pass_inline` not found.

- [ ] **Step 3: Implement** (place near `render_enriched`)

```rust
const MAX_INLINE_CLASSES: usize = 5;
const MAX_INLINE_SKIPPED: usize = 3;

fn short_class(fqcn: &str) -> &str {
    fqcn.rsplit('.').next().unwrap_or(fqcn)
}

fn all_suites<'a>(
    surefire: Option<&'a SurefireResult>,
    failsafe: Option<&'a SurefireResult>,
) -> Vec<&'a surefire_reports::SuiteStat> {
    surefire
        .into_iter()
        .chain(failsafe)
        .flat_map(|r| r.suites.iter())
        .collect()
}

fn all_skipped<'a>(
    surefire: Option<&'a SurefireResult>,
    failsafe: Option<&'a SurefireResult>,
) -> Vec<&'a surefire_reports::SkippedTest> {
    surefire
        .into_iter()
        .chain(failsafe)
        .flat_map(|r| r.skipped_tests.iter())
        .collect()
}

/// Condensed per-class report written next to the tee log. `None` when no
/// suites were parsed (nothing worth writing).
fn render_classes_digest(
    goal: &str,
    surefire: Option<&SurefireResult>,
    failsafe: Option<&SurefireResult>,
) -> Option<String> {
    let suites = all_suites(surefire, failsafe);
    if suites.is_empty() {
        return None;
    }
    let skipped = all_skipped(surefire, failsafe);
    let total: u32 = suites.iter().map(|s| s.tests).sum();
    let total_skipped: u32 = suites.iter().map(|s| s.skipped).sum();
    let passed = total.saturating_sub(total_skipped);

    let mut out = format!("# mvn {goal} — {passed} passed");
    if total_skipped > 0 {
        write!(out, ", {total_skipped} skipped").ok();
    }
    out.push('\n');

    // Group by module; BTreeMap for deterministic order, root module last-free
    // "." key sorts first which is fine.
    let mut by_module: std::collections::BTreeMap<&str, Vec<String>> =
        std::collections::BTreeMap::new();
    for s in &suites {
        by_module
            .entry(s.module.as_deref().unwrap_or("."))
            .or_default()
            .push(format!(
                "{} {} ({:.1}s)",
                short_class(&s.class_name),
                s.tests,
                s.time_secs
            ));
    }
    for (module, classes) in &by_module {
        writeln!(out, "{module}: {}", classes.join(", ")).ok();
    }

    if !skipped.is_empty() {
        out.push_str("skipped:\n");
        for st in &skipped {
            match &st.reason {
                Some(reason) => {
                    writeln!(out, "  {}.{} — {}", short_class(&st.class), st.method, reason)
                        .ok();
                }
                None => {
                    writeln!(out, "  {}.{}", short_class(&st.class), st.method).ok();
                }
            }
        }
    }
    Some(out)
}

/// Hybrid inline rendering for passing runs. Returns the (possibly extended)
/// summary and whether the digest reference line is required — true when the
/// class list or skipped names exceed the inline caps.
fn render_pass_inline(
    text_summary: &str,
    surefire: Option<&SurefireResult>,
    failsafe: Option<&SurefireResult>,
) -> (String, bool) {
    let suites = all_suites(surefire, failsafe);
    let skipped = all_skipped(surefire, failsafe);
    let needs_reference =
        suites.len() > MAX_INLINE_CLASSES || skipped.len() > MAX_INLINE_SKIPPED;

    let mut out = text_summary.to_string();
    if !suites.is_empty() && suites.len() <= MAX_INLINE_CLASSES {
        for s in &suites {
            write!(
                out,
                "\n{}: {} ({:.1}s)",
                short_class(&s.class_name),
                s.tests,
                s.time_secs
            )
            .ok();
        }
    }
    if !skipped.is_empty() && skipped.len() <= MAX_INLINE_SKIPPED {
        for st in &skipped {
            write!(out, "\nskipped: {}.{}", short_class(&st.class), st.method).ok();
            if let Some(reason) = &st.reason {
                write!(out, " — {reason}").ok();
            }
        }
    }
    (out, needs_reference)
}
```

Note: `use std::fmt::Write;` is already imported in this file (used by `render_failure_block`); verify, add if missing.

- [ ] **Step 4: Run tests, review snapshots**

Run: `cargo test -p rtk digest_ ; cargo test -p rtk pass_inline`
Expected: snapshot tests create `.snap.new` files. Review with `cargo insta review` (or inspect + `cargo insta accept`) — check: short class names, module grouping, skip reasons present, no sanitization leaks.

- [ ] **Step 5: Gate + commit**

```bash
cargo fmt --all && cargo clippy --all-targets && cargo test --all
git add src/cmds/jvm/mvn_cmd.rs src/cmds/jvm/snapshots/
git commit --no-verify -m "feat(mvn): pure renderers for pass-run class digest and hybrid inline breakdown"
```

---

### Task 5: Rewire enrich_with_reports

**Files:**
- Modify: `src/cmds/jvm/mvn_cmd.rs` — `enrich_with_reports` (~line 1006-1045) and every test call site (`grep -n "enrich_with_reports(" src/cmds/jvm/mvn_cmd.rs` — 2 production + ~10 test sites; production sites are updated in Task 6)

**Interfaces:**
- Consumes: renderers from Task 4, `collect_reports` from Task 3.
- Produces (used by Task 6):

```rust
pub(crate) struct Enriched {
    pub(crate) text: String,
    pub(crate) digest: Option<String>,  // digest file content; None -> nothing to write
    pub(crate) reference: bool,         // append "classes: <path>" line after writing
}

pub(crate) fn enrich_with_reports(
    text_summary: &str,
    cwd: &std::path::Path,
    since: std::time::SystemTime,
    app_packages: &[String],
    goal: &str,
) -> Enriched
```

- [ ] **Step 1: Write failing tests** (tempdir pattern as in existing enrichment tests ~line 3950)

```rust
/// Helper: tempdir with N copies of the real UsersTest fixture under
/// target/surefire-reports, class names made distinct.
fn tmp_with_reports(n: usize) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("target/surefire-reports");
    std::fs::create_dir_all(&dir).unwrap();
    let xml = include_str!(
        "../../../tests/fixtures/surefire_xml/TEST-com.example.auth.user.UsersTest.xml"
    );
    for i in 0..n {
        let renamed = xml.replace("UsersTest", &format!("Suite{i}Test"));
        std::fs::write(dir.join(format!("TEST-com.example.Suite{i}Test.xml")), renamed)
            .unwrap();
    }
    tmp
}

#[test]
fn enrich_pass_small_run_inlines_classes_and_carries_digest() {
    let tmp = tmp_with_reports(2);
    let since = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    let out = super::enrich_with_reports(
        "mvn test: 24 passed (3.0 s)",
        tmp.path(),
        since,
        &pkgs("com.example"),
        "test",
    );
    assert!(out.text.contains("Suite0Test:"), "inline class list, got: {}", out.text);
    assert!(out.digest.is_some(), "digest written even for inline runs");
    assert!(!out.reference, "2 classes fit inline — no reference line needed");
}

#[test]
fn enrich_pass_large_run_defers_to_digest() {
    let tmp = tmp_with_reports(6);
    let since = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    let out = super::enrich_with_reports(
        "mvn test: 72 passed (9.0 s)",
        tmp.path(),
        since,
        &pkgs("com.example"),
        "test",
    );
    assert_eq!(out.text, "mvn test: 72 passed (9.0 s)");
    assert!(out.reference);
    let digest = out.digest.expect("digest for large run");
    assert!(digest.contains("Suite5Test"), "all classes in digest, got: {digest}");
}

#[test]
fn enrich_clean_run_without_reports_passes_through() {
    // Replaces enrich_happy_path_passes_through_without_io: the pass path now
    // performs discovery I/O, but with no reports found the summary must come
    // back byte-identical and nothing is written.
    let tmp = tempfile::tempdir().unwrap();
    let text = "mvn test: 42 passed (1.234 s)";
    let out = super::enrich_with_reports(
        text,
        tmp.path(),
        std::time::SystemTime::now(),
        &pkgs("com.example"),
        "test",
    );
    assert_eq!(out.text, text);
    assert_eq!(out.digest, None);
}

#[test]
fn enrich_pass_with_skipped_count_in_summary_still_enriches() {
    // "N passed, K skipped (t)" summaries used to bypass the old
    // `looks_clean` substring check; the new pass gate must catch them.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("target/surefire-reports");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("TEST-com.example.MicrosoftEntraIdClient2Test.xml"),
        include_str!(
            "../../../tests/fixtures/surefire_xml/TEST-com.example.auth.partners.entraid.MicrosoftEntraIdClient2Test.xml"
        ),
    )
    .unwrap();
    let since = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    let out = super::enrich_with_reports(
        "mvn test: 5 passed, 8 skipped (2.0 s)",
        tmp.path(),
        since,
        &pkgs("com.example"),
        "test",
    );
    assert!(out.digest.is_some());
    let digest = out.digest.unwrap();
    assert!(digest.contains("skipped:"), "skipped names in digest, got: {digest}");
    assert!(out.reference, "8 skipped > inline cap");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p rtk enrich_pass 2>&1 | tail -5`
Expected: compile error — no field `text` on `String` (return type not changed yet).

- [ ] **Step 3: Implement**

Replace the body of `enrich_with_reports` (keep the doc comment, extend it with the pass-run behavior):

```rust
pub(crate) fn enrich_with_reports(
    text_summary: &str,
    cwd: &std::path::Path,
    since: std::time::SystemTime,
    app_packages: &[String],
    goal: &str,
) -> Enriched {
    let passthrough = |text: String| Enriched {
        text,
        digest: None,
        reference: false,
    };
    if !text_summary.starts_with("mvn ") {
        return passthrough(text_summary.to_string());
    }

    let zero_tests = text_summary.ends_with(": no tests run")
        || text_summary.contains(": 0 passed");
    let has_failures =
        text_summary.contains("failed") || text_summary.contains("BUILD FAILURE");
    // NOTE: replaces the old `looks_clean` substring check ("passed (" missed
    // "N passed, K skipped (t)" summaries) — a run is passing iff it neither
    // failed nor ran zero tests.
    let passing = !zero_tests && !has_failures;

    let (sf_dirs, fs_dirs) = discover_report_dirs(cwd);
    let sf = collect_reports(&sf_dirs, since, app_packages, cwd);
    let fs = collect_reports(&fs_dirs, since, app_packages, cwd);
    let digest = render_classes_digest(goal, sf.as_ref(), fs.as_ref());

    if passing {
        // Fallback invariant: no parsed reports -> summary unchanged, silently.
        if digest.is_none() {
            return passthrough(text_summary.to_string());
        }
        let (text, reference) = render_pass_inline(text_summary, sf.as_ref(), fs.as_ref());
        return Enriched {
            text,
            digest,
            reference,
        };
    }

    match (zero_tests, &sf, &fs) {
        (true, None, None) => passthrough(format!(
            "mvn {goal}: 0 tests executed — surefire detected no tests. \
             Check pom.xml (surefire plugin configuration) or run: \
             rtk proxy mvn {goal}"
        )),
        (false, None, None) => passthrough(format!(
            "{text_summary}\n(no XML reports found — check target/surefire-reports/ \
             or run: rtk proxy mvn {goal})"
        )),
        _ => Enriched {
            text: render_enriched(text_summary, sf.as_ref(), fs.as_ref()),
            reference: digest.is_some(),
            digest,
        },
    }
}
```

(The old match arms `(true, _, None, None)` / `(_, true, None, None)` collapse to the two above because the pass path already returned; behavior for failing/zero-test runs is unchanged except failing runs with parsed reports now also carry the digest.)

Update ALL test call sites: append `.text` where a `String` was asserted (e.g. `assert_eq!(out, text)` → `assert_eq!(out.text, text)`; `out.contains(...)` → `out.text.contains(...)`). Delete `enrich_happy_path_passes_through_without_io` (replaced by `enrich_clean_run_without_reports_passes_through` in Step 1) and update its sibling zero-test tests to use `.text`.

For the two PRODUCTION call sites (lines ~327 and ~776): to keep this task compiling before Task 6 wires the digest, temporarily use `.text`:

```rust
enrich_with_reports(&filtered, &cwd_for_filter, started_at, &app_pkgs, goal_str).text
```

```rust
parts.tests =
    enrich_with_reports(&parts.tests, &cwd, started_at, &app_pkgs, test_goal).text;
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p rtk enrich` → all pass; `cargo test --all 2>&1 | tail -3` → baseline delta zero.

- [ ] **Step 5: Gate + commit**

```bash
cargo fmt --all && cargo clippy --all-targets && cargo test --all
git add src/cmds/jvm/mvn_cmd.rs
git commit --no-verify -m "feat(mvn): enrich passing test runs with class breakdown and digest content"
```

---

### Task 6: Digest write via tee + run-layer wiring

**Files:**
- Modify: `src/core/tee.rs` — add `force_tee_display` next to `force_tee_hint` (~line 219)
- Modify: `src/cmds/jvm/mvn_cmd.rs` — new `finalize_enriched`, rewire the two production call sites (~327, ~776)

**Interfaces:**
- Consumes: `Enriched` from Task 5; `force_tee_path`, `display_path` (both private in `tee.rs`).
- Produces:
  - `pub fn force_tee_display(content: &str, command_slug: &str) -> Option<String>` in `tee.rs` — writes `<epoch>_<slug>.log` in the tee dir, returns the `~`-relative display path. Respects `RTK_TEE=0` and config like every other tee write.
  - `fn finalize_enriched(enriched: Enriched, tee_label: &str) -> String` in `mvn_cmd.rs`.

Note: the tee writer always uses the `.log` extension (`write_tee_file`), so the digest lands as `<epoch>_<label>_classes.log` — deliberate deviation from the spec's `.classes.txt` name; the spec's intent (separate on-demand file next to the tee log) is preserved and file rotation keeps working unmodified.

- [ ] **Step 1: Add `force_tee_display` to `tee.rs`**

```rust
/// Force-write `content` as a tee file and return its display path
/// (`~/...`), or None if tee is disabled/skipped. Used for auxiliary
/// artifacts like the mvn class digest.
pub fn force_tee_display(content: &str, command_slug: &str) -> Option<String> {
    let path = force_tee_path(content, command_slug)?;
    Some(display_path(&path))
}
```

(No unit test: like the other `force_tee_*` helpers it is config- and $HOME-dependent I/O; the pure logic lives in Task 5's tested code.)

- [ ] **Step 2: Add `finalize_enriched` to `mvn_cmd.rs`** (near `enrich_with_reports`)

```rust
/// Write the class digest (if any) through the tee infrastructure and append
/// the `classes: <path>` reference line when the inline output doesn't carry
/// the full breakdown. Falls back to the enriched text unchanged when tee is
/// disabled or the write fails — the summary must never degrade.
fn finalize_enriched(enriched: Enriched, tee_label: &str) -> String {
    let Some(digest) = enriched.digest else {
        return enriched.text;
    };
    let slug = format!("{tee_label}_classes");
    match crate::core::tee::force_tee_display(&digest, &slug) {
        Some(path) if enriched.reference => format!("{}\nclasses: {}", enriched.text, path),
        _ => enriched.text,
    }
}
```

- [ ] **Step 3: Rewire the two call sites**

`run_tests_like` (~line 317): the closure is `move`, and `tee_label` is used after it for `RunOptions::with_tee` — clone it before the closure:

```rust
let (tool_name, tee_label) = mvn_labels(binary, goal_str, goal.tee_slug());
let tee_label_for_filter = tee_label.clone();
runner::run_filtered(
    cmd,
    &tool_name,
    &args.join(" "),
    move |raw: &str| {
        let filtered = filter_mvn_tests_with_goal(raw, goal_str, &app_pkgs);
        let enriched =
            enrich_with_reports(&filtered, &cwd_for_filter, started_at, &app_pkgs, goal_str);
        finalize_enriched(enriched, &tee_label_for_filter)
    },
    runner::RunOptions::with_tee(&tee_label),
)
```

`run_multi_goal` (~line 763), same pattern:

```rust
let (tool_name, tee_label) = mvn_labels(binary, "multi", "multi");
let tee_label_for_filter = tee_label.clone();
runner::run_filtered(
    cmd,
    &tool_name,
    &run_args.join(" "),
    move |raw: &str| {
        if !PLUGIN_MARKER_RE.is_match(raw) && !BUILD_FOOTER_RE.is_match(raw) {
            return raw.to_string();
        }
        let mut parts = filter_segments(raw);
        if enrich && !parts.tests.trim().is_empty() {
            let enriched =
                enrich_with_reports(&parts.tests, &cwd, started_at, &app_pkgs, test_goal);
            parts.tests = finalize_enriched(enriched, &tee_label_for_filter);
        }
        compose_multi(&parts, &header)
    },
    runner::RunOptions::with_tee(&tee_label),
)
```

- [ ] **Step 4: Run full gate**

```bash
cargo fmt --all && cargo clippy --all-targets && cargo test --all
```
Expected: zero clippy warnings; failure set identical to baseline.

- [ ] **Step 5: End-to-end smoke against a real project**

```bash
cargo build --release
cd /home/mariusz/git/auth && RTK_DB_PATH=/tmp/claude-1000/-home-mariusz-projects-rtk-java/e95eca18-5b2e-43df-8555-2e40dc9db510/scratchpad/smoke.db \
  /home/mariusz/projects/rtk-java/.claude/worktrees/merry-soaring-quokka/target/release/rtk mvn test -Dskip.npm -Dtest=UsersTest
```
Expected: summary line, inline `UsersTest: N (t s)` (1 class → inline, no reference). Then a broad run (`rtk mvn test -Dskip.npm`, ~197 classes): one summary line + `classes: ~/.local/share/rtk/tee/<ts>_mvn_test_classes.log`; `cat` that file and verify module grouping and skipped section. If auth builds are too slow for the session, verify instead with a synthetic reactor dir + the release binary in a scratch project and note it in the report.

- [ ] **Step 6: Commit**

```bash
cd /home/mariusz/projects/rtk-java/.claude/worktrees/merry-soaring-quokka
git add src/core/tee.rs src/cmds/jvm/mvn_cmd.rs
git commit --no-verify -m "feat(mvn): write class digest via tee and reference it from run summaries"
```

---

### Task 7: Merge to master + install

**Files:** none (git + cargo only)

- [ ] **Step 1: Final gate on the branch**

```bash
cargo fmt --all && cargo clippy --all-targets && cargo test --all
```

- [ ] **Step 2: Merge to master (no PR — user's standing instruction) and push**

The main checkout at `/home/mariusz/projects/rtk-java` has `master` checked out; the feature branch lives in this worktree, so the merge runs from the main checkout:

```bash
git log --oneline master..feat/mvn-usage-driven-improvements   # review what merges
git -C /home/mariusz/projects/rtk-java merge --ff-only feat/mvn-usage-driven-improvements
git -C /home/mariusz/projects/rtk-java push origin master
```

If `--ff-only` fails (master moved since), rebase the branch onto master in this worktree first, re-run the gate, then retry.

- [ ] **Step 3: Install with native CPU (user's standing preference)**

```bash
cd /home/mariusz/projects/rtk-java
RUSTFLAGS="-C target-cpu=native" cargo install --path .
rtk --version
```

- [ ] **Step 4: Update memory**

Update `~/.claude/projects/-home-mariusz-projects-rtk-java/memory/project_mvn_usage_improvements.md`: add the pass-run enrichment as shipped (commits, digest file convention `<ts>_<label>_classes.log`, hybrid caps 5/3), remove it from the "remaining follow-ups" line.
