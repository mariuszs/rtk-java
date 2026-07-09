# mvn native-format fidelity — design

**Date:** 2026-07-09
**Status:** approved (design), pending implementation plan
**Module:** `src/cmds/jvm/mvn_cmd.rs` (+ snapshots, `surefire_reports.rs` renderers)

## Motivation

RTK's output must read as *"a shorter version of the real command"* — a valid, useful
subset of the tool's own output, never a different format the LLM wouldn't expect. Hooks
rewrite `mvn` → `rtk mvn` silently, so the agent parses RTK's output believing it is Maven.

An audit (2026-07-09) of the mvn filter against this principle found:

- **`guard::never_worse` — fully satisfied.** Every stdout path routes through
  `runner::run_filtered` → `emit_guarded` → `never_worse`. No change needed.
- **Format fidelity — partial by design.** The filter is a *summarizer*: nearly every
  headline is a synthetic `mvn <goal>: …` line Maven never prints, plus RTK-invented
  headers (`Failures (from surefire-reports/):`, `classes: <path>`, `(multi-goal)`,
  `mvn: ok`). The compile path is the exception — it is already a prefix-preserving native
  subset (`[INFO]`/`[ERROR]`/`[WARNING]` verbatim).

### Empirical grep evidence (the decisive input)

A search of the local Claude Code session corpus (`~/.claude/projects`, 1583 transcripts /
803 MB) shows agents filter Maven output by log-level prefixes *frequently and with
anchors*:

- `grep -E "^\[ERROR\]"` (6×), `grep -E "^\[ERROR\].*\.java:\["` (3×) — anchor on
  `[ERROR]` to find compile errors
- `grep -nE "^\[INFO\]"` (6×), `grep -n "^\[INFO\]"` (7×), `grep -v "^\[INFO\]"` (6×),
  `sed 's/\[INFO\]//'` — anchor/strip `[INFO]`
- `grep -v "^\[INFO\]\|^\[WARNING\]"`, `grep -E "\[ERROR\].*\.java|BUILD"`

The `^` anchor means the prefix must be at line start — stripping it kills these greps
entirely. See memory `project_agents_grep_maven_prefixes`. This is the same principle as
the prior "maven-native patterns" commit (agents grep `Tests run:` / `BUILD` / `<<<
FAILURE!`), extended: the tokens agents grep include the `[INFO]`/`[ERROR]` prefixes.

## Design principle

> **RTK mvn output = a prefix-preserving subset of Maven's own lines, plus XML enrichment
> rendered in Maven's own line shape. Match the compile path (the existing gold standard).
> Delete every synthetic `mvn <goal>:` line and every RTK-invented header. Drop
> `Total time`.**

This unifies the test-summary path with the compile path rather than inventing a new style.
It moves the fork toward upstream's pure-subset approach (upstream authored the fidelity
principle; its filter embodies it) **while keeping the fork's differentiator: XML
enrichment** (per-class breakdown, stack-trace digests reconstructed from
`surefire-reports/` / `failsafe-reports/`), which upstream has none of.

### Why this does not cost compression

Unlike a full upstream-style subset (which keeps *all* Maven lines, incl. passing-class
blocks, landing ~85%), this change mostly *removes* invented lines and *renames* headers.
Token savings stay ~99% for tests. Keeping prefixes adds ~2 tokens/line on a 3–6 line
summary — negligible, and it buys grep compatibility. The only surfaces that grow slightly
are `clean`/`checkstyle` (`[INFO] BUILD SUCCESS` vs a one-liner) — pennies, and
`never_worse` caps them anyway.

## Per-surface target formats

Preserve whatever log-level prefix Maven emits on each retained line. Reconstructed
enrichment lines use Maven/surefire's own vocabulary and prefix.

### test pass
```
[INFO] Tests run: 183, Failures: 0, Errors: 0, Skipped: 0
[INFO] Tests run: 12 -- in com.example.FooTest      ← breakdown (inline, ≤5 classes)
[INFO] BUILD SUCCESS
```
- Delete synthetic `mvn test: 183 passed (t)`.
- **Breakdown (option c):** reconstruct surefire's real per-class line
  `[INFO] Tests run: N -- in <FQCN>`, trimming the zero-valued `Failures/Errors/Skipped`
  and `Time elapsed` fields on a clean pass. This is reconstruction of a real (reactor-
  suppressed) surefire line, not fabrication. Inline when ≤`MAX_INLINE_CLASSES` (5);
  otherwise the full breakdown goes to a tee digest file and the summary carries a
  reference line (below).
- Skipped tests: keep a maven-plausible line; drop the RTK `skipped:` prefix in favour of
  surefire's own shape where practical (plan detail).

### test fail  (≈ upstream's shape)
```
[ERROR] Tests run: 5, Failures: 2, Errors: 0, Skipped: 0
[INFO] BUILD FAILURE

[ERROR] Failures:
[ERROR]   com.example.EmailParserTest.should_extract_domain:42 <<< FAILURE!
[ERROR]     AssertionFailedError: expected:<x> but was:<y>     ← enrichment, same shape
```
- Delete synthetic `mvn test: 5 run, 2 failed (t)` and the `1. 2.` renumbering.
- `[ERROR] Failures:` is Maven-native — keep verbatim (resolves the old
  `Failures (from surefire-reports/):` invention).
- Failure lines use Maven's `Class.method:line <<< FAILURE!` coord form; XML-sourced
  stack frames continue underneath in the same prefixed shape.
- **surefire vs failsafe distinction** (verify runs): both are `[ERROR] Failures:`
  natively. Keep a single section; where the source distinction adds value, mark it inline
  in a maven-plausible way (e.g. an `-- in …IT` suffix already identifies integration
  tests) rather than a separate RTK header. Plan detail.

### no tests
```
[WARNING] No tests were executed!
```
- Use surefire's own native line `[WARNING] No tests were executed!` instead of RTK prose.
- Keep a single short RTK diagnostic hint **only** when it adds real value (silent
  misconfiguration: zero tests *and* no reports found), never naming `rtk`.

### compile errors
- Already a prefix-preserving native subset
  (`[INFO] BUILD SUCCESS`, `[ERROR] /path/File.java:[L,C] …`) — the gold standard the other
  surfaces are being aligned to. **One change only:** drop the `[INFO] Total time: …` line
  here too (add it to the subset drop set), so the "no Total time" rule holds uniformly.

### clean / checkstyle  (hard subset)
```
[INFO] BUILD SUCCESS
```
- Delete `mvn clean: deleted N targets` and `mvn checkstyle: ok` — the counts add little.
- On failure: `[INFO]/[ERROR] BUILD FAILURE` + the native error/violation lines.

### dependency:tree / dependency:list
- Keep Maven's native tree/list lines verbatim (they already carry `[INFO]` prefixes and
  are natively compact).
- Delete synthetic `mvn dependency:list: N unique deps`, `no output`,
  `no dependencies found`. Empty results fall back to Maven's own `[INFO] BUILD SUCCESS`.

### multi-goal
- Replace the `mvn <goals> (multi-goal)` header and per-goal `mvn: ok` markers with
  Maven's native `Reactor Summary` block plus the composed per-goal sections (each already
  reshaped per the rules above).

### digest reference (breakdown overflow)
- Replace the RTK `classes: <path>` line with the existing tee-hint house format,
  e.g. `[full per-class report: ~/…_classes.log]` — matching `tee::format_hint`
  (`[full output: …]`), which is not RTK-branded.

## Guard & tracking

No change. `runner::run_filtered` already wraps every emit path in `guard::never_worse`;
the digest file is written out-of-band via `tee::force_tee_display` and does not count
against the stdout token budget. `never_worse` remains the floor guaranteeing RTK never
emits more tokens than raw `mvn`.

## Testing strategy

- **Snapshots:** update the ~8 affected jvm snapshots (`filter_*`, `snapshot_enriched_*`,
  `snapshot_red_flag_no_tests`, `snapshot_verify_auth`, maven4 pass, multi-goal) to the new
  prefix-preserving shapes via `cargo insta`.
- **Token savings:** re-baseline the affected `*_savings` thresholds. Expectation: test
  paths stay ≥94–99%; `clean`/`checkstyle` may dip a few points (still ≫60% floor). Any
  threshold that must move gets a comment explaining why (as with the prior 95→94 change).
- **New guard tests** (regression fences for the principle):
  - no output contains a synthetic `mvn <goal>:` headline;
  - no output contains `(from surefire-reports/)` / `(from failsafe-reports/)` /
    `(multi-goal)` / `mvn: ok` / `classes:`;
  - no filtered stdout contains the literal `rtk` (extends the fix already shipped for the
    red-flag branches);
  - retained lines keep their `[INFO]`/`[ERROR]`/`[WARNING]` prefix (anchored-grep fence).
- **Quality gate:** `cargo fmt --all && cargo clippy --all-targets && cargo test --all`.
  Note: 18 `hooks::*` tests fail locally for environment reasons (permission decisions vs
  local `~/.claude` settings), green on CI — see memory `project_local_gate_git_perm_tests`.

## Non-goals

- Not adopting upstream's full pure-subset filter (would drop XML enrichment and ~14pp of
  compression). We keep enrichment, reshaped to Maven's line form.
- Not keeping `Total time` on *any* path (dropped by decision — adds no signal), including
  the compile subset which retains it today. This also removes the `(t)` duration currently
  shown on test summaries.
- No changes to routing, phase detection, tracking, or the guard.
- Not touching the gradlew filter.

## Risks & trade-offs

- **Snapshot churn is large** — most jvm snapshots change. Mechanical but must be reviewed
  carefully so a real regression doesn't hide in the noise.
- **Small savings dip on clean/checkstyle** — accepted; `never_worse` bounds it.
- **Enrichment prefixing** — reconstructed breakdown/stack lines get `[INFO]`/`[ERROR]`
  prefixes to match surefire; justified because surefire genuinely emits those lines (the
  compact reactor suppresses them), so this is faithful reconstruction, not fabrication.
