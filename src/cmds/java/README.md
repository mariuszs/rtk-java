# Java Ecosystem

> Part of [`src/cmds/`](../README.md) — see also [docs/contributing/TECHNICAL.md](../../../docs/contributing/TECHNICAL.md)

## Specifics

- **mvn_cmd.rs** handles Maven (`mvn`) and Maven Wrapper (`mvnw`) commands
- Auto-detects `mvnw` wrapper in project root; falls back to system `mvn`
- `mvn test` uses a state-machine parser (Preamble → Testing → Summary → Done) for 97-99%+ savings on real-world output
- `mvn compile` uses line filtering to strip `[INFO]` noise, download progress, JVM/native-access warnings, and plugin chatter (jOOQ codegen, Liquibase, npm/React builds, typescript-generator). Also routes `process-classes` and `test-compile` through the same filter (same noise profile)
- `mvn checkstyle:check` (aliased as `checkstyle`) compacts violation lines to `path:line:col [Rule] message`, strips mvn startup noise and Help-link boilerplate, keeps `N Checkstyle violations` summary and BUILD SUCCESS/FAILURE
- `mvn dependency:tree` strips "omitted for duplicate" lines, "version managed from" annotations, and collapses deep transitive branches
- Unknown goals stream via `cmd.status()` passthrough (safe for long-running goals like `spring-boot:run`); rare lifecycle phases (`package`, `install`, `verify`, `clean`, `deploy`) also passthrough — filtered only when the output shape matches compile
- Routing via Clap sub-enum with `#[command(external_subcommand)] Other` for unknown goals; compile-like and checkstyle goals received as `Other` are auto-re-dispatched by `route_goal` to the right filter

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
mvn test: 0 tests executed — surefire detected no tests. Check pom.xml (surefire plugin configuration) or run: rtk proxy mvn test
```

### Bypass

For the rare cases where you need the full raw Maven output:

```bash
rtk proxy mvn test
```
