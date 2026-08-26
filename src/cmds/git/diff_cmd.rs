//! Compares two files and shows only the changed lines.

use crate::core::guard::never_worse;
use crate::core::tracking;
use anyhow::Result;
use std::fs;
use std::path::Path;

/// Ultra-condensed diff - only changed lines, no context.
/// Returns the diff-convention exit code: 0 if identical, 1 if files differ.
pub fn run(file1: &Path, file2: &Path, verbose: u8) -> Result<i32> {
    let timer = tracking::TimedExecution::start();

    if verbose > 0 {
        eprintln!("Comparing: {} vs {}", file1.display(), file2.display());
    }

    let content1 = fs::read_to_string(file1)?;
    let content2 = fs::read_to_string(file2)?;
    let raw = format!("{}\n---\n{}", content1, content2);

    let (rtk, exit_code) = render_file_diff(file1, file2, &content1, &content2);

    let shown = never_worse(&raw, &rtk);
    print!("{}", shown);
    timer.track(
        &format!("diff {} {}", file1.display(), file2.display()),
        "rtk diff",
        &raw,
        shown,
    );
    Ok(exit_code)
}

/// Renders the condensed file comparison and returns it with the
/// diff-convention exit code (0 = identical, 1 = differences found).
fn render_file_diff(file1: &Path, file2: &Path, content1: &str, content2: &str) -> (String, i32) {
    let lines1: Vec<&str> = content1.lines().collect();
    let lines2: Vec<&str> = content2.lines().collect();
    let diff = compute_diff(&lines1, &lines2);

    if diff.changes.is_empty() {
        return ("[ok] Files are identical\n".to_string(), 0);
    }

    let mut rtk = String::new();
    rtk.push_str(&format!("{} → {}\n", file1.display(), file2.display()));
    rtk.push_str(&format!(
        "   +{} added, -{} removed, ~{} modified\n\n",
        diff.added, diff.removed, diff.modified
    ));
    rtk.push_str(&format_diff_changes(&diff));
    (rtk, 1)
}

/// Run diff from stdin (piped command output)
pub fn run_stdin(_verbose: u8) -> Result<()> {
    use std::io::{self, Read};
    let timer = tracking::TimedExecution::start();

    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;

    // Parse unified diff format
    let condensed = condense_unified_diff(&input);
    let shown = never_worse(&input, &condensed);
    println!("{}", shown);

    timer.track("diff (stdin)", "rtk diff (stdin)", &input, shown);

    Ok(())
}

#[derive(Debug)]
enum DiffChange {
    Added(usize, String),
    Removed(usize, String),
    Modified(usize, String, String),
}

struct DiffResult {
    added: usize,
    removed: usize,
    modified: usize,
    changes: Vec<DiffChange>,
}

fn format_diff_changes(diff: &DiffResult) -> String {
    let mut out = String::new();
    for change in &diff.changes {
        match change {
            DiffChange::Added(ln, c) => out.push_str(&format!("+{:4} {}\n", ln, c)),
            DiffChange::Removed(ln, c) => out.push_str(&format!("-{:4} {}\n", ln, c)),
            DiffChange::Modified(ln, old, new) => {
                out.push_str(&format!("~{:4} {} → {}\n", ln, old, new))
            }
        }
    }
    out
}

/// Where the two files line up again after a mismatch.
enum Resync {
    /// `k` lines vanished from the left file.
    Removed(usize),
    /// `k` lines appeared in the right file.
    Added(usize),
}

/// How far ahead to look for that resynchronization point. Bounds the work at
/// O(lines x RESYNC_WINDOW) while covering the edits rtk is actually asked to
/// compare — a class added to a test digest, a block moved in a config. Past
/// the window the walk falls back to pairwise substitution, which is what this
/// whole function used to do unconditionally.
const RESYNC_WINDOW: usize = 200;

/// Nearest shift that makes the two files match again, smallest first.
fn find_resync(lines1: &[&str], i: usize, lines2: &[&str], j: usize) -> Option<Resync> {
    for k in 1..=RESYNC_WINDOW {
        if lines1.get(i + k) == Some(&lines2[j]) {
            return Some(Resync::Removed(k));
        }
        if lines2.get(j + k) == Some(&lines1[i]) {
            return Some(Resync::Added(k));
        }
    }
    None
}

/// Walk both files in step, resynchronizing after an insertion or a deletion.
///
/// Comparing line `i` of one file to line `i` of the other — which this did
/// before — reports every line after a single middle deletion as changed. On
/// rtk's own per-class test digest, where neighbouring lines differ only in a
/// class name and so score well above the 0.5 similarity threshold, one
/// dropped class rendered as 13 bogus "modified" pairs: both wrong about what
/// changed and paid for in full.
fn compute_diff(lines1: &[&str], lines2: &[&str]) -> DiffResult {
    let mut changes = Vec::new();
    let mut added = 0;
    let mut removed = 0;
    let mut modified = 0;
    let mut i = 0;
    let mut j = 0;

    while i < lines1.len() || j < lines2.len() {
        match (lines1.get(i), lines2.get(j)) {
            (Some(a), Some(b)) if a == b => {
                i += 1;
                j += 1;
            }
            (Some(a), Some(b)) => {
                // An in-place edit lines up again on the very next line; only
                // look for a shift when it does not.
                let substitution = lines1.get(i + 1) == lines2.get(j + 1);
                let shift = if substitution {
                    None
                } else {
                    find_resync(lines1, i, lines2, j)
                };
                match shift {
                    Some(Resync::Removed(k)) => {
                        for (offset, line) in lines1[i..i + k].iter().enumerate() {
                            changes.push(DiffChange::Removed(i + offset + 1, line.to_string()));
                            removed += 1;
                        }
                        i += k;
                    }
                    Some(Resync::Added(k)) => {
                        for (offset, line) in lines2[j..j + k].iter().enumerate() {
                            changes.push(DiffChange::Added(j + offset + 1, line.to_string()));
                            added += 1;
                        }
                        j += k;
                    }
                    None => {
                        // Check if it's similar (modification) or completely different
                        if similarity(a, b) > 0.5 {
                            changes.push(DiffChange::Modified(i + 1, a.to_string(), b.to_string()));
                            modified += 1;
                        } else {
                            changes.push(DiffChange::Removed(i + 1, a.to_string()));
                            changes.push(DiffChange::Added(j + 1, b.to_string()));
                            removed += 1;
                            added += 1;
                        }
                        i += 1;
                        j += 1;
                    }
                }
            }
            (Some(a), None) => {
                changes.push(DiffChange::Removed(i + 1, a.to_string()));
                removed += 1;
                i += 1;
            }
            (None, Some(b)) => {
                changes.push(DiffChange::Added(j + 1, b.to_string()));
                added += 1;
                j += 1;
            }
            (None, None) => break,
        }
    }

    DiffResult {
        added,
        removed,
        modified,
        changes,
    }
}

fn similarity(a: &str, b: &str) -> f64 {
    let a_chars: std::collections::HashSet<char> = a.chars().collect();
    let b_chars: std::collections::HashSet<char> = b.chars().collect();

    let intersection = a_chars.intersection(&b_chars).count();
    let union = a_chars.union(&b_chars).count();

    if union == 0 {
        1.0
    } else {
        intersection as f64 / union as f64
    }
}

fn condense_unified_diff(diff: &str) -> String {
    let mut result = Vec::new();
    let mut current_file = String::new();
    let mut added = 0;
    let mut removed = 0;
    let mut changes = Vec::new();

    // Never truncate diff content — users make decisions based on this data.
    // Only strip diff metadata (headers, @@ hunks); all +/- lines shown in full.
    for line in diff.lines() {
        if line.starts_with("diff --git") || line.starts_with("--- ") || line.starts_with("+++ ") {
            if line.starts_with("+++ ") {
                if !current_file.is_empty() && (added > 0 || removed > 0) {
                    result.push(format!("[file] {} (+{} -{})", current_file, added, removed));
                    for c in &changes {
                        result.push(format!("  {}", c));
                    }
                    let total = added + removed;
                    if total > 10 {
                        result.push(format!("  ... +{} more", total - 10));
                    }
                }
                current_file = line
                    .trim_start_matches("+++ ")
                    .trim_start_matches("b/")
                    .to_string();
                added = 0;
                removed = 0;
                changes.clear();
            }
        } else if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
            changes.push(line.to_string());
        } else if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
            changes.push(line.to_string());
        }
    }

    // Last file
    if !current_file.is_empty() && (added > 0 || removed > 0) {
        result.push(format!("[file] {} (+{} -{})", current_file, added, removed));
        for c in &changes {
            result.push(format!("  {}", c));
        }
        let total = added + removed;
        if total > 10 {
            result.push(format!("  ... +{} more", total - 10));
        }
    }

    result.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- similarity ---

    #[test]
    fn test_similarity_identical() {
        assert_eq!(similarity("hello", "hello"), 1.0);
    }

    #[test]
    fn test_similarity_completely_different() {
        assert_eq!(similarity("abc", "xyz"), 0.0);
    }

    #[test]
    fn test_similarity_empty_strings() {
        // Both empty: union is 0, returns 1.0 by convention
        assert_eq!(similarity("", ""), 1.0);
    }

    #[test]
    fn test_similarity_partial_overlap() {
        let s = similarity("abcd", "abef");
        // Shared: a, b. Union: a, b, c, d, e, f = 6. Jaccard = 2/6
        assert!((s - 2.0 / 6.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_similarity_threshold_for_modified() {
        // "let x = 1;" vs "let x = 2;" should be > 0.5 (treated as modification)
        assert!(similarity("let x = 1;", "let x = 2;") > 0.5);
    }

    // --- compute_diff ---

    #[test]
    fn test_compute_diff_identical() {
        let a = vec!["line1", "line2", "line3"];
        let b = vec!["line1", "line2", "line3"];
        let result = compute_diff(&a, &b);
        assert_eq!(result.added, 0);
        assert_eq!(result.removed, 0);
        assert_eq!(result.modified, 0);
        assert!(result.changes.is_empty());
    }

    #[test]
    fn test_compute_diff_added_lines() {
        let a = vec!["line1"];
        let b = vec!["line1", "line2", "line3"];
        let result = compute_diff(&a, &b);
        assert_eq!(result.added, 2);
        assert_eq!(result.removed, 0);
    }

    #[test]
    fn test_compute_diff_removed_lines() {
        let a = vec!["line1", "line2", "line3"];
        let b = vec!["line1"];
        let result = compute_diff(&a, &b);
        assert_eq!(result.removed, 2);
        assert_eq!(result.added, 0);
    }

    #[test]
    fn test_compute_diff_modified_line() {
        // Similar lines (>0.5 similarity) are classified as modified
        let a = vec!["let x = 1;"];
        let b = vec!["let x = 2;"];
        let result = compute_diff(&a, &b);
        assert_eq!(result.modified, 1);
        assert_eq!(result.added, 0);
        assert_eq!(result.removed, 0);
    }

    #[test]
    fn test_compute_diff_completely_different_line() {
        // Dissimilar lines (<= 0.5 similarity) are added+removed, not modified
        let a = vec!["aaaa"];
        let b = vec!["zzzz"];
        let result = compute_diff(&a, &b);
        assert_eq!(result.modified, 0);
        assert_eq!(result.added, 1);
        assert_eq!(result.removed, 1);
    }

    /// A line removed from the middle must not desynchronize everything
    /// after it. The index-by-index comparison this replaced reported the
    /// shifted tail as a run of "modified" pairs, so the answer to "what
    /// changed?" was buried in noise that also had to be paid for.
    #[test]
    fn test_compute_diff_middle_deletion_is_one_change() {
        let a = vec!["a", "b", "c", "d", "e"];
        let b = vec!["a", "c", "d", "e"];
        let result = compute_diff(&a, &b);
        assert_eq!(
            (result.added, result.removed, result.modified),
            (0, 1, 0),
            "changes: {:?}",
            result.changes
        );
    }

    #[test]
    fn test_compute_diff_middle_insertion_is_one_change() {
        let a = vec!["a", "c", "d", "e"];
        let b = vec!["a", "b", "c", "d", "e"];
        let result = compute_diff(&a, &b);
        assert_eq!(
            (result.added, result.removed, result.modified),
            (1, 0, 0),
            "changes: {:?}",
            result.changes
        );
    }

    /// Regression for the shape that exposed this: rtk's own per-class test
    /// digest, where every line differs from its neighbours only in a class
    /// name and a count. Char-set similarity rates any two of them well
    /// above the 0.5 "modified" threshold, so a single dropped class used to
    /// render as a cascade of bogus modifications — 13 of them for one
    /// deleted line in a real 271-line digest.
    #[test]
    fn test_compute_diff_deletion_among_near_identical_lines() {
        let a: Vec<String> = (0..40)
            .map(|i| format!("[INFO] Tests run: 2 -- in com.example.Suite{i:02}Test"))
            .collect();
        let mut b = a.clone();
        b.remove(20);
        let a_ref: Vec<&str> = a.iter().map(String::as_str).collect();
        let b_ref: Vec<&str> = b.iter().map(String::as_str).collect();

        let result = compute_diff(&a_ref, &b_ref);
        assert_eq!(
            (result.added, result.removed, result.modified),
            (0, 1, 0),
            "changes: {:?}",
            result.changes
        );
        match result.changes.as_slice() {
            [DiffChange::Removed(line, text)] => {
                assert_eq!(*line, 21, "1-based position in the left file");
                assert!(text.contains("Suite20Test"), "wrong line reported: {text}");
            }
            other => panic!("expected a single removal, got {other:?}"),
        }
    }

    /// Resynchronization must not paper over a genuine in-place edit.
    #[test]
    fn test_compute_diff_substitution_among_similar_lines() {
        let a = vec!["alpha", "bravo", "charlie", "delta"];
        let b = vec!["alpha", "brava", "charlie", "delta"];
        let result = compute_diff(&a, &b);
        assert_eq!(
            (result.added, result.removed, result.modified),
            (0, 0, 1),
            "changes: {:?}",
            result.changes
        );
    }

    #[test]
    fn test_compute_diff_empty_inputs() {
        let result = compute_diff(&[], &[]);
        assert_eq!(result.added, 0);
        assert_eq!(result.removed, 0);
        assert!(result.changes.is_empty());
    }

    // --- render_file_diff (issue #2364 regression) ---

    #[test]
    fn test_render_modified_only_yaml_not_identical() {
        // "a: 1" vs "a: 2" is classified as modified (similarity > 0.5);
        // the identical check must not ignore modified-only diffs.
        let (out, code) = render_file_diff(
            Path::new("one.yaml"),
            Path::new("two.yaml"),
            "a: 1\n",
            "a: 2\n",
        );
        assert!(
            !out.contains("identical"),
            "modified-only diff reported as identical:\n{}",
            out
        );
        assert!(out.contains("~1 modified"));
        assert!(out.contains("a: 1"));
        assert!(out.contains("a: 2"));
        assert_eq!(code, 1, "differing files must exit 1 (diff convention)");
    }

    #[test]
    fn test_render_modified_only_json_not_identical() {
        let (out, code) = render_file_diff(
            Path::new("j1.json"),
            Path::new("j2.json"),
            "{\"a\": 1}\n",
            "{\"a\": 2}\n",
        );
        assert!(
            !out.contains("identical"),
            "modified-only diff reported as identical:\n{}",
            out
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn test_render_identical_files_exit_zero() {
        let (out, code) = render_file_diff(
            Path::new("a.yaml"),
            Path::new("b.yaml"),
            "a: 1\nb: 2\n",
            "a: 1\nb: 2\n",
        );
        assert!(out.contains("[ok] Files are identical"));
        assert_eq!(code, 0);
    }

    #[test]
    fn test_render_added_removed_exit_one() {
        let (out, code) = render_file_diff(Path::new("t1.txt"), Path::new("t2.txt"), "x\n", "y\n");
        assert!(out.contains("+1 added, -1 removed"));
        assert_eq!(code, 1);
    }

    // --- condense_unified_diff ---

    #[test]
    fn test_condense_unified_diff_single_file() {
        let diff = r#"diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,4 @@
 fn main() {
+    println!("hello");
     println!("world");
 }
"#;
        let result = condense_unified_diff(diff);
        assert!(result.contains("src/main.rs"));
        assert!(result.contains("+1"));
        assert!(result.contains("println"));
    }

    #[test]
    fn test_condense_unified_diff_multiple_files() {
        let diff = r#"diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
+added line
diff --git a/b.rs b/b.rs
--- a/b.rs
+++ b/b.rs
-removed line
"#;
        let result = condense_unified_diff(diff);
        assert!(result.contains("a.rs"));
        assert!(result.contains("b.rs"));
    }

    #[test]
    fn test_condense_unified_diff_empty() {
        let result = condense_unified_diff("");
        assert!(result.is_empty());
    }

    // --- truncation accuracy ---

    fn make_large_unified_diff(added: usize, removed: usize) -> String {
        let mut lines = vec![
            "diff --git a/config.yaml b/config.yaml".to_string(),
            "--- a/config.yaml".to_string(),
            "+++ b/config.yaml".to_string(),
            "@@ -1,200 +1,200 @@".to_string(),
        ];
        for i in 0..removed {
            lines.push(format!("-old_value_{}", i));
        }
        for i in 0..added {
            lines.push(format!("+new_value_{}", i));
        }
        lines.join("\n")
    }

    #[test]
    fn test_condense_unified_diff_overflow_count_accuracy() {
        // 100 added + 100 removed = 200 total changes, only 10 shown
        // True overflow = 200 - 10 = 190
        // Bug: changes vec capped at 15, so old code showed "+5 more" (15-10) instead of "+190 more"
        let diff = make_large_unified_diff(100, 100);
        let result = condense_unified_diff(&diff);
        assert!(
            result.contains("+190 more"),
            "Expected '+190 more' but got:\n{}",
            result
        );
        assert!(
            !result.contains("+5 more"),
            "Bug still present: showing '+5 more' instead of true overflow"
        );
    }

    #[test]
    fn test_condense_unified_diff_no_false_overflow() {
        // 8 changes total — all fit within the 10-line display cap, no overflow message
        let diff = make_large_unified_diff(4, 4);
        let result = condense_unified_diff(&diff);
        assert!(
            !result.contains("more"),
            "No overflow message expected for 8 changes, got:\n{}",
            result
        );
    }

    #[test]
    fn test_no_truncation_large_diff() {
        // Verify compute_diff returns all changes without truncation
        let mut a = Vec::new();
        let mut b = Vec::new();
        for i in 0..500 {
            a.push(format!("line_{}", i));
            if i % 3 == 0 {
                b.push(format!("CHANGED_{}", i));
            } else {
                b.push(format!("line_{}", i));
            }
        }
        let a_refs: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
        let b_refs: Vec<&str> = b.iter().map(|s| s.as_str()).collect();
        let result = compute_diff(&a_refs, &b_refs);

        assert!(
            result.changes.len() > 100,
            "Expected 100+ changes, got {}",
            result.changes.len()
        );
        assert!(!result.changes.is_empty());
    }

    #[test]
    fn test_format_diff_shows_all_changes() {
        let mut a = Vec::new();
        let mut b = Vec::new();
        for i in 0..100 {
            a.push(format!("old_line_{}", i));
            b.push(format!("new_line_{}", i));
        }
        let a_refs: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
        let b_refs: Vec<&str> = b.iter().map(|s| s.as_str()).collect();
        let diff = compute_diff(&a_refs, &b_refs);
        let output = format_diff_changes(&diff);

        assert!(output.contains("old_line_0"), "should contain first change");
        assert!(output.contains("new_line_99"), "should contain last change");
    }

    #[test]
    fn test_long_lines_not_truncated() {
        let long_line = "x".repeat(500);
        let a = vec![long_line.as_str()];
        let b = vec!["short"];
        let result = compute_diff(&a, &b);
        match &result.changes[0] {
            DiffChange::Removed(_, content) | DiffChange::Added(_, content) => {
                assert_eq!(content.len(), 500, "Line was truncated!");
            }
            DiffChange::Modified(_, old, _) => {
                assert_eq!(old.len(), 500, "Line was truncated!");
            }
        }
    }
}
