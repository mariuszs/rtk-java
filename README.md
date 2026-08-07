# rtk-java

**rtk for Java teams — the RTK fork with first-class Maven support**

[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
[![Fork of rtk-ai/rtk](https://img.shields.io/badge/fork%20of-rtk--ai%2Frtk-informational)](https://github.com/rtk-ai/rtk)
![mvn test token savings](https://img.shields.io/badge/mvn%20test-−99%25%20tokens-success)

[Java / Maven](#java--maven) · [Install](#installation) · [Why this fork](#why-this-fork) · [Upstream](#relationship-to-upstream)

---

rtk filters and compresses command outputs before they reach your LLM context.
Single Rust binary, 100+ supported commands, <10ms overhead.

**This fork is the Java/Maven build.** Everything upstream rtk does, plus a Maven
filter that turns a 1,500-line `mvn verify` into a couple of dozen lines — and,
when tests fail, reads the Surefire/Failsafe XML reports to hand your agent the
actual stack trace and the captured logs instead of a summary line.

> Looking for the general-purpose tool? Use [rtk-ai/rtk](https://github.com/rtk-ai/rtk).
> Working in a Maven codebase with Claude Code / Copilot / Cursor? Use this one.

## Why this fork

Java build output is uniquely hostile to an LLM context window: multi-module
reactors, plugin chatter, download progress — and, worst of all, a failing test
whose stack trace is not in stdout at all, only in `target/surefire-reports/*.xml`.
Upstream rtk ships a Maven filter covering `test`, `compile` and `package`. This
fork takes it much further.

| | Upstream `rtk` | `rtk-java` |
|---|---|---|
| Goals filtered | `test`, `compile`, `package`/`install`/`verify`/`deploy` | + `clean`, `checkstyle:check`, `dependency:tree`, `dependency:list`, `integration-test`, `surefire:`/`failsafe:` goals |
| Multi-goal chains (`mvn clean verify`) | filtered as one blob by the first goal | split per plugin boundary, each segment gets its own filter |
| Failure detail | whatever Maven printed to stdout | **Surefire/Failsafe XML enrichment**: real stack traces, captured stdout/stderr, per-suite stats |
| Passing runs | one summary line | per-class breakdown + full digest on disk via tee |
| Maven Daemon | not supported | `rtk mvnd <goal>`, tracked separately in `rtk gain` |
| `mvn -q` | passed through | stripped automatically — rtk gets full output and compresses it itself |
| `spring-boot:run`, `quarkus:dev` | — | streaming passthrough, safe for long-running goals |

Everything else — git, gh, cargo, npm, pytest, docker, kubectl, the hook system,
`rtk gain` analytics — is upstream rtk, kept in sync.

## Token Savings

Per-goal ratios rtk applies for Maven (source: `src/discover/rules.rs`, backed by
savings assertions in `cargo test --all` against real fixtures in `tests/fixtures/mvn_*.txt`):

| Goal | Savings |
|------|---------|
| `mvn test` | -99% |
| `mvn verify` | -95% |
| `mvn clean` | -95% |
| `mvn checkstyle:check` | -90% |
| `mvn compile` / `test-compile` | -85% |
| `mvn dependency:list` | -80% |
| `mvn dependency:tree` | -70% |

The rest of the toolchain keeps upstream's 60-90% range: `git status` -80%,
`git diff` -75%, `grep` -80%, `cat`/`read` -70%, `docker ps` -80%. In a Maven
codebase the build commands dominate, so a working session lands around -85%.

## Installation

```bash
cargo install --git https://github.com/mariuszs/rtk-java
```

The binary is still called `rtk`, so every hook, alias and doc from upstream works
unchanged. If you already have upstream rtk installed, this replaces it.

> Homebrew, `install.sh` and the prebuilt release archives publish **upstream**
> rtk, which does not contain the Java work — install from git.

### Verify Installation

```bash
rtk --version    # rtk 0.43.0 or newer
rtk mvn --help   # Maven filter present?
rtk mvnd --help  # Maven Daemon support present? (fork-only — proves you have this build)
rtk gain         # Token savings stats
```

> **Name collision warning**: Another project named "rtk" (Rust Type Kit) exists on
> crates.io. If `rtk gain` fails, you have the wrong package. Use `cargo install --git` above.

> **Windows**: use [WSL](https://learn.microsoft.com/en-us/windows/wsl/install) — the
> auto-rewrite hook needs a Unix shell. On native Windows the filters work but commands
> are not rewritten automatically. See [Windows setup](#windows) below.

## Quick Start

```bash
# 1. Install for your AI tool
rtk init -g                     # Claude Code / Copilot (default)
rtk init -g --gemini            # Gemini CLI
rtk init -g --codex             # Codex (OpenAI)
rtk init -g --agent cursor      # Cursor
rtk init -g --agent windsurf    # Windsurf
rtk init --agent cline          # Cline / Roo Code
rtk init --agent kilocode       # Kilo Code
rtk init --agent antigravity    # Google Antigravity
rtk init -g --agent pi          # Pi
rtk init --agent hermes         # Hermes

# 2. Restart your AI tool, then test
git status  # Automatically rewritten to rtk git status
```

Hook-based agents rewrite Bash commands (e.g., `git status` -> `rtk git status`) before execution. Plugin-based agents, including Hermes, use their plugin API to rewrite commands before execution. The agent receives compact output without needing to call `rtk` explicitly.

**Important:** the hook only runs on Bash tool calls. Claude Code built-in tools like `Read`, `Grep`, and `Glob` do not pass through the Bash hook, so they are not auto-rewritten. To get RTK's compact output for those workflows, use shell commands (`cat`/`head`/`tail`, `rg`/`grep`, `find`) or call `rtk read`, `rtk grep`, or `rtk find` directly.

## Java / Maven

### Goals

```bash
rtk mvn test                     # state-machine parser (Preamble → Testing → Summary), -97…99%
rtk mvn verify                   # surefire + failsafe merged into one summary
rtk mvn clean                    # "mvn clean: deleted /path/target (1.4 s)"
rtk mvn compile                  # also test-compile, process-classes
rtk mvn checkstyle:check         # path:line:col [Rule] message + violation count
rtk mvn dependency:tree          # duplicates and managed-version annotations dropped
rtk mvn dependency:list
rtk mvn clean verify             # multi-goal chain: per-segment filters
rtk mvn clean test-compile checkstyle:check
rtk mvn spring-boot:run          # unknown / long-running goal → streaming passthrough
rtk mvnd test                    # Maven Daemon, same filters, tracked separately
rtk proxy mvn test               # bypass: full raw output
```

`mvn`, `./mvnw` and `mvnw.cmd` are auto-detected; `rtk mvnd` always calls the daemon
binary. `package`, `install`, `deploy` and `integration-test` run through the test
filter (with XML enrichment) under their own goal name. Zero goals (`mvn -version`,
`mvn --help`) pass through untouched.

### Stack traces from Surefire/Failsafe XML

When a test fails, Maven's stdout gives you a class name and a count. rtk reads
`target/surefire-reports/TEST-*.xml` and `target/failsafe-reports/*.xml` after the
build and appends what the agent actually needs:

- **Full stack trace per failure** — framework frames collapsed, root cause and the
  whole `Caused by:` chain preserved (up to 50 lines per trace).
- **Captured stdout/stderr of the failing tests only** — 2,000 chars per test,
  10,000 total.
- **Report counters** in the footer: `(reports: N surefire, M failsafe, K stale files skipped)`.

Application frames are told apart from framework frames using the `groupId` from your
`pom.xml` (project groupId → parent groupId → no filtering as fallback). Reports older
than the current run are skipped by mtime, so a stale `target/` never leaks into the output.

### Green runs are not silent

A passing run reports a per-class breakdown plus a Maven-native aggregate line, and
writes the full class digest to disk (tee) referenced from the summary — so an agent
can grep the familiar `Tests run: … Failures: … Errors: … Skipped: …` patterns without
re-running the build. Skipped test names and reactor module names are carried through
from the XML.

### "0 tests executed" is treated as a red flag

If Maven reports no tests and there are no Surefire reports to back that up, rtk says
so instead of printing a cheerful summary:

```
mvn test: 0 tests executed — surefire detected no tests.
Check pom.xml (surefire plugin configuration) or run: rtk proxy mvn test
```

### Noise the compile filter removes

Download progress, `[INFO]` scaffolding, JVM/native-access warnings, Reactor Build
Order, jOOQ codegen, Liquibase, npm/React builds nested in the Maven build,
typescript-generator, artifactregistry-maven-wagon and GCP auth chatter,
enforcer/githook/compiler plugin boilerplate, and duplicated `javac` error locations
(each error is reported once). A failing multi-module reactor collapses to
`Reactor: N modules — M SUCCESS, K FAILURE (module, …)`.

### Agent integration details

- `-q` / `--quiet` is stripped from filtered runs so rtk sees the full output and does
  the compression itself.
- Command rewriting handles Maven options before the goal (`mvn -T1C clean verify`),
  transparent prefixes (`timeout`, `nice`), single-quoted `bash -c` wrappers, and drops
  trailing `| tail -n` / `| head -n` pipes the agent adds out of habit.
- A `| grep` stage is left alone on purpose: only the grep is rewritten, never the Maven
  command, because filtering first would change which lines the grep can match. So
  `mvn test | grep …` is faithful to raw Maven — but it bypasses the XML enrichment
  above, and the `Caused by:` chain it recovers is not in stdout to be matched. Run
  Maven plain unless you specifically want raw stdout.
- `rtk discover` knows per-goal savings; `rtk gain` tracks `mvn` and `mvnd` separately.
- Gradle Wrapper (`rtk gradlew`) is inherited from upstream: build / test / connectedTest
  / lint / dependencies.

## How It Works

```
  Without rtk:                                    With rtk:

  Claude  --git status-->  shell  -->  git         Claude  --git status-->  RTK  -->  git
    ^                                   |            ^                      |          |
    |        ~2,000 tokens (raw)        |            |   ~200 tokens        | filter   |
    +-----------------------------------+            +------- (filtered) ---+----------+
```

Four strategies applied per command type:

1. **Smart Filtering** - Removes noise (comments, whitespace, boilerplate)
2. **Grouping** - Aggregates similar items (files by directory, errors by type)
3. **Truncation** - Keeps relevant context, cuts redundancy
4. **Deduplication** - Collapses repeated log lines with counts

## Commands

### Files
```bash
rtk ls .                        # Token-optimized directory tree
rtk read file.rs                # Smart file reading
rtk read file.rs -l aggressive  # Signatures only (strips bodies)
rtk smart file.rs               # 2-line heuristic code summary
rtk find "*.rs" .               # Compact find results
rtk grep "pattern" .            # Grouped search results
rtk diff file1 file2            # Condensed diff (exit 1 if files differ)
```

### Git
```bash
rtk git status                  # Compact status
rtk git log -n 10               # One-line commits
rtk git diff                    # Condensed diff
rtk git add                     # -> "ok"
rtk git commit -m "msg"         # -> "ok abc1234"
rtk git push                    # -> "ok main"
rtk git pull                    # -> "ok 3 files +10 -2"
```

### GitHub CLI
```bash
rtk gh pr list                  # Compact PR listing
rtk gh pr view 42               # PR details + checks
rtk gh issue list               # Compact issue listing
rtk gh run list                 # Workflow run status
```

### Test Runners
```bash
rtk jest                        # Jest compact (failures only)
rtk vitest                      # Vitest compact (failures only)
rtk playwright test             # E2E results (failures only)
rtk pytest                      # Python tests (-90%)
rtk go test                     # Go tests (NDJSON, -90%)
rtk cargo test                  # Cargo tests (-90%)
rtk rake test                   # Ruby minitest (-90%)
rtk rspec                       # RSpec tests (JSON, -60%+)
rtk mvn test                    # Maven tests (-99%) — see [Java / Maven](#java--maven)
rtk mvn verify                  # Maven verify — surefire + failsafe XML enrichment
rtk mvnd test                   # Maven Daemon tests (same filter, same savings)
rtk err <cmd>                   # Filter errors only from any command
rtk test <cmd>                  # Generic test wrapper - failures only (-90%)
```

### Build & Lint
```bash
rtk lint                        # ESLint grouped by rule/file
rtk lint biome                  # Supports other linters
rtk tsc                         # TypeScript errors grouped by file
rtk next build                  # Next.js build compact
rtk prettier --check .          # Files needing formatting
rtk cargo build                 # Cargo build (-80%)
rtk cargo clippy                # Cargo clippy (-80%)
rtk ruff check                  # Python linting (JSON, -80%)
rtk golangci-lint run           # Go linting (JSON, -85%)
rtk rubocop                     # Ruby linting (JSON, -60%+)
rtk mvn compile                 # Maven compile (-85%)
rtk mvn checkstyle:check        # Checkstyle violations (-90%)
rtk mvn dependency:tree         # Maven dependency tree (-70%)
rtk gradlew build               # Gradle build (-80%)
rtk sbt test                    # ScalaTest output (-90%)
rtk sbt compile                 # Compilation errors only (-75%)
```

### Package Managers
```bash
rtk pnpm list                   # Compact dependency tree
rtk pip list                    # Python packages (auto-detect uv)
rtk pip outdated                # Outdated packages
rtk bundle install              # Ruby gems (strip Using lines)
rtk prisma generate             # Schema generation (no ASCII art)
```

### AWS
```bash
rtk aws sts get-caller-identity # One-line identity
rtk aws ec2 describe-instances  # Compact instance list
rtk aws lambda list-functions   # Name/runtime/memory (strips secrets)
rtk aws logs get-log-events     # Timestamped messages only
rtk aws cloudformation describe-stack-events  # Failures first
rtk aws dynamodb scan           # Unwraps type annotations
rtk aws iam list-roles          # Strips policy documents
rtk aws s3 ls                   # Truncated with tee recovery
```

### Containers
```bash
rtk docker ps                   # Compact container list
rtk docker images               # Compact image list
rtk docker logs <container>     # Deduplicated logs
rtk docker compose ps           # Compose services
rtk kubectl pods                # Compact pod list
rtk kubectl logs <pod>          # Deduplicated logs
rtk kubectl services            # Compact service list
rtk oc get pods                 # OpenShift pod summary
rtk oc get services             # OpenShift service list
rtk oc logs <pod>               # Deduplicated logs
```

### Infrastructure as Code
```bash
rtk pulumi preview              # Strip header/URL/duration noise
rtk pulumi up                   # Compact apply output
rtk pulumi destroy              # Compact destroy output
rtk pulumi refresh              # Drift summary
rtk pulumi stack                # Stack metadata (strips owner/timestamps)
```

### Data & Analytics
```bash
rtk json config.json            # Structure without values
rtk deps                        # Dependencies summary
rtk env -f AWS                  # Filtered env vars
rtk log app.log                 # Deduplicated logs
rtk curl <url>                  # Truncate + save full output
rtk wget <url>                  # Download, strip progress bars
rtk summary <long command>      # Heuristic summary
rtk proxy <command>             # Raw passthrough + tracking
```

### Token Savings Analytics
```bash
rtk gain                        # Summary stats
rtk gain --graph                # ASCII graph (last 30 days)
rtk gain --history              # Recent command history
rtk gain --daily                # Day-by-day breakdown
rtk gain --all --format json    # JSON export for dashboards

rtk discover                    # Find missed savings opportunities
rtk discover --all --since 7    # All projects, last 7 days

rtk session                     # Show RTK adoption across recent sessions
```

## Global Flags

```bash
-u, --ultra-compact    # ASCII icons, inline format (extra token savings)
-v, --verbose          # Increase verbosity (-v, -vv, -vvv)
```

## Examples

**Directory listing:**
```
# ls -la (45 lines, ~800 tokens)        # rtk ls (12 lines, ~150 tokens)
drwxr-xr-x  15 user staff 480 ...       my-project/
-rw-r--r--   1 user staff 1234 ...       +-- src/ (8 files)
...                                      |   +-- main.rs
                                         +-- Cargo.toml
```

**Git operations:**
```
# git push (15 lines, ~200 tokens)       # rtk git push (1 line, ~10 tokens)
Enumerating objects: 5, done.             ok main
Counting objects: 100% (5/5), done.
Delta compression using up to 8 threads
...
```

**Test output:**
```
# cargo test (200+ lines on failure)     # rtk test cargo test (~20 lines)
running 15 tests                          FAILED: 2/15 tests
test utils::test_parse ... ok               test_edge_case: assertion failed
test utils::test_format ... ok              test_overflow: panic at utils.rs:18
...
```

## Auto-Rewrite Hook

The most effective way to use rtk. The hook transparently intercepts Bash commands and rewrites them to rtk equivalents before execution.

**Result**: 100% rtk adoption across all conversations and subagents, zero token overhead.

**Scope note:** this only applies to Bash tool calls. Claude Code built-in tools such as `Read`, `Grep`, and `Glob` bypass the hook, so use shell commands or explicit `rtk` commands when you want RTK filtering there.

### Setup

```bash
rtk init -g                 # Install hook + RTK.md (recommended)
rtk init -g --opencode      # OpenCode plugin (instead of Claude Code)
rtk init -g --auto-patch    # Non-interactive (CI/CD)
rtk init -g --hook-only     # Hook only, no RTK.md
rtk init --show             # Verify installation
```

After install, **restart Claude Code**.

## Windows

RTK works on Windows with some limitations. The auto-rewrite hook (`rtk-rewrite.sh`) requires a Unix shell, so on native Windows RTK falls back to **CLAUDE.md injection mode** — your AI assistant receives RTK instructions but commands are not rewritten automatically.

### Recommended: WSL (full support)

For the best experience, use [WSL](https://learn.microsoft.com/en-us/windows/wsl/install) (Windows Subsystem for Linux). Inside WSL, RTK works exactly like Linux — full hook support, auto-rewrite, everything:

```bash
# Inside WSL
cargo install --git https://github.com/mariuszs/rtk-java
rtk init -g
```

### Native Windows (limited support)

On native Windows (cmd.exe / PowerShell), RTK filters work but the hook does not auto-rewrite commands:

```powershell
# 1. Build from source (this fork publishes no prebuilt binaries)
cargo install --git https://github.com/mariuszs/rtk-java
# 2. Initialize (falls back to CLAUDE.md injection)
rtk init -g
# 3. Use rtk explicitly
rtk mvn test
rtk git status
```

**Important**: Do not double-click `rtk.exe` — it is a CLI tool that prints usage and exits immediately. Always run it from a terminal (Command Prompt, PowerShell, or Windows Terminal).

| Feature | WSL | Native Windows |
|---------|-----|----------------|
| Filters (cargo, git, etc.) | Full | Full |
| Auto-rewrite hook | Yes | No (CLAUDE.md fallback) |
| `rtk init -g` | Hook mode | CLAUDE.md mode |
| `rtk gain` / analytics | Full | Full |

## Supported AI Tools

RTK supports 16 AI coding tools. Each integration rewrites shell commands to `rtk` equivalents, reducing the bash output the agent reads where the agent supports command interception.

| Tool | Install | Method |
|------|---------|--------|
| **Claude Code** | `rtk init -g` | PreToolUse hook (bash) |
| **GitHub Copilot (VS Code)** | `rtk init -g --copilot` | PreToolUse hook — transparent rewrite |
| **GitHub Copilot CLI** | `rtk init -g --copilot` | PreToolUse deny-with-suggestion (CLI limitation) |
| **Cursor** | `rtk init -g --agent cursor` | preToolUse hook (hooks.json) |
| **Gemini CLI** | `rtk init -g --gemini` | BeforeTool hook |
| **Codex** | `rtk init -g --codex` | AGENTS.md + RTK.md instructions |
| **Windsurf** | `rtk init -g --agent windsurf` | .windsurfrules (project-scoped) |
| **Cline / Roo Code** | `rtk init --agent cline` | .clinerules (project-scoped) |
| **OpenCode** | `rtk init -g --opencode` | Plugin TS (tool.execute.before) |
| **OpenClaw** | `openclaw plugins install ./openclaw` | Plugin TS (before_tool_call) |
| **Pi** | `rtk init -g --agent pi` (global) | TypeScript extension (tool_call) |
| **Hermes** | `rtk init --agent hermes` | Python plugin adapter (terminal command mutation via `rtk rewrite`) |
| **Mistral Vibe** | `rtk init -g --agent vibe` | `pre_tool` hook (hooks.toml) |
| **Kilo Code** | `rtk init --agent kilocode` | .kilocode/rules/rtk-rules.md (project-scoped) |
| **Google Antigravity** | `rtk init --agent antigravity` | .agents/rules/antigravity-rtk-rules.md (project-scoped) |

For per-agent setup details, override controls, and graceful degradation, see the [Supported Agents guide](https://www.rtk-ai.app/guide/getting-started/supported-agents). The Hermes plugin source and tests live in `hooks/hermes/`; installed Hermes runtime files still live under `~/.hermes/plugins/rtk-rewrite/`.

## Configuration

`~/.config/rtk/config.toml` (macOS: `~/Library/Application Support/rtk/config.toml`):

```toml
[hooks]
exclude_commands = ["curl", "playwright"]  # skip rewrite for these

[tee]
enabled = true          # save raw output on failure (default: true)
mode = "failures"       # "failures", "always", or "never"
```

When a command fails, RTK saves the full unfiltered output so the LLM can read it without re-executing:

```
FAILED: 2/15 tests
[full output: ~/.local/share/rtk/tee/1707753600_cargo_test.log]
```

For the full config reference (all sections, env vars, per-project filters), see the [Configuration guide](https://www.rtk-ai.app/guide/getting-started/configuration).

### Uninstall

```bash
rtk init -g --uninstall     # Remove hook, RTK.md, settings.json entry
cargo uninstall rtk          # Remove binary
brew uninstall rtk           # If installed via Homebrew
```

## Documentation

- **[src/cmds/jvm/README.md](src/cmds/jvm/README.md)** — Maven filter internals: goal routing, XML enrichment, application-package detection
- **[rtk-ai.app/guide](https://www.rtk-ai.app/guide)** — upstream user guide (supported agents, analytics, configuration, troubleshooting — applies to this fork too)
- **[INSTALL.md](INSTALL.md)** — detailed installation reference
- **[ARCHITECTURE.md](docs/contributing/ARCHITECTURE.md)** — system design and technical decisions
- **[CONTRIBUTING.md](CONTRIBUTING.md)** — contribution guide
- **[SECURITY.md](SECURITY.md)** — security policy

## Privacy & Telemetry

RTK can collect **anonymous, aggregate usage metrics** once per day. Telemetry is **disabled by default** and requires **explicit opt-in consent** (GDPR Art. 6, 7) during `rtk init` or via `rtk telemetry enable`. This data helps us build a better product: identifying which commands need filters, which filters need improvement, and how much value RTK delivers. For the full list of fields, data handling, and contributor guidelines, see **[docs/TELEMETRY.md](docs/TELEMETRY.md)**.

**What is collected and why:**

| Category | Data | Why |
|----------|------|-----|
| Identity | Salted device hash (SHA-256, not reversible) | Count unique installations without tracking individuals |
| Environment | RTK version, OS, architecture, install method | Know which platforms to support and test |
| Usage volume | Command count (24h), total commands, tokens saved (24h/30d/total) | Measure adoption and value delivered |
| Quality | Top 5 passthrough commands (0% savings), parse failure count, commands with <30% savings | Identify missing filters and weak ones to improve |
| Ecosystem | Command category distribution (e.g. git 45%, cargo 20%, js 15%) | Prioritize filter development for popular ecosystems |
| Retention | Days since first use, active days in last 30 | Understand engagement and detect churn |
| Adoption | AI agent hook type (claude/gemini/codex), custom TOML filter count | Track integration coverage and DSL adoption |
| Configuration | Whether config.toml exists, number of excluded commands, project count | Understand user maturity and customization patterns |
| Features | Usage counts for meta-commands (gain, discover, proxy, verify) | Know which RTK features are valued vs unused |
| Economics | Estimated USD savings (based on API token pricing) | Quantify the value RTK provides to users |

All data is **aggregate counts or anonymized command names** (first 3 words, no arguments). Top commands report only tool names (e.g. "git", "cargo"), never full command lines.

**What is NOT collected:** source code, file paths, command arguments, secrets, environment variables, personal data, or repository contents.

**Manage telemetry:**
```bash
rtk telemetry status     # Check current consent state
rtk telemetry enable     # Give consent (interactive prompt)
rtk telemetry disable    # Withdraw consent — stops all collection immediately
rtk telemetry forget     # Withdraw consent + delete all local data + request server-side erasure
```

**Override via environment:**
```bash
export RTK_TELEMETRY_DISABLED=1   # Blocks telemetry regardless of consent
```

## Relationship to upstream

`rtk-java` tracks [rtk-ai/rtk](https://github.com/rtk-ai/rtk) `master` and merges it in
regularly. The Maven work is developed here first and upstreamed as PRs — parts of it
(the base `mvn` module) have already landed upstream.

Beyond Maven, this fork carries fixes not yet released upstream: `find` falls back to raw
output on unsupported flags, `rtk lint` no longer hijacks `npm run` scripts, `tsc` stopped
inflating its own output with a synthetic summary, `curl`/`npm`/`npx` are no longer
rewritten, `grep` context separators are faithful to real `grep`, and several UTF-8
boundary panics in analytics were fixed.

## Credits

Upstream rtk is built by the [rtk-ai](https://github.com/rtk-ai/rtk) core team —
Patrick Szymkowiak (founder), Florian Bruniaux, Adrien Eppling, Nicolas Le Cam and
Takayuki Maeda. This fork exists on top of their work; all upstream credit belongs
to them.

## Contributing

Issues and PRs about Java/Maven filtering are welcome [here](https://github.com/mariuszs/rtk-java/issues).
Anything else is better filed [upstream](https://github.com/rtk-ai/rtk), where the
upstream community and its [Discord](https://discord.gg/RySmvNF5kF) live.

## License

Apache License 2.0 - see [LICENSE](LICENSE) for details.

## Disclaimer

See [DISCLAIMER.md](DISCLAIMER.md).
