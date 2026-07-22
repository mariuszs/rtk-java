//! Honest-savings audit over the real tracking database.
//!
//! `rtk gain` scores every run against the full raw output. That baseline is a
//! counterfactual: the host agent truncates a tool result before the model sees
//! it, so characters past the limit were never billable and cannot be "saved".
//! This module re-scores `history.db` against `min(raw, limit)` — what the agent
//! was actually billed — and ranks filters by `honest% x volume`, the only
//! figure that says whether a filter earns its place. A filter whose `SAVED`
//! column is negative is not weak, it is a defect: it costs more than running
//! the bare command.
//!
//! The reconstruction is exact. The DB stores `ceil(chars / 4)`, so capping a
//! stored count at `ceil(limit / 4)` reproduces the character limit:
//! below the limit the cap is inert, above it both sides collapse to the same
//! number.
//!
//! The database is opened read-only — the audit can never touch real history.
//!
//! ```text
//! cargo test usage_audit -- --ignored --nocapture
//! RTK_AUDIT_DAYS=0 cargo test usage_audit -- --ignored --nocapture     # all history
//! RTK_AUDIT_LIMIT=10000 cargo test usage_audit -- --ignored --nocapture
//! RTK_AUDIT_MIN_RUNS=1 cargo test usage_audit -- --ignored --nocapture
//! ```

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::core::tracking::{get_db_path, DEFAULT_AGENT_OUTPUT_LIMIT};

/// Truncation limit in characters, honouring `RTK_AUDIT_LIMIT`.
pub(crate) fn limit() -> usize {
    env_usize("RTK_AUDIT_LIMIT").unwrap_or(DEFAULT_AGENT_OUTPUT_LIMIT)
}

/// Window in days, honouring `RTK_AUDIT_DAYS`. `0` means all history.
pub(crate) fn days() -> usize {
    env_usize("RTK_AUDIT_DAYS").unwrap_or(60)
}

/// Commands below this run count are folded into one row, honouring
/// `RTK_AUDIT_MIN_RUNS`. Rare commands carry no signal but plenty of noise.
pub(crate) fn min_runs() -> u64 {
    env_usize("RTK_AUDIT_MIN_RUNS").unwrap_or(20) as u64
}

fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}

/// Stored token counts are `ceil(chars / 4)`, so the character limit maps onto
/// a token cap of `ceil(limit / 4)`.
fn token_cap(limit_chars: usize) -> u64 {
    limit_chars.div_ceil(4) as u64
}

/// One command, aggregated over the window.
#[derive(Default)]
pub(crate) struct UsageRow {
    pub command: String,
    pub runs: u64,
    /// Billable (truncation-capped) tokens the bare command would have cost.
    pub bill_raw: u64,
    /// Billable tokens rtk actually cost.
    pub bill_out: u64,
    /// Uncapped totals, kept only to contrast with what `rtk gain` claims.
    pub raw: u64,
    pub out: u64,
}

impl UsageRow {
    /// Billable tokens saved. Negative means the filter inflates its output.
    pub fn saved(&self) -> i64 {
        self.bill_raw as i64 - self.bill_out as i64
    }

    /// Savings against the honest (truncation-capped) baseline.
    pub fn honest_pct(&self) -> f64 {
        pct(self.bill_raw, self.bill_out)
    }

    /// Savings against the full raw output — the inflated figure.
    pub fn claimed_pct(&self) -> f64 {
        pct(self.raw, self.out)
    }
}

fn pct(base: u64, actual: u64) -> f64 {
    if base == 0 {
        return 0.0;
    }
    100.0 - (actual as f64 / base as f64 * 100.0)
}

/// Which filter handled the run. `rtk_cmd` looks like `rtk grep -n foo`, so the
/// second word is the filter; fall back to the raw command when it is missing.
fn command_name(rtk_cmd: &str, original_cmd: &str) -> String {
    let mut words = rtk_cmd.split_whitespace();
    if words.next() == Some("rtk") {
        if let Some(name) = words.next() {
            return name.to_string();
        }
    }
    original_cmd
        .split_whitespace()
        .next()
        .unwrap_or("(unknown)")
        .to_string()
}

/// Open the user's history database read-only. `Ok(None)` when it does not
/// exist yet — an audit with no data is not a failure.
pub(crate) fn open_history() -> Result<Option<(Connection, PathBuf)>> {
    let path = get_db_path().context("Failed to resolve history database path")?;
    if !path.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("Failed to open {} read-only", path.display()))?;
    Ok(Some((conn, path)))
}

/// Aggregate the window into per-command rows, ranked by billable tokens saved.
pub(crate) fn load(
    conn: &Connection,
    window_days: usize,
    limit_chars: usize,
) -> Result<Vec<UsageRow>> {
    let cap = token_cap(limit_chars);
    let mut by_command: HashMap<String, UsageRow> = HashMap::new();

    // `strftime` renders the cutoff in the same `YYYY-MM-DDT` shape the
    // timestamps use, so the string comparison is a real date comparison.
    let sql = if window_days == 0 {
        "SELECT rtk_cmd, original_cmd, input_tokens, output_tokens FROM commands".to_string()
    } else {
        format!(
            "SELECT rtk_cmd, original_cmd, input_tokens, output_tokens FROM commands \
             WHERE timestamp >= strftime('%Y-%m-%dT', 'now', '-{window_days} days')"
        )
    };

    let mut stmt = conn
        .prepare(&sql)
        .context("Failed to prepare audit query")?;
    let mut rows = stmt.query([]).context("Failed to run audit query")?;

    while let Some(row) = rows.next().context("Failed to read audit row")? {
        let rtk_cmd: String = row.get(0)?;
        let original_cmd: String = row.get(1)?;
        let raw: i64 = row.get(2)?;
        let out: i64 = row.get(3)?;
        let (raw, out) = (raw.max(0) as u64, out.max(0) as u64);

        let name = command_name(&rtk_cmd, &original_cmd);
        let entry = by_command.entry(name.clone()).or_default();
        entry.command = name;
        entry.runs += 1;
        entry.raw += raw;
        entry.out += out;
        entry.bill_raw += raw.min(cap);
        entry.bill_out += out.min(cap);
    }

    let mut rows: Vec<UsageRow> = by_command.into_values().collect();
    rows.sort_by(|a, b| b.saved().cmp(&a.saved()).then(b.runs.cmp(&a.runs)));
    Ok(rows)
}

/// Fold everything under `threshold` runs into a single trailing row, so the
/// table shows commands with enough volume to judge without hiding the rest
/// from the totals.
pub(crate) fn fold_rare(rows: Vec<UsageRow>, threshold: u64) -> Vec<UsageRow> {
    let (kept, rare): (Vec<UsageRow>, Vec<UsageRow>) =
        rows.into_iter().partition(|r| r.runs >= threshold);
    if rare.is_empty() {
        return kept;
    }
    let mut folded = UsageRow {
        command: format!("({} cmds <{threshold} runs)", rare.len()),
        ..Default::default()
    };
    for r in &rare {
        folded.runs += r.runs;
        folded.raw += r.raw;
        folded.out += r.out;
        folded.bill_raw += r.bill_raw;
        folded.bill_out += r.bill_out;
    }
    let mut kept = kept;
    kept.push(folded);
    kept
}

/// Print the ranked table plus the totals every decision should be made from.
pub(crate) fn report(rows: &[UsageRow], window_days: usize, limit_chars: usize) {
    let mut total = UsageRow {
        command: "TOTAL".to_string(),
        ..Default::default()
    };
    for r in rows {
        total.runs += r.runs;
        total.raw += r.raw;
        total.out += r.out;
        total.bill_raw += r.bill_raw;
        total.bill_out += r.bill_out;
    }
    let total_saved = total.saved().max(1) as f64;

    let window = if window_days == 0 {
        "all history".to_string()
    } else {
        format!("last {window_days}d")
    };
    println!("\n=== rtk honest savings ({window}, limit={limit_chars} chars) ===");
    println!(
        "{:<16} {:>7} {:>11} {:>11} {:>8} {:>12} {:>7} {:>9}",
        "COMMAND", "RUNS", "BILL_RAW", "BILL_OUT", "HONEST%", "SAVED", "SHARE%", "CLAIMED%"
    );

    for r in rows {
        println!(
            "{:<16} {:>7} {:>11} {:>11} {:>7.1} {:>12} {:>6.1} {:>8.1}",
            r.command,
            r.runs,
            r.bill_raw,
            r.bill_out,
            r.honest_pct(),
            r.saved(),
            r.saved() as f64 / total_saved * 100.0,
            r.claimed_pct(),
        );
    }

    println!(
        "{:<16} {:>7} {:>11} {:>11} {:>7.1} {:>12} {:>6.1} {:>8.1}",
        total.command,
        total.runs,
        total.bill_raw,
        total.bill_out,
        total.honest_pct(),
        total.saved(),
        100.0,
        total.claimed_pct(),
    );

    let inflating: Vec<&UsageRow> = rows.iter().filter(|r| r.saved() < 0).collect();
    if inflating.is_empty() {
        println!("\nNo command bills more than its bare equivalent.");
    } else {
        println!("\nINFLATING — these cost more than not filtering at all:");
        for r in inflating {
            println!(
                "  {:<14} {:>6} runs  {:>+10} tokens  ({:.1}%)",
                r.command,
                r.runs,
                r.saved(),
                r.honest_pct()
            );
        }
    }

    println!(
        "\nHonest {:.1}% vs claimed {:.1}% — the gap is savings on characters the \
         agent was never billed for.",
        total.honest_pct(),
        total.claimed_pct(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Re-score real usage. Ignored: depends on the developer's own history.db.
    #[test]
    #[ignore]
    fn usage_audit() {
        let Some((conn, path)) = open_history().expect("open history db") else {
            println!("no history database yet — nothing to audit");
            return;
        };
        let (window, lim) = (days(), limit());
        println!("source: {}", path.display());

        let rows = load(&conn, window, lim).expect("load usage rows");
        if rows.is_empty() {
            println!("no runs recorded in the window");
            return;
        }
        report(&fold_rare(rows, min_runs()), window, lim);
    }

    fn memory_db() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute(
            "CREATE TABLE commands (
                id INTEGER PRIMARY KEY,
                timestamp TEXT NOT NULL,
                original_cmd TEXT NOT NULL,
                rtk_cmd TEXT NOT NULL,
                input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                saved_tokens INTEGER NOT NULL,
                savings_pct REAL NOT NULL
            )",
            [],
        )
        .expect("create table");
        conn
    }

    fn insert(conn: &Connection, rtk_cmd: &str, input: i64, output: i64) {
        conn.execute(
            "INSERT INTO commands (timestamp, original_cmd, rtk_cmd, input_tokens, \
             output_tokens, saved_tokens, savings_pct) \
             VALUES (strftime('%Y-%m-%dT%H:%M:%S', 'now'), 'cmd', ?1, ?2, ?3, 0, 0.0)",
            rusqlite::params![rtk_cmd, input, output],
        )
        .expect("insert row");
    }

    #[test]
    fn command_name_prefers_the_rtk_filter() {
        assert_eq!(
            command_name("rtk grep -n foo src/", "grep -n foo src/"),
            "grep"
        );
        assert_eq!(command_name("rtk mvn test", "mvn test"), "mvn");
    }

    #[test]
    fn command_name_falls_back_to_the_original() {
        assert_eq!(command_name("", "cargo build"), "cargo");
        assert_eq!(command_name("rtk", "ls -la"), "ls");
        assert_eq!(command_name("", ""), "(unknown)");
    }

    #[test]
    fn token_cap_matches_the_character_limit() {
        // ceil(30000 / 4) — the exact token count of a fully truncated result.
        assert_eq!(token_cap(30_000), 7_500);
        assert_eq!(token_cap(10_001), 2_501);
        assert_eq!(token_cap(0), 0);
    }

    #[test]
    fn oversized_runs_are_capped_on_both_sides() {
        let conn = memory_db();
        // 40k tokens of raw, 20k of filtered: both far past the 7500 cap, so the
        // run is worth exactly nothing — this is the reactor dependency:tree case.
        insert(&conn, "rtk mvn dependency:tree", 40_000, 20_000);

        let rows = load(&conn, 0, 30_000).expect("load");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].bill_raw, 7_500);
        assert_eq!(rows[0].bill_out, 7_500);
        assert_eq!(rows[0].saved(), 0);
        // The uncapped baseline is what makes this look like a 50% win.
        assert!((rows[0].claimed_pct() - 50.0).abs() < 0.01);
    }

    #[test]
    fn small_runs_are_untouched_by_the_cap() {
        let conn = memory_db();
        insert(&conn, "rtk git status", 100, 25);

        let rows = load(&conn, 0, 30_000).expect("load");
        assert_eq!(rows[0].bill_raw, 100);
        assert_eq!(rows[0].bill_out, 25);
        assert!((rows[0].honest_pct() - rows[0].claimed_pct()).abs() < 0.01);
    }

    #[test]
    fn inflating_filters_report_negative_savings() {
        let conn = memory_db();
        // The pre-fix tsc defect: a clean run emitted a synthetic summary.
        insert(&conn, "rtk tsc --noEmit", 0, 7);
        insert(&conn, "rtk tsc --noEmit", 0, 7);

        let rows = load(&conn, 0, 30_000).expect("load");
        assert_eq!(rows[0].saved(), -14);
        assert!(rows[0].honest_pct() == 0.0, "no baseline to save against");
    }

    #[test]
    fn rows_rank_by_billable_tokens_saved() {
        let conn = memory_db();
        insert(&conn, "rtk grep x", 1_000, 900); // small ratio, still the winner
        insert(&conn, "rtk ls", 100, 10); // great ratio, trivial volume

        let rows = load(&conn, 0, 30_000).expect("load");
        assert_eq!(rows[0].command, "grep");
        assert_eq!(rows[1].command, "ls");
    }

    #[test]
    fn rare_commands_fold_without_leaving_the_totals() {
        let conn = memory_db();
        for _ in 0..5 {
            insert(&conn, "rtk grep x", 100, 10);
        }
        insert(&conn, "rtk glab mr list", 100, 10);

        let folded = fold_rare(load(&conn, 0, 30_000).expect("load"), 5);
        assert_eq!(folded.len(), 2);
        assert_eq!(folded[0].command, "grep");
        assert_eq!(folded[1].command, "(1 cmds <5 runs)");
        assert_eq!(folded.iter().map(|r| r.saved()).sum::<i64>(), 5 * 90 + 90);
    }

    #[test]
    fn the_window_excludes_older_runs() {
        let conn = memory_db();
        insert(&conn, "rtk grep x", 100, 10);
        conn.execute(
            "INSERT INTO commands (timestamp, original_cmd, rtk_cmd, input_tokens, \
             output_tokens, saved_tokens, savings_pct) \
             VALUES ('2020-01-01T00:00:00', 'grep', 'rtk grep old', 999, 1, 0, 0.0)",
            [],
        )
        .expect("insert old row");

        assert_eq!(load(&conn, 0, 30_000).expect("all").len(), 1);
        let windowed = load(&conn, 60, 30_000).expect("windowed");
        assert_eq!(windowed[0].runs, 1, "the 2020 run must fall outside 60d");
    }
}
