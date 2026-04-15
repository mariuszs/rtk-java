//! Autodetects the application Java package from `pom.xml <groupId>`.
//! Used by `surefire_reports` / `stack_trace` to classify application frames.
//! Can be overridden by `RTK_MVN_APP_PACKAGE` env var.
