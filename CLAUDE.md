# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**rtk (Rust Token Killer)** is a high-performance CLI proxy that minimizes LLM token consumption by filtering and compressing command outputs. It reduces bash output by 60-90% on common development operations through smart filtering, grouping, truncation, and deduplication. All percentages in this repo measure bash output, not your bill. RTK ships no tokenizer (`src/core/tracking.rs` estimates tokens as `bytes / 4`), so the ratios are reliable but the absolute token counts are approximate.

This is a fork with critical fixes for git argument parsing and modern JavaScript stack support (pnpm, vitest, Next.js, TypeScript, Playwright, Prisma).

### Merging Upstream Releases (Non-Negotiable)

**Upstream wins on every overlap.** When resolving a conflict against a
`rtk-ai/rtk` release, take upstream's side. The fork's version survives only
where upstream has no counterpart at all — not because ours is older, better
tested, or measured faster.

**Overlap means a decision the merge forces on you** — a conflict git reports,
or a hunk upstream rewrote where the fork had patched. A fork commit that
merely lives on an upstream file is not an overlap; it becomes one the day
upstream touches the same lines, and git says so at that moment. Scoped that
way the detector is the tool, not a list someone has to remember to open.

Rules:

1. **Superseded means deleted.** When upstream reimplements something the fork
   had patched, drop the fork patch entirely rather than layering it on top.
   v0.49.0 replaced `rtk diff`'s line-pairing with a real Myers diff, so the
   fork's resync-window fix went with it.
2. **A fork guard that breaks an upstream test is a bug, not a conflict.**
   Delete it. Confirm the test passes on the pristine tag first
   (`git worktree add <tmp> vX.Y.Z && cargo test --bin rtk <test>`), so you know
   the failure is ours. In reverse it says nothing: a fork that *deleted* an
   upstream guard is a standing keep, and belongs on the list below.
3. **A fork test asserting the old contract is obsolete.** Update it to the new
   upstream semantics; do not re-pin the old behavior.
4. **Keeping fork behavior over new upstream behavior needs a stated reason,
   and the reason belongs at the site.** Write it as a comment in the hunk that
   holds the deviation, so the next conflict shows the reasoning to whoever
   resolves it — `discover/rules.rs` carries the measured npm/npx/curl numbers
   where those rules used to be; `cmds/js/tsc_cmd.rs` and `core/stream.rs`
   carry theirs the same way. Only a keep with no hunk to speak from goes on
   the standing list below. Report every keep to the user either way.
5. **Adapt, don't fork, when upstream reshapes an API.** Fork-only helpers move
   onto the new interface (`force_tee_display` onto the `[retriever]` dispatch),
   they do not keep a private copy of the old one.
6. **Verify on a clean `HOME`.** Local `exclude_commands` in
   `~/.config/rtk/config.toml` makes ~27 hook tests fail for reasons unrelated
   to the merge:

   ```bash
   env HOME=$(mktemp -d) CARGO_HOME=~/.cargo RUSTUP_HOME=~/.rustup \
       PATH="$HOME/.cargo/bin:$PATH" cargo test --all
   ```

**Standing exceptions** — keeps that no site comment can carry:

- **`src/cmds/jvm/mvn_cmd.rs` replaces upstream's Maven filter outright.** None
  of upstream's `detect_phase` / `filter_surefire` / `filter_compile` /
  `filter_package` / `filter_quiet` / `run` / `run_daemon` survive here, and
  `main.rs` routes `Commands::Mvn` / `Mvnd` to the fork's `mvn_cmd::dispatch`
  instead. This is what the fork is for: 75.7% against upstream's 51.7% on the
  same fixtures. Already re-decided at v0.44.1 and v0.48.0 — decide it the same
  way, and re-measure before ever reversing it.
- **The heredoc bail is deleted on purpose** (`discover/registry.rs`,
  `hooks/hook_cmd.rs`, `hooks/rewrite_cmd.rs`). Upstream returns `None` on
  `has_heredoc`; the fork splits the body off and rewrites what follows the
  terminator, never auto-allowing the result. Restoring that bail — which rule
  2 read backwards would do — hands raw Maven to the agent again (91 calls,
  76k chars, measured 2026-09-11).
- **`rtk mvn … | tail -N` truncation-*drop*** runs ahead of upstream's
  pipeline-producer path, because upstream's variant keeps the stage and it
  would cut rtk's own compact summary.

### Name Collision Warning

**Two different "rtk" projects exist:**
- This project: Rust Token Killer (rtk-ai/rtk)
- reachingforthejack/rtk: Rust Type Kit (DIFFERENT - generates Rust types)

**Verify correct installation:**
```bash
rtk --version  # Should show "rtk 0.28.2" (or newer)
rtk gain       # Should show token savings stats (NOT "command not found")
```

If `rtk gain` fails, you have the wrong package installed.

## Development Commands

> **Note**: If rtk is installed, prefer `rtk <cmd>` over raw commands for token-optimized output.
> All commands work with passthrough support even for subcommands rtk doesn't specifically handle.

### Build & Run
```bash
cargo build                   # raw
rtk cargo build               # preferred (token-optimized)
cargo build --release         # release build (optimized)
cargo run -- <command>        # run directly
cargo install --path .        # install locally
```

### Testing
```bash
cargo test                    # all tests
rtk cargo test                # preferred (token-optimized)
cargo test <test_name>        # specific test
cargo test <module_name>::    # module tests
cargo test -- --nocapture     # with stdout
bash scripts/test-all.sh      # smoke tests (installed binary required)
```

### Linting & Quality
```bash
cargo check                   # check without building
cargo fmt                     # format code
cargo clippy --all-targets    # all clippy lints
rtk cargo clippy --all-targets # preferred
```

### Pre-commit Gate
```bash
cargo fmt --all && cargo clippy --all-targets && cargo test --all
```

### Package Building
```bash
cargo deb                     # DEB package (needs cargo-deb)
cargo generate-rpm            # RPM package (needs cargo-generate-rpm, after release build)
```

## Architecture

rtk uses a **command proxy architecture**: `main.rs` routes CLI commands via a Clap `Commands` enum to specialized filter modules in `src/cmds/*/`, each of which executes the underlying command and compresses its output. Token savings are tracked in SQLite via `src/core/tracking.rs`.

For the full architecture, component details, and module development patterns, see:
- [ARCHITECTURE.md](docs/contributing/ARCHITECTURE.md) — System design, module organization, filtering strategies, error handling
- [docs/contributing/TECHNICAL.md](docs/contributing/TECHNICAL.md) — End-to-end flow, folder map, hook system, filter pipeline

Module responsibilities are documented in each folder's `README.md` and each file's `//!` doc header. Browse `src/cmds/*/` to discover available filters.

Supported ecosystems: git/gh/gt, cargo, go/golangci-lint, npm/pnpm/npx, ruff/pytest/pip/mypy, rspec/rubocop/rake, dotnet, playwright/vitest/jest, docker/kubectl/aws, gradlew/mvn/mvnd/sbt, php/artisan/phpunit/phpstan/pest.

### Proxy Mode

**Purpose**: Execute commands without filtering but track usage for metrics.

**Usage**: `rtk proxy <command> [args...]`

**Benefits**:
- **Bypass RTK filtering**: Workaround bugs or get full unfiltered output
- **Track usage metrics**: Measure which commands Claude uses most (visible in `rtk gain --history`)
- **Guaranteed compatibility**: Always works even if RTK doesn't implement the command

**Examples**:
```bash
rtk proxy git log --oneline -20    # Full git log output (no truncation)
rtk proxy npm install express      # Raw npm output (no filtering)
rtk proxy curl https://api.example.com/data  # Any command works
```

All proxy commands appear in `rtk gain --history` with 0% bash output reduction (input = output).

## Coding Rules

Rust patterns, error handling, and anti-patterns are defined in `.claude/rules/rust-patterns.md` (auto-loaded into context). Key points:

- **anyhow::Result** everywhere, always `.context("description")?`
- **No unwrap()** in production code
- **`LazyLock` statics** for all regex (never compile on every function call)
- **Fallback pattern**: if filter fails, execute raw command unchanged
- **No async**: single-threaded by design (startup <10ms)
- **Exit code propagation**: `std::process::exit(code)` on child failure

Testing strategy and performance targets are defined in `.claude/rules/cli-testing.md` (auto-loaded). Key targets: <10ms startup, <5MB memory, 60-90% reduction in bash output bytes.

For contribution workflow and design philosophy, see [CONTRIBUTING.md](CONTRIBUTING.md). For the step-by-step filter implementation checklist, see [src/cmds/README.md](src/cmds/README.md#adding-a-new-command-filter).

## Build Verification (Mandatory)

**CRITICAL**: After ANY Rust file edits, ALWAYS run the full quality check pipeline before committing:

```bash
cargo fmt --all && cargo clippy --all-targets && cargo test --all
```

**Rules**:
- Never commit code that hasn't passed all 3 checks
- Fix ALL clippy warnings before moving on (zero tolerance)
- If build fails, fix it immediately before continuing to next task

**Performance verification** (for filter changes):
```bash
hyperfine 'rtk git log -10' --warmup 3          # before
cargo build --release
hyperfine 'target/release/rtk git log -10' --warmup 3  # after (should be <10ms)
```

## Working Directory Confirmation

**ALWAYS confirm working directory before starting any work**:

```bash
pwd  # Verify you're in the rtk project root
git branch  # Verify correct branch (main, feature/*, etc.)
```

**Never assume** which project to work in. Always verify before file operations.

## Avoiding Rabbit Holes

**Stay focused on the task**. Do not make excessive operations to verify external APIs, documentation, or edge cases unless explicitly asked.

**Rule**: If verification requires more than 3-4 exploratory commands, STOP and ask the user whether to continue or trust available info.

**Examples of rabbit holes to avoid**:
- Excessive regex pattern testing (trust snapshot tests, don't manually verify 20 edge cases)
- Deep diving into external command documentation (use fixtures, don't research git/cargo internals)
- Over-testing cross-platform behavior (test macOS + Linux, trust CI for Windows)
- Verifying API signatures across multiple crate versions (use docs.rs if needed, don't clone repos)

**When to stop and ask**:
- "Should I research X external API behavior?" → ASK if it requires >3 commands
- "Should I test Y edge case?" → ASK if not mentioned in requirements
- "Should I verify Z across N platforms?" → ASK if N > 2

## Plan Execution Protocol

When user provides a numbered plan (QW1-QW4, Phase 1-5, sprint tasks, etc.):

1. **Execute sequentially**: Follow plan order unless explicitly told otherwise
2. **Commit after each logical step**: One commit per completed phase/task
3. **Never skip or reorder**: If a step is blocked, report it and ask before proceeding
4. **Track progress**: Use task list (TaskCreate/TaskUpdate) for plans with 3+ steps
5. **Validate assumptions**: Before starting, verify all referenced file paths exist and working directory is correct
