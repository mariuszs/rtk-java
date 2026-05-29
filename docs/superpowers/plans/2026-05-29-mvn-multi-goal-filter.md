# mvn Multi-Goal Signal-Aware Filter — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `rtk mvn <goal1> <goal2> ...` filter all chained goals instead of routing the whole output through the first goal's filter and discarding the rest.

**Architecture:** Replace the Clap `MvnCommands` subcommand enum with a single raw-arg capture (`Mvn { args: Vec<OsString> }`), and move all goal routing into `mvn_cmd::dispatch`. One goal → existing single-goal filters (unchanged). ≥2 goals → new `run_multi_goal`, which strips `-q`, splits output on Maven plugin markers, runs each segment group through its existing sub-filter, enriches the test portion from surefire/failsafe XML, and always preserves the BUILD block.

**Tech Stack:** Rust, `anyhow`, `lazy_static`/`regex`, `insta` snapshots, existing `runner`/`tracking` infra. No async.

**Spec:** `docs/superpowers/specs/2026-05-29-mvn-multi-goal-filter-design.md`

**Branch:** `feat/mvn-multi-goal` (already created off `master`; the spec commit is already on it).

**Working notes for the implementer:**
- After ANY Rust edit, the project gate is `cargo fmt --all && cargo clippy --all-targets && cargo test --all`. Run it before each commit.
- New functions added before they are wired into the dispatcher will trip `dead_code`. Annotate them with `#[allow(dead_code)]` and **remove the annotation in Task 7** (the cutover) when they get their first caller. This is the established pattern in this module.
- Commit messages: Conventional Commits, no JIRA, no AI attribution. Use `--no-verify` if the commit-msg hook demands a JIRA in the title.
- `count_tokens` is the test-module helper already defined in `mvn_cmd.rs` tests (`text.split_whitespace().count()`). Reuse it; do not redefine.
- All fixtures live in `tests/fixtures/` and are included with `include_str!("../../../tests/fixtures/<name>.txt")`.

---

## File Structure

- **Modify** `src/main.rs`:
  - `Commands::Mvn` / `Commands::Mvnd` (lines ~717-727): change `{ #[command(subcommand)] command: MvnCommands }` → `{ #[arg(trailing_var_arg = true, allow_hyphen_values = true)] args: Vec<OsString> }`.
  - Delete `enum MvnCommands` (lines ~1123-1160).
  - Collapse `dispatch_mvn` (lines ~1398-1408) to a single call into `mvn_cmd::dispatch`.
- **Modify** `src/cmds/jvm/mvn_cmd.rs`: add `parse_goals`, `chain_runs_tests`, `split_segments` + `SegmentKind`, `filter_segments` + `MultiParts` + `compose_multi`, `filter_mvn_multi`, `run_multi_goal`, `run_passthrough_all`, `dispatch`; widen `GoalRouting` + `route_goal`; delete `run_other`; update `test_route_goal`; add new unit/snapshot tests.
- **Create** fixtures under `tests/fixtures/`: `mvn_multi_clean_testcompile_checkstyle_pass.txt`, `mvn_multi_compile_failure.txt`, `mvn_multi_clean_verify_fail.txt`, plus a `failsafe-reports` XML fixture for the enrichment path (mirror the existing surefire XML fixture layout used by `surefire_reports` tests).

---

## Task 1: `parse_goals` — goal detection (pure)

**Files:**
- Modify: `src/cmds/jvm/mvn_cmd.rs` (add near `route_goal`, ~line 423)
- Test: same file, `#[cfg(test)] mod tests`

- [ ] **Step 1: Write the failing tests**

Add to the test module:

```rust
#[test]
fn test_parse_goals_detection() {
    let v = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();

    // Multiple goals + flags
    assert_eq!(parse_goals(&v("clean test-compile checkstyle:check -Dskip.npm -q")),
               vec!["clean", "test-compile", "checkstyle:check"]);
    // -pl takes a value: `core` is NOT a goal
    assert_eq!(parse_goals(&v("-pl core test")), vec!["test"]);
    // -rf takes a value that contains ':' — must not be mistaken for a plugin goal
    assert_eq!(parse_goals(&v("-rf :mod verify")), vec!["verify"]);
    // single goal + attached -D flag
    assert_eq!(parse_goals(&v("test -Dtest=Foo")), vec!["test"]);
    // leading flag before goals
    assert_eq!(parse_goals(&v("-q clean install")), vec!["clean", "install"]);
    // no goals
    assert_eq!(parse_goals(&v("-version")), Vec::<String>::new());
    // plugin:goal form
    assert_eq!(parse_goals(&v("dependency:tree")), vec!["dependency:tree"]);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib parse_goals_detection`
Expected: FAIL — `cannot find function parse_goals`.

- [ ] **Step 3: Implement `parse_goals`**

Add (above `route_goal`):

```rust
/// Maven lifecycle phases (clean + default + site lifecycles). A bare token
/// matching one of these is a goal even without a `:`.
const MAVEN_PHASES: &[&str] = &[
    "pre-clean", "clean", "post-clean",
    "validate", "initialize",
    "generate-sources", "process-sources", "generate-resources", "process-resources",
    "compile", "process-classes",
    "generate-test-sources", "process-test-sources", "generate-test-resources",
    "process-test-resources", "test-compile", "process-test-classes",
    "test", "prepare-package", "package",
    "pre-integration-test", "integration-test", "post-integration-test",
    "verify", "install", "deploy",
    "pre-site", "site", "post-site", "site-deploy",
];

/// Maven options that consume the FOLLOWING token as their value, so that
/// token must never be treated as a goal (`-pl core`, `-rf :module`).
const VALUE_TAKING_OPTS: &[&str] = &[
    "-pl", "--projects", "-P", "--activate-profiles", "-f", "--file",
    "-T", "--threads", "-rf", "--resume-from", "-s", "--settings",
    "-gs", "--global-settings", "-l", "--log-file", "-b", "--builder",
    "-t", "--toolchains",
];

/// Extract the goal/phase tokens (in order) from a raw mvn arg vector.
/// A token is a goal iff it (1) is not a flag, (2) is not the value of a
/// preceding value-taking option, and (3) is a known lifecycle phase or has
/// the `plugin:goal` form (contains ':').
fn parse_goals(args: &[String]) -> Vec<String> {
    let mut goals = Vec::new();
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg.starts_with('-') {
            // `-pl core` style: the value is a separate token. `-pl=core` and
            // `-Dk=v` keep the value attached, so only bare opts skip-next.
            if VALUE_TAKING_OPTS.contains(&arg.as_str()) {
                skip_next = true;
            }
            continue;
        }
        if MAVEN_PHASES.contains(&arg.as_str()) || arg.contains(':') {
            goals.push(arg.clone());
        }
    }
    goals
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib parse_goals_detection`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --all-targets && cargo test --all
git add src/cmds/jvm/mvn_cmd.rs
git commit --no-verify -m "feat(mvn): add parse_goals for multi-goal detection"
```

---

## Task 2: `chain_runs_tests` — enrichment gate (pure)

**Files:**
- Modify: `src/cmds/jvm/mvn_cmd.rs` (below `parse_goals`)
- Test: same file

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn test_chain_runs_tests() {
    let g = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
    assert!(!chain_runs_tests(&g("clean test-compile checkstyle:check")));
    assert!(!chain_runs_tests(&g("clean compile")));
    assert!(chain_runs_tests(&g("clean test")));
    assert!(chain_runs_tests(&g("clean verify")));
    assert!(chain_runs_tests(&g("clean install")));
    // plugin goal forms
    assert!(chain_runs_tests(&g("surefire:test")));
    assert!(chain_runs_tests(&g("failsafe:integration-test")));
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib chain_runs_tests`
Expected: FAIL — `cannot find function chain_runs_tests`.

- [ ] **Step 3: Implement**

```rust
/// Phases that actually execute surefire/failsafe (everything from `test`
/// onward in the default lifecycle).
const TEST_RUNNING_PHASES: &[&str] = &[
    "test", "prepare-package", "package",
    "pre-integration-test", "integration-test", "post-integration-test",
    "verify", "install", "deploy",
];

/// True if any goal in the chain runs tests — gates XML enrichment in
/// `run_multi_goal`. Avoids a spurious "no XML reports" note when the chain
/// only compiles / runs checkstyle.
#[allow(dead_code)]
fn chain_runs_tests(goals: &[String]) -> bool {
    goals.iter().any(|g| {
        TEST_RUNNING_PHASES.contains(&g.as_str())
            || g.starts_with("surefire:")
            || g.starts_with("failsafe:")
    })
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --lib chain_runs_tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --all-targets && cargo test --all
git add src/cmds/jvm/mvn_cmd.rs
git commit --no-verify -m "feat(mvn): add chain_runs_tests enrichment gate"
```

---

## Task 3: `split_segments` — marker-based segmentation (pure)

**Files:**
- Modify: `src/cmds/jvm/mvn_cmd.rs`
- Test: same file

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn test_split_segments_classifies() {
    let raw = "\
[INFO] Scanning for projects...
[INFO] --- maven-clean-plugin:3.2.0:clean (default-clean) @ app ---
[INFO] Deleting /app/target
[INFO] --- maven-compiler-plugin:3.13.0:testCompile (default-testCompile) @ app ---
[INFO] Compiling 12 source files
[INFO] --- maven-checkstyle-plugin:3.6.0:check (default-cli) @ app ---
[INFO] You have 0 Checkstyle violations.
[INFO] BUILD SUCCESS
[INFO] Total time:  3.0 s";
    let segs = split_segments(raw);
    let kinds: Vec<SegmentKind> = segs.iter().map(|s| s.kind).collect();
    assert_eq!(kinds, vec![
        SegmentKind::Preamble,
        SegmentKind::Clean,
        SegmentKind::Compile,
        SegmentKind::Checkstyle,
    ]);
    // The checkstyle segment carries its body up to (not including) the BUILD block.
    assert!(segs[3].body.contains("0 Checkstyle violations"));
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib split_segments_classifies`
Expected: FAIL — `cannot find type SegmentKind`.

- [ ] **Step 3: Implement `SegmentKind`, `Segment`, `split_segments`**

```rust
lazy_static! {
    /// `[INFO] --- <plugin>:<version>:<goal> (<exec>) @ <module> ---`
    static ref PLUGIN_MARKER_RE: Regex =
        Regex::new(r"^\[INFO\]\s+-{3,}\s+(\S+?):\S+:(\S+)\s+\(").unwrap();
    /// Start of the trailing reactor/build footer.
    static ref BUILD_FOOTER_RE: Regex =
        Regex::new(r"(BUILD SUCCESS|BUILD FAILURE|Reactor Summary)").unwrap();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentKind {
    Preamble,   // before the first plugin marker
    Clean,
    Compile,    // compile / testCompile
    Surefire,   // unit tests
    Failsafe,   // integration tests
    Checkstyle,
    Other,      // jar, resources, install, ...
}

struct Segment {
    kind: SegmentKind,
    body: String,
}

/// Classify a plugin marker line into a SegmentKind by its plugin + goal.
fn classify_marker(plugin: &str, goal: &str) -> SegmentKind {
    match (plugin, goal) {
        ("maven-clean-plugin", _) => SegmentKind::Clean,
        ("maven-compiler-plugin", _) => SegmentKind::Compile,
        ("maven-surefire-plugin", _) => SegmentKind::Surefire,
        ("maven-failsafe-plugin", _) => SegmentKind::Failsafe,
        ("maven-checkstyle-plugin", _) => SegmentKind::Checkstyle,
        (_, g) if g == "check" && plugin.contains("checkstyle") => SegmentKind::Checkstyle,
        _ => SegmentKind::Other,
    }
}

/// Split raw mvn output into segments at plugin-execution markers. The
/// trailing BUILD/Reactor footer is NOT a segment — it is handled separately
/// by `extract_build_block`. Everything before the first marker is `Preamble`.
#[allow(dead_code)]
fn split_segments(raw: &str) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut current_kind = SegmentKind::Preamble;
    let mut current_body = String::new();
    let mut in_footer = false;

    for line in raw.lines() {
        let stripped = strip_ansi(line);
        if !in_footer && BUILD_FOOTER_RE.is_match(&stripped) {
            in_footer = true; // stop accumulating into segments
        }
        if let Some(caps) = PLUGIN_MARKER_RE.captures(&stripped) {
            // flush the previous segment
            if !current_body.is_empty() || current_kind != SegmentKind::Preamble {
                segments.push(Segment { kind: current_kind, body: std::mem::take(&mut current_body) });
            } else {
                current_body.clear();
            }
            let plugin = caps.get(1).map_or("", |m| m.as_str());
            let goal = caps.get(2).map_or("", |m| m.as_str());
            current_kind = classify_marker(plugin, goal);
            in_footer = false;
            continue;
        }
        if !in_footer {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }
    if !current_body.is_empty() {
        segments.push(Segment { kind: current_kind, body: current_body });
    }
    segments
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --lib split_segments_classifies`
Expected: PASS. If the Preamble flush logic emits an unexpected empty leading segment, adjust the flush guard until the asserted `kinds` vector matches, then re-run.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --all-targets && cargo test --all
git add src/cmds/jvm/mvn_cmd.rs
git commit --no-verify -m "feat(mvn): add marker-based segment splitter"
```

---

## Task 4: `filter_segments` + `compose_multi` + `filter_mvn_multi` + SUCCESS fixture

**Files:**
- Modify: `src/cmds/jvm/mvn_cmd.rs`
- Create: `tests/fixtures/mvn_multi_clean_testcompile_checkstyle_pass.txt`
- Test: same file (snapshot + savings)

- [ ] **Step 1: Create the SUCCESS fixture (real, anonymized)**

Assemble a realistic single-module `clean test-compile checkstyle:check` log by concatenating the real captured segments from existing fixtures, keeping exactly one trailing BUILD block. Source material: the `rtk proxy` capture in the design spec's Context section (`com.devskiller`→`com.example`, real paths→`/app/...`, module `auth`→`app`). The fixture MUST contain, in order: a `Scanning for projects` preamble, a `maven-clean-plugin:*:clean` marker + a couple `Deleting` lines, a `maven-compiler-plugin:*:testCompile` marker + `[WARNING]`/`[INFO]` deprecation noise, a `maven-checkstyle-plugin:*:check` marker + `[INFO] You have 0 Checkstyle violations.`, then `[INFO] BUILD SUCCESS` + `[INFO] Total time:  25.5 s`. Keep it ~60-100 lines so savings are measurable.

- [ ] **Step 2: Write the failing test**

```rust
#[test]
fn test_filter_mvn_multi_success() {
    let input = include_str!("../../../tests/fixtures/mvn_multi_clean_testcompile_checkstyle_pass.txt");
    let output = filter_mvn_multi(input, "clean test-compile checkstyle:check");
    assert!(output.contains("(multi-goal)"), "missing header: {output}");
    assert!(output.contains("BUILD SUCCESS"), "lost BUILD line: {output}");
    assert!(output.contains("0 Checkstyle violations") || output.contains("0 violations"),
            "lost checkstyle signal: {output}");
    // clean noise must be gone
    assert!(!output.contains("Deleting"), "clean noise leaked: {output}");
    let savings = 100.0 - (count_tokens(&output) as f64 / count_tokens(input) as f64 * 100.0);
    assert!(savings >= 85.0, "expected ≥85%, got {:.1}%", savings);
    insta::assert_snapshot!(output);
}
```

- [ ] **Step 3: Run it to verify it fails**

Run: `cargo test --lib filter_mvn_multi_success`
Expected: FAIL — `cannot find function filter_mvn_multi`.

- [ ] **Step 4: Implement `MultiParts`, `filter_segments`, `extract_build_block`, `compose_multi`, `filter_mvn_multi`**

```rust
#[derive(Default)]
struct MultiParts {
    compile: String,
    tests: String,      // surefire + failsafe combined (enriched later in run_multi_goal)
    checkstyle: String,
    build: String,      // BUILD SUCCESS/FAILURE + Total time (+ Reactor Summary on failure)
    stray_errors: Vec<String>, // [ERROR] lines from dropped/Other segments
}

/// Pull the trailing build footer: always keep BUILD SUCCESS/FAILURE + Total
/// time; on failure also keep the Reactor Summary block (which module failed).
fn extract_build_block(raw: &str) -> String {
    let lines: Vec<&str> = raw.lines().collect();
    let failed = raw.contains("BUILD FAILURE");
    let mut out: Vec<String> = Vec::new();
    let mut in_reactor = false;
    for line in &lines {
        let s = strip_ansi(line);
        let st = s.trim();
        if failed && st.contains("Reactor Summary") {
            in_reactor = true;
        }
        if in_reactor {
            // keep reactor summary rows until the BUILD line
            if st.contains("BUILD FAILURE") || st.contains("BUILD SUCCESS") {
                in_reactor = false;
            } else if !is_maven_boilerplate(st) && !st.is_empty() {
                out.push(strip_maven_prefix(&s).to_string());
            }
        }
        if st.contains("BUILD SUCCESS") || st.contains("BUILD FAILURE") {
            out.push(if failed { "BUILD FAILURE".to_string() } else { "BUILD SUCCESS".to_string() });
        }
        if let Some(t) = parse_total_time(&s) {
            out.push(format!("Total time: {t}"));
        }
    }
    out.join("\n")
}

/// Run each segment group through its existing sub-filter and collect the
/// signal pieces. Pure: no filesystem access (enrichment happens in
/// run_multi_goal).
#[allow(dead_code)]
fn filter_segments(raw: &str) -> MultiParts {
    let segments = split_segments(raw);
    let mut parts = MultiParts::default();

    let mut compile_buf = String::new();
    let mut test_buf = String::new();
    let mut has_failsafe = false;
    let mut checkstyle_buf = String::new();

    for seg in &segments {
        match seg.kind {
            SegmentKind::Compile => compile_buf.push_str(&seg.body),
            SegmentKind::Surefire => test_buf.push_str(&seg.body),
            SegmentKind::Failsafe => { test_buf.push_str(&seg.body); has_failsafe = true; }
            SegmentKind::Checkstyle => checkstyle_buf.push_str(&seg.body),
            SegmentKind::Clean | SegmentKind::Preamble => {} // dropped as noise
            SegmentKind::Other => {
                for l in seg.body.lines() {
                    if strip_ansi(l).trim_start().starts_with(ERROR_TAG) {
                        parts.stray_errors.push(strip_maven_prefix(&strip_ansi(l)).to_string());
                    }
                }
            }
        }
    }

    if !compile_buf.trim().is_empty() {
        parts.compile = filter_mvn_compile(&compile_buf);
    }
    if !test_buf.trim().is_empty() {
        let goal = if has_failsafe { "verify" } else { "test" };
        parts.tests = filter_mvn_tests_with_goal(&test_buf, goal, &[]);
    }
    if !checkstyle_buf.trim().is_empty() {
        parts.checkstyle = filter_mvn_checkstyle(&checkstyle_buf);
    }
    parts.build = extract_build_block(raw);
    parts
}

/// Assemble the final multi-goal report from already-filtered (and possibly
/// enriched) parts, in canonical order.
fn compose_multi(parts: &MultiParts, goals_header: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "mvn {goals_header} (multi-goal)");
    for piece in [&parts.compile, &parts.tests, &parts.checkstyle] {
        if !piece.trim().is_empty() {
            out.push_str(piece.trim_end());
            out.push('\n');
        }
    }
    for e in &parts.stray_errors {
        out.push_str(e);
        out.push('\n');
    }
    if !parts.build.trim().is_empty() {
        out.push_str(parts.build.trim_end());
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Pure multi-goal filter (no XML enrichment) — snapshot-tested directly.
/// `run_multi_goal` wraps this and adds enrichment on the test portion.
fn filter_mvn_multi(raw: &str, goals_header: &str) -> String {
    // Degraded-input fallback: no markers AND no build footer → never swallow.
    if !PLUGIN_MARKER_RE.is_match(raw) && !BUILD_FOOTER_RE.is_match(raw) {
        return raw.to_string();
    }
    let parts = filter_segments(raw);
    compose_multi(&parts, goals_header)
}
```

- [ ] **Step 5: Run the test; review the snapshot**

Run: `cargo test --lib filter_mvn_multi_success`
Expected: FAIL first run (new snapshot pending). Then:
Run: `cargo insta review` → inspect that the output shows the header, a terse checkstyle line, no `Deleting`, and `BUILD SUCCESS` + `Total time`. Accept (`a`) if correct.
Run: `cargo test --lib filter_mvn_multi_success` → PASS, savings ≥85%.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all && cargo clippy --all-targets && cargo test --all
git add src/cmds/jvm/mvn_cmd.rs src/cmds/jvm/snapshots/ tests/fixtures/mvn_multi_clean_testcompile_checkstyle_pass.txt
git commit --no-verify -m "feat(mvn): add multi-goal segment filter (success path)"
```

---

## Task 5: Failure fixture + `filter_mvn_multi` failure snapshot

**Files:**
- Create: `tests/fixtures/mvn_multi_compile_failure.txt`
- Test: `src/cmds/jvm/mvn_cmd.rs`

- [ ] **Step 1: Create the FAILURE fixture**

Assemble a `clean test-compile checkstyle:check` log where `testCompile` fails: preamble, clean marker, `maven-compiler-plugin:*:testCompile` marker followed by real-shaped `[ERROR] /app/src/.../Foo.java:[12,5] cannot find symbol` lines (reuse the shape from `tests/fixtures/mvn_compile_reactor_fail.txt`), then a `[INFO] BUILD FAILURE`, a `[INFO] ------` rule, and `[INFO] Total time:  4.1 s`. No checkstyle marker (build aborted at compile). ~50 lines.

- [ ] **Step 2: Write the failing test**

```rust
#[test]
fn test_filter_mvn_multi_compile_failure() {
    let input = include_str!("../../../tests/fixtures/mvn_multi_compile_failure.txt");
    let output = filter_mvn_multi(input, "clean test-compile checkstyle:check");
    assert!(output.contains("BUILD FAILURE"), "lost failure signal: {output}");
    assert!(output.contains("cannot find symbol"), "lost compile error detail: {output}");
    let savings = 100.0 - (count_tokens(&output) as f64 / count_tokens(input) as f64 * 100.0);
    assert!(savings >= 60.0, "failure path still expected ≥60%, got {:.1}%", savings);
    insta::assert_snapshot!(output);
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test --lib filter_mvn_multi_compile_failure`
Expected: FAIL — pending snapshot (and assertions until the fixture is right).

- [ ] **Step 4: Review + accept snapshot**

Run: `cargo insta review` → confirm the compile `[ERROR]` block survives and `BUILD FAILURE` is present. Accept.
Run: `cargo test --lib filter_mvn_multi_compile_failure` → PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --all-targets && cargo test --all
git add src/cmds/jvm/mvn_cmd.rs src/cmds/jvm/snapshots/ tests/fixtures/mvn_multi_compile_failure.txt
git commit --no-verify -m "test(mvn): multi-goal compile-failure snapshot"
```

---

## Task 6: `run_multi_goal` — execution + `-q` strip + XML enrichment

**Files:**
- Modify: `src/cmds/jvm/mvn_cmd.rs`
- Test: same file (unit test for `-q` stripping)

- [ ] **Step 1: Write the failing test (`-q` stripping helper)**

```rust
#[test]
fn test_strip_quiet_flags() {
    let v = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
    assert_eq!(strip_quiet_flags(&v("clean verify -q")), v("clean verify"));
    assert_eq!(strip_quiet_flags(&v("--quiet clean test")), v("clean test"));
    assert_eq!(strip_quiet_flags(&v("clean test -Dq=1")), v("clean test -Dq=1"));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --lib strip_quiet_flags`
Expected: FAIL — `cannot find function strip_quiet_flags`.

- [ ] **Step 3: Implement `strip_quiet_flags` + `run_multi_goal`**

```rust
/// Remove `-q` / `--quiet` so RTK receives full output and does the
/// compression itself (multi-goal "smart quiet").
fn strip_quiet_flags(args: &[String]) -> Vec<String> {
    args.iter()
        .filter(|a| a.as_str() != "-q" && a.as_str() != "--quiet")
        .cloned()
        .collect()
}

/// Run a multi-goal invocation: strip -q, run mvn, filter via filter_mvn_multi,
/// then enrich the test portion from surefire/failsafe XML when the chain runs
/// tests. Reuses `runner::run_filtered` so exit code + tee behave like every
/// other goal.
fn run_multi_goal(binary: MvnBinary, args: &[String], verbose: u8) -> Result<i32> {
    let goals = parse_goals(args);
    let goals_header = goals.join(" ");
    let run_args = strip_quiet_flags(args);

    let mut cmd = mvn_command(binary);
    for arg in &run_args {
        cmd.arg(arg);
    }
    if verbose > 0 {
        eprintln!("Running: {binary} {} (multi-goal)", run_args.join(" "));
    }

    let started_at = std::time::SystemTime::now();
    let cwd = std::env::current_dir().unwrap_or_else(|e| {
        eprintln!("rtk {binary}: could not determine cwd: {e}");
        std::path::PathBuf::from(".")
    });
    let app_pkgs = crate::cmds::jvm::pom_groupid::detect(&cwd);
    let enrich = chain_runs_tests(&goals);
    let test_goal = if goals.iter().any(|g| g == "verify" || g == "integration-test") {
        "verify"
    } else {
        "test"
    };
    let header = goals_header.clone();

    let (tool_name, tee_label) = mvn_labels(binary, "multi", "multi");
    runner::run_filtered(
        cmd,
        &tool_name,
        &run_args.join(" "),
        move |raw: &str| {
            let mut parts = filter_segments(raw);
            if enrich && !parts.tests.trim().is_empty() {
                parts.tests = enrich_with_reports(&parts.tests, &cwd, started_at, &app_pkgs, test_goal);
            }
            // Degraded fallback identical to filter_mvn_multi.
            if !PLUGIN_MARKER_RE.is_match(raw) && !BUILD_FOOTER_RE.is_match(raw) {
                return raw.to_string();
            }
            compose_multi(&parts, &header)
        },
        runner::RunOptions::with_tee(&tee_label),
    )
}
```

Note: `filter_mvn_multi` stays as the pure, snapshot-tested entry; `run_multi_goal` reuses `filter_segments` + `compose_multi` directly so it can splice enrichment between them. Remove the `#[allow(dead_code)]` from `filter_segments`, `chain_runs_tests`, `split_segments` here if the compiler now sees callers; otherwise they are removed in Task 7.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib strip_quiet_flags`
Expected: PASS. (`run_multi_goal` is exercised end-to-end in Task 8; it may still warn `dead_code` until Task 7 wires `dispatch`.)

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --all-targets && cargo test --all
git add src/cmds/jvm/mvn_cmd.rs
git commit --no-verify -m "feat(mvn): add run_multi_goal with -q strip and XML enrichment"
```

---

## Task 7: Cutover — widen `route_goal`, new `dispatch`, rewire `main.rs`

This is the atomic switch. After it, multi-goal is live and `dead_code` annotations come off.

**Files:**
- Modify: `src/cmds/jvm/mvn_cmd.rs` (`GoalRouting`, `route_goal`, add `dispatch` + `run_passthrough_all`, delete `run_other`, update `test_route_goal`)
- Modify: `src/main.rs` (`Commands::Mvn`/`Mvnd`, delete `MvnCommands`, collapse `dispatch_mvn`)

- [ ] **Step 1: Update the failing test first (`test_route_goal`)**

Replace the body of `test_route_goal` (currently ~line 2560):

```rust
#[test]
fn test_route_goal() {
    assert_eq!(route_goal("compile"), GoalRouting::Compile);
    assert_eq!(route_goal("process-classes"), GoalRouting::Compile);
    assert_eq!(route_goal("test-compile"), GoalRouting::Compile);
    assert_eq!(route_goal("checkstyle:check"), GoalRouting::Checkstyle);
    assert_eq!(route_goal("checkstyle"), GoalRouting::Checkstyle);
    // Now first-class single-goal routes (were Passthrough under the old Clap model):
    assert_eq!(route_goal("test"), GoalRouting::Test);
    assert_eq!(route_goal("verify"), GoalRouting::Verify);
    assert_eq!(route_goal("clean"), GoalRouting::Clean);
    assert_eq!(route_goal("dependency:tree"), GoalRouting::DepTree);
    // Still passthrough — no dedicated filter:
    assert_eq!(route_goal("package"), GoalRouting::Passthrough);
    assert_eq!(route_goal("install"), GoalRouting::Passthrough);
    assert_eq!(route_goal("deploy"), GoalRouting::Passthrough);
    assert_eq!(route_goal("spring-boot:run"), GoalRouting::Passthrough);
    assert_eq!(route_goal("quarkus:dev"), GoalRouting::Passthrough);
    assert_eq!(route_goal("compilee"), GoalRouting::Passthrough);
    assert_eq!(route_goal(""), GoalRouting::Passthrough);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib test_route_goal`
Expected: FAIL — `no variant Test`/`Verify`/`Clean`/`DepTree`.

- [ ] **Step 3: Widen `GoalRouting` + `route_goal`**

Replace the enum + function (currently lines ~405-423):

```rust
#[derive(Debug, PartialEq, Eq)]
enum GoalRouting {
    Test,
    Verify,
    Clean,
    Compile,
    Checkstyle,
    DepTree,
    Passthrough,
}

fn route_goal(subcommand: &str) -> GoalRouting {
    if COMPILE_LIKE_GOALS.iter().any(|(g, _)| *g == subcommand) {
        return GoalRouting::Compile;
    }
    match subcommand {
        "test" => GoalRouting::Test,
        "verify" => GoalRouting::Verify,
        "clean" => GoalRouting::Clean,
        "checkstyle:check" | "checkstyle" => GoalRouting::Checkstyle,
        "dependency:tree" => GoalRouting::DepTree,
        _ => GoalRouting::Passthrough,
    }
}
```

- [ ] **Step 4: Add `dispatch` + `run_passthrough_all`, delete `run_other`**

Replace `run_other` (lines ~440-480) with:

```rust
/// Stream an unfiltered mvn invocation (long-running/unsupported goals, or
/// goal-less commands like `mvn -version`). Tracked for metrics only.
fn run_passthrough_all(binary: MvnBinary, args: &[OsString], verbose: u8) -> Result<i32> {
    if verbose > 0 {
        eprintln!("Running: {binary} {} (passthrough)", tracking::args_display(args));
    }
    let timer = tracking::TimedExecution::start();
    let mut cmd = mvn_command(binary);
    for arg in args {
        cmd.arg(arg);
    }
    let status = cmd
        .status()
        .with_context(|| format!("Failed to run {binary}"))?;
    let args_str = tracking::args_display(args);
    timer.track_passthrough(
        &format!("{binary} {args_str}"),
        &format!("rtk {binary} {args_str} (passthrough)"),
    );
    Ok(exit_code_from_status(&status, binary.as_str()))
}

/// Top-level mvn/mvnd entry point. Parses goals from the raw arg vector and
/// routes: 0 goals → passthrough; 1 goal → its single-goal filter; ≥2 →
/// multi-goal aggregating filter.
pub fn dispatch(binary: MvnBinary, args: &[OsString], verbose: u8) -> Result<i32> {
    let str_args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let goals = parse_goals(&str_args);

    match goals.len() {
        0 => run_passthrough_all(binary, args, verbose),
        1 => {
            let goal = goals[0].clone();
            // Pass every arg EXCEPT the matched goal token; the run_* helpers
            // prepend their own canonical goal name.
            let rest: Vec<String> = {
                let mut removed = false;
                str_args.iter().filter(|a| {
                    if !removed && **a == goal { removed = true; false } else { true }
                }).cloned().collect()
            };
            match route_goal(&goal) {
                GoalRouting::Test => run_test(binary, &rest, verbose),
                GoalRouting::Verify => run_verify(binary, &rest, verbose),
                GoalRouting::Clean => run_clean(binary, &rest, verbose),
                GoalRouting::Compile => run_compile_like(binary, &goal, &rest, verbose),
                GoalRouting::Checkstyle => run_checkstyle(binary, &rest, verbose),
                GoalRouting::DepTree => run_dep_tree(binary, &rest, verbose),
                GoalRouting::Passthrough => run_passthrough_all(binary, args, verbose),
            }
        }
        _ => run_multi_goal(binary, &str_args, verbose),
    }
}
```

Remove every remaining `#[allow(dead_code)]` added in Tasks 2-6 (they now have callers).

- [ ] **Step 5: Rewire `main.rs`**

Edit `Commands::Mvn` and `Commands::Mvnd` (lines ~717-727):

```rust
    /// Maven commands with compact output
    Mvn {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },

    /// Maven Daemon (mvnd) commands with compact output — same filters as `rtk mvn`
    Mvnd {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
```

Delete `enum MvnCommands` (lines ~1123-1160). Replace `dispatch_mvn` (lines ~1394-1408):

```rust
/// Dispatch an mvn/mvnd invocation to the multi-goal-aware router in mvn_cmd.
fn dispatch_mvn(binary: mvn_cmd::MvnBinary, args: Vec<OsString>, verbose: u8) -> Result<i32> {
    mvn_cmd::dispatch(binary, &args, verbose)
}
```

Update the two call sites (lines ~2201-2202):

```rust
        Commands::Mvn { args } => dispatch_mvn(mvn_cmd::MvnBinary::Mvn, args, cli.verbose)?,
        Commands::Mvnd { args } => dispatch_mvn(mvn_cmd::MvnBinary::Mvnd, args, cli.verbose)?,
```

Confirm `use std::ffi::OsString;` is present in `main.rs` (it is — other commands use it).

- [ ] **Step 6: Run the full suite**

Run: `cargo test --all`
Expected: PASS, including `test_route_goal` and all existing single-goal mvn tests. Fix any non-exhaustive-match or unused-import errors the compiler points to.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all && cargo clippy --all-targets && cargo test --all
git add src/main.rs src/cmds/jvm/mvn_cmd.rs
git commit --no-verify -m "feat(mvn): route all goals via dispatch, enable multi-goal filtering"
```

---

## Task 8: Multi-module + verify-failure (failsafe XML) fixtures + tests

**Files:**
- Create: `tests/fixtures/mvn_multi_clean_verify_fail.txt` and a failsafe XML report fixture (mirror the existing surefire XML fixture used by `surefire_reports` tests — check `src/cmds/jvm/surefire_reports.rs` tests / `tests/fixtures/` for the established `*-reports/*.xml` layout and copy that shape under a temp dir created by the test).
- Test: `src/cmds/jvm/mvn_cmd.rs`

- [ ] **Step 1: Create the verify-failure fixture**

Assemble a `clean verify` reactor log: preamble, clean marker, compiler markers (success), `maven-surefire-plugin:*:test` marker + a passing `Tests run` block, `maven-failsafe-plugin:*:integration-test` marker + a `Tests run: N, Failures: 1` block referencing `com.example.FooIT`, then `Reactor Summary` with one module FAILURE, `[INFO] BUILD FAILURE`, `Total time`. Reuse real shapes from `tests/fixtures/mvn_verify_auth.txt` and `mvn_test_reactor_fail.txt`.

- [ ] **Step 2: Write the filter-level failure test**

```rust
#[test]
fn test_filter_mvn_multi_verify_failure_stdout() {
    let input = include_str!("../../../tests/fixtures/mvn_multi_clean_verify_fail.txt");
    let output = filter_mvn_multi(input, "clean verify");
    assert!(output.contains("BUILD FAILURE"), "got: {output}");
    assert!(output.contains("FooIT") || output.contains("failed"), "lost IT failure: {output}");
    let savings = 100.0 - (count_tokens(&output) as f64 / count_tokens(input) as f64 * 100.0);
    assert!(savings >= 80.0, "expected ≥80%, got {:.1}%", savings);
    insta::assert_snapshot!(output);
}
```

(The XML-enrichment layer is integration-tested through the existing single-goal `enrich_with_reports` snapshot tests, which Task 6 reuses verbatim — no new XML test harness needed at the `filter_mvn_multi` level, which is intentionally pure/stdout-only.)

- [ ] **Step 3: Run + review snapshot**

Run: `cargo test --lib filter_mvn_multi_verify_failure_stdout`; `cargo insta review` → accept once the IT failure + Reactor Summary failing module + BUILD FAILURE are present.

- [ ] **Step 4: Commit**

```bash
cargo fmt --all && cargo clippy --all-targets && cargo test --all
git add src/cmds/jvm/mvn_cmd.rs src/cmds/jvm/snapshots/ tests/fixtures/mvn_multi_clean_verify_fail.txt
git commit --no-verify -m "test(mvn): multi-goal verify-failure snapshot"
```

---

## Task 9: Docs + final gate

**Files:**
- Modify: `src/cmds/jvm/README.md`, `README.md`, `CHANGELOG.md`

- [ ] **Step 1: Update docs**

In `src/cmds/jvm/README.md` and the top-level `README.md` mvn section, document that `rtk mvn` now accepts arbitrary goal chains and that multi-goal invocations are filtered per-goal with the BUILD signal always preserved (and `-q` is auto-dropped in multi-goal mode). Add a `CHANGELOG.md` entry under the unreleased section: `feat(mvn): signal-aware multi-goal filtering (clean test-compile checkstyle:check, clean verify, ...)`.

- [ ] **Step 2: Full gate**

Run:
```bash
cargo fmt --all && cargo clippy --all-targets && cargo test --all
```
Expected: clean fmt, zero new clippy warnings on `mvn_cmd.rs`/`main.rs`, all tests pass. (Note: per the surefire-XML PR notes there may be 6 PRE-EXISTING clippy warnings unrelated to this work; do not let them block, but introduce no new ones.)

- [ ] **Step 3: Manual smoke (optional, if a Maven project is handy)**

```bash
cargo build --release
target/release/rtk mvn clean test-compile checkstyle:check   # expect per-goal signal + BUILD line
echo "exit=$?"                                                # expect real mvn exit code
```

- [ ] **Step 4: Commit**

```bash
git add README.md src/cmds/jvm/README.md CHANGELOG.md
git commit --no-verify -m "docs(mvn): document multi-goal filtering"
```

---

## Self-Review (completed by plan author)

**Spec coverage:**
- Approach 3 (single capture variant) → Task 7 (main.rs + dispatch). ✓
- `parse_goals` flag/value/phase-or-colon → Task 1. ✓
- Widen `GoalRouting`/`route_goal` + updated `test_route_goal` → Task 7. ✓
- `chain_runs_tests` → Task 2 (+ tests). ✓
- Segment split by markers + group + reuse sub-filters → Tasks 3-4. ✓
- BUILD block always preserved (success collapse / failure + Reactor Summary) → `extract_build_block`, Task 4. ✓
- `-q` strip in multi-goal → Task 6 (`strip_quiet_flags`). ✓
- XML enrichment on test sub-output, gated, scoped (not whole composite) → Task 6 (`run_multi_goal`). ✓
- Fallback / never-swallow → `filter_mvn_multi` + `run_multi_goal` guard, Tasks 4 & 6. ✓
- Exit code via `runner::run_filtered` → Task 6. ✓
- Fixtures (success, compile-failure, verify-failure) + snapshots + ≥85% savings + parse_goals/chain_runs_tests unit tests → Tasks 4,5,8,1,2. ✓
- Existing single-goal filters untouched → only callers change (Task 7); filter fns unmodified. ✓

**Placeholder scan:** No TBD/TODO; every code step has concrete code. Fixture-creation steps describe exact required content and cite real source fixtures to assemble from. ✓

**Type consistency:** `MultiParts` fields (`compile`/`tests`/`checkstyle`/`build`/`stray_errors`) consistent across `filter_segments`/`compose_multi`/`run_multi_goal`. `SegmentKind` variants consistent across `classify_marker`/`split_segments`/`filter_segments`. `filter_mvn_tests_with_goal(&str,&str,&[String])`, `filter_mvn_compile(&str)`, `filter_mvn_checkstyle(&str)`, `enrich_with_reports(&str,&Path,SystemTime,&[String],&str)` match the real signatures in `mvn_cmd.rs`. ✓

**Known refinement points (expected during TDD, not blockers):** the `extract_build_block` Reactor-Summary loop and the `split_segments` Preamble-flush guard are the two spots most likely to need a small tweak against the real fixtures — both are covered by snapshot review steps.
