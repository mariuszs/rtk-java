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
