# mvn Surefire/Failsafe XML Enrichment — Design

**Status:** Draft → ready for implementation planning
**Branch:** `feat/mvn-surefire-xml` (stacked on `feat/mvn-rust-module`)
**PR target:** `master` of fork `mariuszs/rtk-java`
**Related:** upstream PR rtk-ai/rtk#1089 (existing mvn filter), maven-mcp project (Java prior art)

## Context

The current `mvn test` filter (landed in PR #1089 on upstream `rtk-ai/rtk`, also present on our fork's `feat/mvn-rust-module`) runs a text state machine on stdout. It compresses 60–99% of tokens on happy paths but **loses diagnostic signal** in four concrete failure modes:

1. **Aggregate-only failures.** Only test names + up to 3 detail lines per failure survive. Stack traces, assertion messages, and root causes are dropped. Agents fall back to `rtk proxy mvn test` or manually `cat target/surefire-reports/*.txt`.
2. **No-tests false happy path.** `BUILD SUCCESS` with `Tests run: 0` renders as `"mvn test: no tests run"`, indistinguishable from a healthy but empty run. Real causes (broken surefire plugin config, wrong `-Dtest=` selector) slip through.
3. **Preamble-dropped plugin errors.** `[ERROR]` lines before the `T E S T S` marker (plugin misconfiguration, validation errors) are fully discarded.
4. **Integration-test failures lost.** `ApplicationContext` load failures, Hibernate connection errors, etc. live in `target/failsafe-reports/*.xml` — the text filter never touches them.

The `maven-mcp` project (`/home/mariusz/projects/maven-mcp`) has a production-grade Java implementation that solves exactly this: a `SurefireReportParser` (JAXP DOM) plus a `StackTraceProcessor` with segment-aware, application-vs-framework-aware, root-cause-preserving truncation. We port that design to Rust and integrate it as a post-text-filter enrichment layer.

The dependency `quick-xml = "0.37"` is already present (used by `dotnet_trx.rs` for `.trx` parsing). The pattern of "parse artifact files after command execution, with time-gate to skip stale reports" is already established by `dotnet_trx.rs::parse_trx_file_since`.

## Goals

- Port `SurefireReportParser` to Rust as `src/cmds/java/surefire_reports.rs` (~500 LoC incl. tests).
- Port `StackTraceProcessor` to Rust as `src/cmds/java/stack_trace.rs` (~400 LoC incl. tests). 1:1 semantic fidelity with the Java original.
- Add `src/cmds/java/pom_groupid.rs` — autodetect `appPackage` from `pom.xml <groupId>` (with parent fallback and `RTK_MVN_APP_PACKAGE` override).
- Extend `filter_mvn_test` flow with a new pure-I/O layer `enrich_with_reports(text, cwd, since, app_pkg)` that:
  - reads `target/surefire-reports/TEST-*.xml` and `target/failsafe-reports/*.xml` only when the text summary suggests a failure or zero-test situation,
  - time-gates files via `mtime >= started_at` captured before `mvn` execution,
  - emits a structured failures section appended to the existing summary line.
- Introduce a no-tests red-flag heuristic: distinguish "clean run with no sources" from "suspicious zero tests (possibly misconfigured surefire)".
- Preserve current token-savings targets on the happy path (≥97%) and maintain ≥85% on enriched-failure paths.
- Ship with reusable fixtures ported from `maven-mcp/src/test/resources/surefire-reports/` plus three synthetic `pom.xml` fixtures for groupId detection.

## Non-Goals

- Rewriting the existing state-machine text filter. The text filter stays pure and its snapshot tests remain untouched.
- Parsing Surefire `.txt` reports. XML is the canonical structured format; `.txt` is a stdout-redirect and duplicates information we already have.
- Flaky test detection, test re-run logic, coverage integration.
- Live streaming of enriched output. Enrichment runs once, after `mvn` exits.
- Parsing arbitrary non-Maven test reports. Scope is Surefire + Failsafe XML only.
- Changing the upstream PR #1089. That PR lands the text filter as-is; this work stacks on top and ships to our fork only.

## Architecture

### Module map

```
src/cmds/java/
├── mod.rs                  (modified — export new modules)
├── mvn_cmd.rs              (modified — integrate enrichment in run_test)
├── surefire_reports.rs     (NEW — XML parser, TEST-*.xml iteration)
├── stack_trace.rs          (NEW — port of StackTraceProcessor)
└── pom_groupid.rs          (NEW — appPackage autodetect)

tests/fixtures/java/
├── surefire-reports/
│   ├── TEST-com.example.PassingTest.xml           (copied from maven-mcp)
│   ├── TEST-com.example.FailingTest.xml           (copied from maven-mcp)
│   ├── TEST-com.example.FailingTestWithLogs.xml   (copied from maven-mcp)
│   ├── TEST-com.example.SkippedTest.xml           (copied from maven-mcp)
│   └── TEST-com.example.ErrorTest.xml             (copied from maven-mcp)
├── failsafe-reports/
│   ├── TEST-com.example.DbIntegrationIT.xml       (synthesized — ApplicationContext failure)
│   └── TEST-com.example.PortConflictIT.xml        (synthesized — socket-bind failure)
├── poms/
│   ├── single-module-pom.xml      (explicit <groupId>)
│   ├── multi-module-parent-pom.xml (parent POM, has <groupId>)
│   ├── child-pom.xml              (no <groupId>, has <parent><groupId>)
│   └── no-groupid-pom.xml         (edge case — returns None)
└── stack-traces/
    ├── simple-assertion.txt       (1-segment, short)
    ├── caused-by-chain.txt        (3-segment, framework-heavy)
    └── suppressed-nested.txt      (Suppressed + indented Caused by)
```

### Data flow

```
run_test(args):
  started_at = SystemTime::now()                  # captured BEFORE exec
  cwd        = std::env::current_dir()?
  app_pkg    = pom_groupid::detect(&cwd)          # cached per-cwd

  output = execute_command("mvn", ...)

  text_summary = filter_mvn_test(&output.stdout)  # PURE — existing tests unchanged
  enriched     = enrich_with_reports(             # NEW I/O layer
      &text_summary, &cwd, started_at, app_pkg.as_deref(),
  )

  tracking::record(...)
  print!("{enriched}")
  exit(output.status.code().unwrap_or(1))
```

`filter_mvn_test(output: &str) -> String` remains unchanged. `enrich_with_reports` is the only new I/O surface in the test path.

### Time-gate rationale

Developers and CI rerun builds frequently; stale XML reports from previous runs would pollute diagnostic output with false failures and inflate counts. `started_at` is captured in `run_test` just before process spawn. `surefire_reports::parse_dir` compares each file's `mtime` against `since` and skips older files. This mirrors `dotnet_trx::parse_trx_file_since`.

## Components

### `surefire_reports.rs`

**Constants (1:1 with maven-mcp):**

```rust
pub const DEFAULT_STACK_TRACE_LINES: usize = 50;
pub const DEFAULT_PER_TEST_OUTPUT_LIMIT: usize = 2000;
pub const DEFAULT_TOTAL_OUTPUT_LIMIT: usize = 10000;
```

**Public types:**

```rust
pub struct TestSummary {
    pub run: u32,
    pub failures: u32,
    pub errors: u32,
    pub skipped: u32,
}

pub enum FailureKind { Failure, Error }

pub struct TestFailure {
    pub test_class: String,
    pub test_method: String,
    pub kind: FailureKind,
    pub message: Option<String>,        // from message="..." attribute
    pub failure_type: Option<String>,   // from type="..." attribute
    pub stack_trace: Option<String>,    // processed via stack_trace::process
    pub test_output: Option<String>,    // combined system-out + system-err, truncated to per-test limit
}

pub struct SurefireResult {
    pub summary: TestSummary,
    pub failures: Vec<TestFailure>,
    pub files_read: usize,
    pub files_skipped_stale: usize,
    pub files_malformed: usize,
}
```

**Public API:**

```rust
pub fn parse_dir(
    dir: &Path,
    since: Option<SystemTime>,
    app_package: Option<&str>,
) -> Option<SurefireResult>;

pub fn parse_dir_with_limits(
    dir: &Path,
    since: Option<SystemTime>,
    app_package: Option<&str>,
    per_test_output_limit: usize,
    total_output_limit: usize,
    stack_trace_lines: usize,
) -> Option<SurefireResult>;
```

Returns `None` when the directory does not exist or contains no `TEST-*.xml` files that pass the time-gate. Returns `Some(SurefireResult { summary: zero, failures: empty, … })` when fresh files exist but none contain failures.

**Parsing strategy (quick-xml streaming):**

Event loop with enum state `{Idle, InTestsuite, InTestcase, InFailureText, InErrorText, InSystemOut, InSystemErr}`. Transitions:

- `Start("testsuite")`: read attributes `tests`, `failures`, `errors`, `skipped`; add to per-file totals. Push state `InTestsuite`.
- `Start("testcase")` inside `InTestsuite`: read `classname`, `name`. Save as `current_testcase`. Push state `InTestcase`.
- `Start("failure")` / `Start("error")` inside `InTestcase`: read `message`, `type` attrs. Push state `InFailureText` / `InErrorText`. Begin text accumulator.
- `Text` inside `InFailureText` / `InErrorText`: append to stack_trace buffer.
- `End("failure")` / `End("error")`: finalize stack trace via `stack_trace::process(raw, app_package, DEFAULT_STACK_TRACE_LINES)`. Record failure. Pop state.
- `Start("system-out")` / `Start("system-err")` inside `InTestcase`: if current testcase has a failure/error already recorded, push state and begin buffer. Otherwise ignore (we don't extract logs from passing tests — matches maven-mcp behavior).
- `End("system-out")` / `End("system-err")`: append buffered text to `TestFailure::test_output` (with `[STDERR]` separator if both present), pop state.
- `End("testcase")`: pop state. Clear `current_testcase`.
- `End("testsuite")`: pop state. Accumulate this file's summary into per-dir totals.

After all files processed, run `apply_total_output_limit` (iterate failures, cumulative length of `test_output`; once exceeds 10000, null out remaining `test_output` fields).

**File selection:**

- `read_dir(dir)` → filter entries where `file_name().starts_with("TEST-") && file_name().ends_with(".xml")`.
- For each, check `metadata().modified()?` against `since`. Increment `files_skipped_stale` on skip.
- On parse failure (malformed XML, IO error), increment `files_malformed`, emit `eprintln!("rtk mvn: skipping malformed {}", name)`, continue. Never panic.

**Error handling:** `anyhow::Result<Option<SurefireResult>>` internally; public wrapper swallows the Err variant and returns `None` after logging. The enrichment layer must never crash `mvn_cmd` — mvn already ran, output must flow.

### `stack_trace.rs`

**Constants (1:1 with maven-mcp):**

```rust
const DEFAULT_ROOT_CAUSE_APP_FRAMES: usize = 10;
const MAX_HEADER_LENGTH: usize = 200;
```

**Types:**

```rust
struct Segment {
    header: String,
    frames: Vec<String>,
}
```

**Public API:**

```rust
pub fn process(
    raw: &str,
    app_package: Option<&str>,   // None = keep all frames (no classification)
    max_lines: usize,            // 0 = no hard cap
) -> Option<String>;
```

Returns `None` iff `raw` is empty or whitespace-only.

**Algorithm (1:1 port):**

1. **`parse_segments(trace)`**: split lines. First non-empty line is top-level header. Each subsequent line starting with `"Caused by:"` (exact prefix match, no leading whitespace — critical: indented `"\tCaused by:"` stays as a frame inside Suppressed blocks) closes the current segment and opens a new one. All other lines append to current segment's frames.

2. **`is_structural_line(line)`**: returns `true` if:
   - `line.trim_start().starts_with("Suppressed:")`, OR
   - `line.starts_with(char::is_whitespace)` AND `line.trim_start().starts_with("Caused by:")` (nested in Suppressed).

3. **`is_application_frame(line, app_package)`**: if `app_package.is_none()`, return `true`. Otherwise strip leading whitespace, strip `"at "` prefix. If remainder starts with `app_package`, return `true`. Lines like `"\t... 42 more"` return `false`.

4. **`add_collapsed_frames(output, frames, app_package)`** (top-level + intermediate segments):
   - Iterate frames. Count consecutive framework frames.
   - When hitting app or structural frame: if counter > 0, push `"\t... N framework frames omitted"` and reset. Push the app/structural frame (structural goes through `truncate_header`).
   - At end of loop: flush remaining framework-frame counter.

5. **`add_root_cause_frames(output, frames, app_package)`**:
   - Same as above, but also count `app_frames_emitted`. Structural frames bypass the cap; non-structural app frames stop being emitted once `app_frames_emitted >= DEFAULT_ROOT_CAUSE_APP_FRAMES`.

6. **`apply_hard_cap(output_lines, segments, max_lines)`**:
   - Segment count ≤ 1: `output[..max_lines]`.
   - Multi-segment: find root-cause header (last segment's truncated header) in output. If its index ≥ max_lines − 1, build synthetic: `[top_header, "\t... (intermediate frames truncated)", root_header, …root-cause frames until cap]`. Otherwise truncate at max_lines.

7. **`truncate_header(line)`**: if `line.chars().count() > MAX_HEADER_LENGTH`, return first `MAX_HEADER_LENGTH` chars + `"..."`. UTF-8 safe via `utils::truncate`.

**`process()` orchestration:**

```
segments = parse_segments(raw.trim())
if segments.empty: return Some(raw.trim())

let filter = app_package.is_some_and(|p| !p.is_empty())
let mut out = Vec::new()
out.push(truncate_header(&segments[0].header))

if segments.len() == 1:
    add_collapsed_frames(&mut out, &segments[0].frames, app_package, filter)
else:
    add_collapsed_frames(&mut out, &segments[0].frames, app_package, filter)
    for seg in &segments[1..segments.len()-1]:
        out.push(truncate_header(&seg.header))
        add_collapsed_frames(&mut out, &seg.frames, app_package, filter)
    let root = segments.last().unwrap()
    out.push(truncate_header(&root.header))
    add_root_cause_frames(&mut out, &root.frames, app_package, filter)

if max_lines > 0 && out.len() > max_lines:
    out = apply_hard_cap(out, segments, max_lines)

Some(out.join("\n"))
```

### `pom_groupid.rs`

**Public API:**

```rust
pub fn detect(cwd: &Path) -> Option<String>;
```

**Algorithm:**

1. If env var `RTK_MVN_APP_PACKAGE` is set and non-empty, return its value (override always wins).
2. Check thread-local cache `(PathBuf, Option<String>)` keyed by `cwd`. If hit, return cached value.
3. Resolve `cwd.join("pom.xml")`. If missing, cache `None` and return.
4. Stream-parse pom.xml with quick-xml, tracking element stack depth:
   - When inside top-level `<project>` (depth 1 under `<project>`), catch `<groupId>` → capture text, return after `</groupId>`.
   - If no top-level `<groupId>` found during first pass, fall back: look for `<project>/<parent>/<groupId>` (depth 2).
   - Return first match, or `None`.
5. Cache result. Return.

**Streaming impl notes:**

- Single pass with a small state machine: track stack of tag names.
- Short-circuit: once first groupId found at valid depth, close reader and return.
- Malformed XML: return `None` silently (never panic).

### `mvn_cmd.rs` integration

**New helper:**

```rust
pub(crate) fn enrich_with_reports(
    text_summary: &str,
    cwd: &Path,
    since: SystemTime,
    app_package: Option<&str>,
) -> String;
```

**Logic:**

```
if !text_summary.starts_with("mvn "):
    return text_summary.to_owned()   // defensive — shouldn't happen

let looks_clean   = contains("passed (") && !contains("failed") && !contains("BUILD FAILURE")
let zero_tests    = text_summary == "mvn test: no tests run" || contains("0 passed")
let has_failures  = contains(" failed") || contains("BUILD FAILURE")

if looks_clean && !zero_tests:
    return text_summary.to_owned()   // optimization — happy path, no I/O

let sf = surefire_reports::parse_dir(&cwd.join("target/surefire-reports"), Some(since), app_package)
let fs = surefire_reports::parse_dir(&cwd.join("target/failsafe-reports"), Some(since), app_package)

match (zero_tests, has_failures, &sf, &fs):
    (true, _, None, None) =>
        "mvn test: 0 tests executed — surefire nie wykrył testów. \
         Sprawdź pom.xml (plugin surefire configuration) lub uruchom: rtk proxy mvn test"
    (true, _, Some(r), _) if r.summary.run > 0 =>
        // reports show tests ran; text said zero — trust reports
        render_enriched(text_summary, sf.as_ref(), fs.as_ref(), zero_tests)
    (_, true, None, None) =>
        format!("{text_summary}\n(no XML reports found — check target/surefire-reports/ \
                 or run: rtk proxy mvn test)")
    _ =>
        render_enriched(text_summary, sf.as_ref(), fs.as_ref(), zero_tests)
```

**Rendering format (`render_enriched`):**

```
<text_summary verbatim>

Failures (from surefire-reports/):
1. com.example.UserServiceTest.shouldReturnUser
   AssertionFailedError: expected:<200> but was:<404>
   org.opentest4j.AssertionFailedError: expected:<200> but was:<404>
     at com.example.UserServiceTest.shouldReturnUser(UserServiceTest.java:42)
     ... 8 framework frames omitted

2. com.example.OrderServiceTest.shouldHandleNull
   AssertionError: Unexpected exception
   java.lang.AssertionError: Unexpected exception: NullPointerException
     at com.example.OrderServiceTest.shouldHandleNull(OrderServiceTest.java:55)

Integration failures (from failsafe-reports/):
1. com.example.DbIntegrationIT.shouldConnect
   Caused by: HibernateException
   Caused by: org.hibernate.HibernateException: Unable to acquire JDBC Connection
     at com.example.DbIntegrationIT.shouldConnect(DbIntegrationIT.java:88)
     ... 14 framework frames omitted

  captured stderr:
    Connection refused (Connection refused)

(reports: 12 surefire, 1 failsafe, 3 stale files skipped)
```

Key rendering rules:

- Cap at **10 failures per source** (10 surefire + 10 failsafe, each independently). Append `"\n... +N more failures"` under the relevant section heading when truncated. Matches the text filter's existing `MAX_FAILURES_SHOWN = 10` convention.
- `message` field goes on the second line (short summary from `<failure message="..." type="...">` attributes; falls back to the first line of the stack trace if `message` is empty). Full stack trace on subsequent lines, indented 5 spaces.
- `test_output` (combined `<system-out>` + `<system-err>` as stored in `TestFailure::test_output`), when present and non-empty, renders as a single block labeled `captured output:` (labeling distinguishes stdout-only, stderr-only, and combined via the `[STDERR]` separator already embedded in the buffer).
- Footer line `(reports: …)` only when at least one file was read or skipped. Format: `(reports: N surefire, M failsafe, K stale files skipped, J malformed)` — omit count-components that are zero.

## Data / Models

Fixtures from maven-mcp are copied verbatim to `tests/fixtures/java/surefire-reports/`. Synthesized failsafe fixtures:

- `TEST-com.example.DbIntegrationIT.xml`: 3-segment Caused-by chain (wrapper → SpringContextException → HibernateException), 40+ frames, system-err with JDBC error.
- `TEST-com.example.PortConflictIT.xml`: 1-segment, short, SocketException, system-err with "address already in use".

POM fixtures are minimal:

```xml
<!-- single-module-pom.xml -->
<project><groupId>com.example.app</groupId><artifactId>app</artifactId><version>1.0</version></project>

<!-- child-pom.xml -->
<project>
  <parent><groupId>com.example.app</groupId><artifactId>parent</artifactId><version>1.0</version></parent>
  <artifactId>child</artifactId>
</project>
```

## Tests

### Unit

**`surefire_reports::tests`:**

- `parse_dir_happy` — single passing XML → `summary{run=3, failures=0}`, `failures.is_empty()`.
- `parse_dir_with_failures` — FailingTest → 2 `TestFailure` entries with stack traces and messages.
- `parse_dir_with_logs` — FailingTestWithLogs → `test_output` contains stdout + `[STDERR]` + stderr; passing test's `<system-out>` NOT extracted.
- `parse_dir_multi_file` — 5 XMLs in dir → summary aggregates across all files.
- `parse_dir_time_gate` — fixtures copied into a `tempfile::TempDir` with `filetime::set_file_mtime` set before `since` → `files_skipped_stale == n`. Requires adding `filetime = "0.2"` to `[dev-dependencies]` (not present today; runtime code uses `std::fs::Metadata::modified()` only).
- `parse_dir_malformed_graceful` — corrupt XML in dir → `files_malformed == 1`, other files still parsed.
- `parse_dir_missing_returns_none` — non-existent dir → `None`.
- `parse_dir_empty_returns_none` — dir exists but no `TEST-*.xml` → `None`.
- `total_output_limit_applied` — 10+ failures with large test_output → later entries have `test_output == None`.

**`stack_trace::tests`** (port tests from `StackTraceProcessorTest.java`; enumerate during implementation):

- Single segment, no filter → returns verbatim.
- Single segment, with app_package → framework frames collapsed.
- Three-segment Caused-by chain → top/intermediate collapsed, root-cause preserved.
- Suppressed block with indented `Caused by:` → structural lines preserved, not parsed as segments.
- Hard cap with root cause beyond limit → synthetic intermediate-truncated output.
- Hard cap with root cause within limit → straight truncate.
- Header >200 chars → truncated with `"..."`.
- UTF-8 in frames (Japanese class names) → no panic, char-boundary-safe truncation.

**`pom_groupid::tests`:**

- single-module POM → `Some("com.example.app")`.
- child POM (no groupId, has parent.groupId) → `Some("com.example.app")`.
- no-groupid POM → `None`.
- missing pom.xml → `None`.
- env var `RTK_MVN_APP_PACKAGE=com.override` set → returns `"com.override"` regardless of pom content.
- malformed pom.xml → `None`, no panic.

### Integration (in `mvn_cmd.rs`)

- `enrich_happy_path_no_io` — text `"mvn test: 32 passed (11.6s)"` → returns verbatim, no directory reads (verify via `tempdir` with no `target/` present).
- `enrich_with_failures_snapshot` — copy surefire fixtures to `tempdir/target/surefire-reports/`, set mtime to `now`, invoke `enrich_with_reports` → insta snapshot.
- `enrich_with_both_reports_snapshot` — surefire + failsafe fixtures → snapshot with both sections.
- `enrich_red_flag_no_tests` — text `"mvn test: no tests run"`, empty `target/` → returns red-flag message.
- `enrich_stale_reports_skipped` — fixtures with `mtime` before `since` → `files_skipped_stale > 0`, no failures in output.
- `enrich_malformed_xml_does_not_crash` — one malformed XML in fixture dir → output still produced, `files_malformed == 1`.

### Token savings

- `savings_enriched_failures` — real multi-module `mvn verify` log (~2000 lines) with synthesized surefire + failsafe XMLs → enriched output ≤ 400 lines. Assert `savings >= 85%`.
- `savings_happy_path_unchanged` — happy path fixture → assert `savings >= 97%`. No enrichment, no I/O.

### Snapshots (insta)

- `snap_enriched_surefire_only.snap`
- `snap_enriched_surefire_and_failsafe.snap`
- `snap_enriched_truncated_stack.snap` — 200-frame trace, cap at 50 lines.
- `snap_enriched_multi_caused_by.snap` — 3-segment chain with root-cause preserved.
- `snap_red_flag_no_tests.snap`
- `snap_fallback_no_xml_reports.snap`

### Performance

`hyperfine` check: `target/release/rtk mvn test` on a project with 50 `TEST-*.xml` files must complete within +5ms vs. text-only path. I/O is the only new cost; budget is generous to account for disk cache cold-start.

## Implementation Plan

Commits, in order, each reviewable in isolation:

1. **`feat(mvn): port StackTraceProcessor to Rust`** (~400 LoC)
   - `src/cmds/java/stack_trace.rs` + tests
   - Fixtures: `tests/fixtures/java/stack-traces/*.txt`
   - No integration yet. Module compiles standalone with full test coverage.

2. **`feat(mvn): add SurefireReportParser`** (~500 LoC)
   - `src/cmds/java/surefire_reports.rs` + tests
   - Fixtures: 5 XMLs from maven-mcp + 2 synthesized failsafe XMLs
   - Depends on `stack_trace.rs` for stack processing.

3. **`feat(mvn): autodetect appPackage from pom.xml`** (~150 LoC)
   - `src/cmds/java/pom_groupid.rs` + tests
   - Fixtures: 4 POM files
   - Independent of parsers above.

4. **`feat(mvn): enrich test output with XML reports`** (~250 LoC)
   - Modify `mvn_cmd.rs`: capture `started_at`, call `detect`, call `enrich_with_reports`.
   - Implement `enrich_with_reports` + `render_enriched`.
   - Snapshot tests + integration tests.
   - No-tests red-flag heuristic.
   - Existing `filter_mvn_test` tests untouched.

5. **`docs(mvn): document surefire/failsafe XML enrichment`**
   - Update `src/cmds/java/README.md`.
   - Add section to `CHANGELOG.md`.
   - Note `RTK_MVN_APP_PACKAGE` env var.

## Risks / Trade-offs

**[Risk] Surefire XML format drift.** Surefire 3.x has been stable and the format is widely consumed (IntelliJ, CI, Maven itself). Any drift would break our snapshot tests first and fixes would be local. Mitigation: snapshot tests on real-world fixtures, port maven-mcp's proven parser.

**[Risk] File I/O on every failing `mvn test`.** Directory read + N file opens + N XML parses. On a 50-file repo this is ~5ms cold cache, <1ms warm. Acceptable for diagnostic path; skipped entirely on happy path via `looks_clean` short-circuit.

**[Risk] `appPackage` autodetect picks wrong package in polyglot monorepos.** A repo with `pom.xml` at root but mixed Java/Kotlin/Scala modules might have a groupId that doesn't match the failing test's package. Consequence: framework-frame collapsing disabled (all frames kept). This degrades gracefully — output is slightly noisier but never wrong. Override via `RTK_MVN_APP_PACKAGE`.

**[Risk] Stale reports from previous runs slip through time-gate.** Only if a previous `mvn` wrote XML, the user then `touch`ed the files, then ran `rtk mvn test` without the filter re-writing them (e.g., compilation failed before surefire ran). Result: yesterday's failures shown as today's. Mitigation: the time-gate uses `mtime`; surefire always rewrites. Only a manual `touch` defeats this, which is user error. Worst-case impact is bounded to diagnostic noise, never incorrect exit codes.

**[Trade-off] Enrichment increases output size on failures.** Savings drop from 90%+ to ~85% on enriched paths. This is the whole point: we trade compression for signal exactly when signal matters. Happy path remains maximally compressed.

**[Trade-off] Port duplicates maven-mcp's Java code.** Future fixes require updates in two places. Mitigated by the algorithms being small, well-defined, and stable. No live sync needed.

**[Trade-off] We read XML even when text filter correctly identified all failures.** Accepted: the XML is the source of truth. Text parsing of stderr `[ERROR]` messages is fragile; XML is canonical. If counts disagree between text and XML, XML wins in the enriched rendering.

## Open Questions

None blocking. The design is a direct port with explicit decisions above.

## References

- Java original: `/home/mariusz/projects/maven-mcp/src/main/java/io/github/mavenmcp/parser/SurefireReportParser.java`
- Stack trace original: `/home/mariusz/projects/maven-mcp/src/main/java/io/github/mavenmcp/parser/StackTraceProcessor.java`
- Design doc (Java): `/home/mariusz/projects/maven-mcp/openspec/changes/archive/2026-02-15-surefire-parser-and-test-tool/design.md`
- Rust prior art for XML-report parsing: `src/cmds/dotnet/dotnet_trx.rs`
- Related PR (upstream base): `rtk-ai/rtk#1089` (feat(mvn): add Maven (Java) filter module)
- Fork: `mariuszs/rtk-java` (PR target: `master`)
