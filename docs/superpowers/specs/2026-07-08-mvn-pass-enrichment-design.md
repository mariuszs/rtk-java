# mvn pass-run enrichment — design

Date: 2026-07-08
Status: approved (brainstormed interactively)

## Problem

Surefire/failsafe XML enrichment currently fires only on failing or zero-test
runs — `enrich_with_reports` early-returns for clean summaries
(`looks_clean && !zero_tests`). Transcript analysis of real usage (auth,
skiller, map, eval) showed ~134 manual digs into `target/surefire-reports/`,
roughly half of them on **passing** runs: Claude wanted per-class stats,
confirmation of what actually ran (`-Dtest=...`), and names of skipped tests.

## Goals

- Passing test-like runs (`test`, `verify`, `surefire:test`, `failsafe:*`,
  multi-goal chains) get a compact, useful enrichment.
- Keep inline output short (rtk's core value); full detail goes to a digest
  file Claude can read on demand — same proven pattern as tee logs.
- Never degrade the happy path: any enrichment problem falls back to the
  unchanged text summary.

Explicitly out of scope (user decision): "slowest tests" ranking — for
performance work a single test can be re-run directly.

## Design

### Data flow

`enrich_with_reports` no longer early-returns on clean summaries. It always
runs the existing `discover_report_dirs` + `collect_reports` (time-gated by
`since`), for both passing and failing runs. Both the single-goal path
(`run_tests_like`) and multi-goal path (`run_multi_goal`) already route
through it, so no routing changes are needed.

### Parser extension (`surefire_reports.rs`)

`SurefireResult` gains two fields:

- `suites: Vec<SuiteStat>` — `{ class_name, tests: u32, skipped: u32,
  time_secs: f64, module: Option<String> }`. Name, counts and time come from
  `<testsuite>` attributes (one entry per XML file). Module is derived from
  the report directory (parent of `target/`) by the collect layer — the XML
  itself does not know it.
- `skipped_tests: Vec<SkippedTest>` — `{ class, method, reason:
  Option<String> }`, from `<skipped message="...">` elements the streaming
  parser already passes over.

### Digest file

A condensed report written next to the tee log:
`~/.local/share/rtk/tee/<ts>_<label>.classes.txt` (reuse path helpers from
`src/core/tee.rs`). Contents:

```
# mvn test 2026-07-08T21:15 — 214 passed, 3 skipped (module: class breakdown)
services: UserFacadeTest 8 (2.1s), TokenParserTest 12 (0.3s), ...
web:      ScimControllerIT 5 (4.0s), ...
skipped:  UserFacadeTest.should_x (@Disabled "flaky on CI"), ...
```

Written whenever reports parsed successfully — on passing **and** failing
runs (transcripts show stats digs happen on failures too). On failing runs
the output gains only the ` — classes: <digest path>` reference after the
existing failure blocks; the inline class list never appears there.

### Hybrid inline rendering

Constants: `MAX_INLINE_CLASSES = 5`, `MAX_INLINE_SKIPPED = 3`.

- ≤5 classes → inline list, one per line: `UserFacadeTest: 8 (2.1s)`
  (digest still written, for consistency).
- >5 classes → single summary line + ` — classes: <digest path>`.
- `K skipped` always appended to the summary line when K>0; skipped names
  inline only when ≤3, otherwise digest-only.

### Error handling

The happy path must never get worse:

- No XML found, all reports stale (`files_read == 0`), parse error, or digest
  write failure → return the text summary unchanged, silently. Red-flag
  messaging stays confined to the existing fail/zero-tests paths.
- No cross-validation of XML totals vs text-summary counts; the breakdown is
  labelled as coming from reports and presented as-is.

### Testing (TDD, real fixtures)

- New fixtures: real surefire XMLs (small run ≤5 classes, large reactor run,
  run with `@Disabled` skips), sanitized per repo convention (com.example,
  /home/user, greek-letter table names).
- Snapshot tests: inline variant (small run), file-reference variant (large
  run), skipped-names variants (≤3 inline, >3 digest-only).
- `enrich_happy_path_passes_through_without_io` is deliberately replaced by a
  new invariant: clean run with **no reports found** → summary unchanged.
- Token-savings tests (≥60%) unchanged. Post-run XML parsing cost is
  negligible relative to mvn runtime and does not touch the <10ms startup
  target.

## Alternatives considered

- **Separate lightweight pass-only enricher** — smaller blast radius but
  duplicates dir discovery, time-gating and rendering. Rejected: saves
  microseconds nobody notices.
- **Console-derived breakdown (no XML)** — parse `-- in com.example.FooTest`
  lines. Rejected: no skip reasons, and Maven 3.9 vs 4 console format drift
  is exactly the bug class XML avoids.
- **Always inline / always file-only** — rejected in favour of the hybrid:
  inline for tiny runs (`-Dtest=...`) avoids a follow-up Read round-trip;
  file reference keeps big reactor runs to one line.
