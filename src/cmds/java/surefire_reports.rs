//! Parses Maven Surefire/Failsafe XML test reports from
//! `target/surefire-reports/TEST-*.xml` and `target/failsafe-reports/*.xml`.
//! Uses quick-xml streaming parser. Time-gated by `started_at` to skip stale
//! reports from previous runs.
