# mvn Multi-Goal Signal-Aware Filter — Design

**Status:** Draft → ready for implementation planning
**Branch:** `feat/mvn-multi-goal` (off `master`)
**PR target:** `master` of fork `mariuszs/rtk-java`
**Related:** PR rtk-ai/rtk#1089 (mvn filter), `feat/mvn-surefire-xml` (XML enrichment) — both landed on fork `master`

## Context

RTK's Maven module routes a command to **one** filter, chosen by the **first** positional goal. This is correct for every other RTK ecosystem (dotnet, cargo, go, npm) because those are single-verb tools — you run `dotnet build` *or* `dotnet test`, never `dotnet build test`. Maven is the only supported ecosystem where chaining goals/phases in one invocation (`mvn clean install`, `mvn clean test-compile checkstyle:check`) is idiomatic and common.

The Clap `MvnCommands` subcommand enum inherits the single-verb model: `rtk mvn clean test-compile checkstyle:check -Dskip.npm -q` parses `clean` as the `Clean` variant and captures `test-compile checkstyle:check -Dskip.npm -q` as its `trailing_var_arg`. The whole combined output then runs through `filter_mvn_clean`, which extracts only the clean summary and **discards** the test-compile errors, the checkstyle violations, and the `BUILD SUCCESS/FAILURE` line.

### Verified symptom (real session transcript)

Command (after RTK hook rewrite):
```
rtk mvn clean test-compile checkstyle:check -Dskip.npm -q 2>&1 | tail -25; echo "EXIT=$status"
```
RTK output:
```
mvn clean: nothing to clean (?)
```
The build genuinely succeeded — the unfiltered `rtk proxy` run showed `You have 0 Checkstyle violations.`, `BUILD SUCCESS`, `Total time: 25.459 s`. All of that signal was thrown away by the clean filter.

Two non-bugs were ruled out during analysis:
- **Exit-code propagation already works** (`runner::run` returns the child code, `main` calls `process::exit`). The empty `EXIT=` in the transcript was a shell artifact (`$status` after a pipe in the Bash tool's shell), not RTK.
- **No TTY gating exists.** A prior session's compact summary recorded "summary line only in TTY" as fact; this was a misdiagnosis. There is no `is_terminal`/`atty` anywhere in the mvn or runner path — the missing summary was this same goal-routing bug.

Out of scope this round (noted, separate work): the `grep`/`rg` hook rewrite not translating flags/regex dialects (`--type` reaching plain grep; BRE `\|` alternation reaching `rg`).

## Goals

1. A multi-goal invocation preserves the signal the user actually needs: all `[ERROR]` lines, `BUILD SUCCESS/FAILURE` + `Total time`, per-module test counts, and `You have N Checkstyle violations`.
2. Reuse the existing, already-tested single-goal sub-filters rather than inventing a parallel parser.
3. Never swallow the whole output again — a degraded path must still surface signal.
4. Leave the recently-stabilized single-goal filters (PR #1089, surefire-XML) untouched in behavior.

## Non-Goals

- `-q` handling for single-goal invocations (possible later extension).
- The `grep`/`rg` hook rewrite issue (separate module, separate round).

Surefire/failsafe XML enrichment **is in scope** for multi-goal (see below). Without it, `mvn clean verify` with a failing integration test shows `BUILD FAILURE` but no cause — failsafe failures (`ApplicationContext` load errors, etc.) live only in `failsafe-reports/*.xml`, which stdout never carries. Excluding enrichment would cripple the most common chained patterns (`clean test`, `clean verify`, `clean install`).

## Approach (chosen: Approach 3 — single capture variant)

Replace the typed `MvnCommands` subcommand enum with a single raw-arg capture, and move **all** goal routing into `mvn_cmd.rs`. This removes the root cause (the Clap subcommand model's mismatch with Maven's grammar) rather than patching around it. The existing `route_goal`/`GoalRouting` already perform goal-name-based routing for compile/checkstyle/passthrough inside `run_other`; this design promotes that to the primary, exhaustive router.

Bonus correctness: this also fixes leading global flags (`mvn -q test`, `mvn -pl core test`), which the current subcommand model mishandles because Clap cannot match a subcommand that begins with `-`.

**Blast radius (verified):** `MvnCommands` is referenced only in `main.rs` (enum + two `Command` fields + `dispatch_mvn`). No other module depends on the variant structure. One existing test (`test_route_goal`) needs updating.

### Architecture

**`main.rs`:** `Commands::Mvn`/`Mvnd` change from `{ command: MvnCommands }` to `{ args: Vec<OsString> }` (`trailing_var_arg = true, allow_hyphen_values = true`). `MvnCommands` is deleted. `dispatch_mvn` collapses to a single call: `mvn_cmd::dispatch(binary, &args, verbose)`.

**`mvn_cmd.rs` — new entry point `dispatch`:**
```text
let goals = parse_goals(args);          // Vec of goal tokens, in order
match goals.len() {
    0 => run_passthrough_all(binary, args, verbose),      // mvn -version, --help, bare mvn
    1 => route_goal(goals[0]) → existing run_test / run_verify / run_clean /
                                run_compile_like / run_checkstyle / run_dep_tree
                                (or run_passthrough_all for install/package/deploy/spring-boot:run)
    _ => run_multi_goal(binary, args, verbose),            // ≥2 goals
}
```
For the single-goal case, the matched goal token is removed from `args` before delegating (the existing `run_*` functions prepend their canonical goal name, so the token must not be duplicated). The existing single-goal filter functions are called unchanged. `run_other` and its passthrough tail fold into `dispatch` + `run_passthrough_all`. `GoalRouting` is widened to `{ Test, Verify, Clean, Compile, Checkstyle, DepTree, Passthrough }`.

### Goal detection — `parse_goals` (the core, pure function)

A token in `args` is a **goal** iff:
1. it does not start with `-` (flags: `-q`, `-Dskip.npm`, `--fail-at-end`), **and**
2. it is not the value of a preceding value-taking option — `-pl`/`--projects`, `-P`/`--activate-profiles`, `-f`/`--file`, `-T`/`--threads`, `-rf`/`--resume-from`, `-s`/`--settings`, `-gs`/`--global-settings`, `-l`/`--log-file`, `-b`/`--builder`, `-t`/`--toolchains`, **and**
3. it looks like a goal — a known Maven **lifecycle phase** (clean / default / site lifecycles) **or** the `plugin:goal` form (contains `:`).

Condition 3 alone rejects `-pl core` (`core` is neither a phase nor contains `:`); condition 2 catches the adversarial `-rf :module-b` (`:module-b` contains `:` but is a flag value). Belt-and-suspenders.

Known phases (allowlist): `pre-clean clean post-clean validate initialize generate-sources process-sources generate-resources process-resources compile process-classes generate-test-sources process-test-sources generate-test-resources process-test-resources test-compile process-test-classes test prepare-package package pre-integration-test integration-test post-integration-test verify install deploy pre-site site post-site site-deploy`.

### Multi-goal filter — `run_multi_goal` + `filter_mvn_multi`

`run_multi_goal` orchestrates; the split + per-group sub-filter + compose path is the pure, snapshot-tested `filter_mvn_multi`. XML enrichment (which needs filesystem access) is layered on top in `run_multi_goal`.

1. `run_multi_goal` captures `started_at` (SystemTime) and `app_packages` (`pom_groupid::detect`) — exactly as `run_tests_like` does — then builds the mvn command from the original args **minus `-q`/`--quiet`** (RTK becomes the quiet mode — the user's suggestion #4). Full output is filtered; raw output is tee'd under label `mvn_multi` for failure recovery.
2. `filter_mvn_multi` splits output into **segments** on Maven plugin-execution markers: `[INFO] --- <plugin>:<version>:<goal> (<exec>) @ <module> ---`.
3. Each segment is classified by its plugin/goal, **grouped by target sub-filter**, and the matching sub-filter runs **once per group** (segments of a type concatenated — so the test filters' multi-module count accumulation works):

   | Plugin marker | Sub-filter |
   |---|---|
   | `maven-compiler-plugin:*:compile` / `:testCompile` | `filter_mvn_compile` |
   | `maven-surefire-plugin:*:test` | `filter_mvn_tests` (test goal) |
   | `maven-failsafe-plugin:*` | `filter_mvn_tests` (verify goal) |
   | `maven-checkstyle-plugin:*:check` | `filter_mvn_checkstyle` |
   | `maven-clean-plugin:*:clean` | **dropped** (noise in multi-goal) |
   | other (jar, resources, install, …) | **dropped, but `[ERROR]` lines kept** (safety net) |

4. **XML enrichment (run_multi_goal, gated):** if the chain runs tests (`chain_runs_tests(goals)` — any phase from `test` onward, or a `surefire:`/`failsafe:` plugin goal), `run_multi_goal` applies `enrich_with_reports` to the **test/failsafe sub-output only** (not the whole composite) with the captured `started_at`/`app_packages`. Scoping to the test sub-output is required: applying it to the composite would let a checkstyle- or compile-caused `BUILD FAILURE` trigger the "no XML reports found" note, which is misleading when tests never failed. The `goal` argument passed reflects the highest test phase in the chain (`verify`/`integration-test` → `verify`, else `test`) so the recovery hint reads correctly.
5. The trailing **BUILD block is always preserved**: on `BUILD SUCCESS` → only `BUILD SUCCESS` + `Total time` (per-module SUCCESS lines collapsed); on `BUILD FAILURE` → additionally the `Reactor Summary` (which module failed) + all stray `[ERROR]` lines.
6. Output is composed in canonical order — compile → test (enriched) → failsafe (enriched) → checkstyle → BUILD block — under a header `mvn <goals> (multi-goal)`.

### Error handling, exit code, recovery

- **Fallback (mandatory RTK pattern):** if the split finds **no** markers *and* no BUILD line (atypical output), return the **raw output unchanged**. Guarantee: the filter can never swallow everything — the original bug cannot recur even under degradation.
- **Exit code:** `run_multi_goal` runs through `runner::run_filtered`, which returns the child's exit code → `main` calls `process::exit`. Works automatically, like every other goal. This closes complaint ② — both the `BUILD FAILURE` line survives *and* the real exit code propagates.
- **Recovery:** tee under `mvn_multi` → on failure the existing `tee_and_hint` mechanism points to the full raw log.

## Testing

- **Fixtures (real output, anonymized per established practice — `com.devskiller`→`com.example`, file paths→generic):**
  - SUCCESS `clean test-compile checkstyle:check`, single-module — sourced from the real session transcript `rtk proxy` capture.
  - FAILURE variant (compile error or checkstyle violation).
  - Multi-module `clean install` / `clean verify`.
  - `clean verify` with a **failing integration test** + a `failsafe-reports/*.xml` fixture, exercising the multi-goal XML enrichment path (the case that would be invisible without enrichment).
- **Snapshot tests (`insta`)** for each fixture.
- **`parse_goals` unit tests (the core):** `clean test-compile checkstyle:check`→3; `-pl core test`→1 (test); `-rf :mod verify`→1 (verify); `test -Dtest=Foo`→1; `-q clean install`→2; `-version`→0; `dependency:tree`→1.
- **`chain_runs_tests` unit tests:** `[clean, test-compile, checkstyle:check]`→false (no enrichment); `[clean, verify]`→true; `[clean, install]`→true; `[clean, test]`→true; `[clean, compile]`→false.
- **`test_route_goal` updated:** `clean`→Clean, `test`→Test, `verify`→Verify, `dependency:tree`→DepTree; `install`/`package`/`deploy`/`spring-boot:run`→Passthrough.
- **Token savings ≥85%** (consistent with the surefire-XML failure-path precedent; multi-goal input is the full reactor log, so realistically ≫90%).
- **Regression:** all existing single-goal tests stay green (the `run_*` functions are unchanged).

## Accepted risks

- **Marker-format fragility:** the segment split depends on Maven's `--- plugin:version:goal ---` marker format. If Maven changes it, snapshot tests fail loudly and the fix is local. The no-marker fallback (raw passthrough) prevents silent signal loss.
- **Sub-filters consuming partial output:** the existing state-machine filters were written to consume one goal's full output; feeding them a concatenation of just-their-segments is close but not identical. Mitigated by real-fixture snapshot tests covering each sub-filter in multi-goal context.
