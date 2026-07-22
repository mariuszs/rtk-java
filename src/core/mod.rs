//! Building blocks shared across all RTK modules.

pub mod args_utils;
pub mod config;
pub mod constants;
pub mod display_helpers;
pub mod filter;
pub mod guard;
pub mod runner;
/// Test-only: re-scores `history.db` against the truncation limit.
#[cfg(test)]
pub mod savings_audit;
pub mod stream;
pub mod tee;
pub mod telemetry;
pub mod telemetry_cmd;
pub mod toml_filter;
pub mod tracking;
pub mod truncate;
pub mod utils;
