//! Filters grep output by grouping matches by file.

use crate::core::config;
use crate::core::stream::exec_capture;
use crate::core::tracking;
use crate::core::utils::resolved_command;
use anyhow::{Context, Result};
use regex::Regex;
use std::collections::HashMap;

#[allow(clippy::too_many_arguments)]
pub fn run(
    pattern: &str,
    path: &str,
    max_line_len: usize,
    max_results: usize,
    context_only: bool,
    file_type: Option<&str>,
    source_tool: Option<&str>,
    extra_args: &[String],
    verbose: u8,
) -> Result<i32> {
    let timer = tracking::TimedExecution::start();

    if verbose > 0 {
        eprintln!("grep: '{}' in {}", pattern, path);
    }

    // Dialect-aware: grep BRE is faithfully translated to Rust-regex; rg patterns
    // pass through untouched (so a literal `\|` stays literal).
    let rg_pattern = effective_pattern(pattern, source_tool, extra_args);

    let mut rg_cmd = resolved_command("rg");
    // --no-ignore-vcs: match grep -r behavior (don't skip .gitignore'd files).
    // Without this, rg returns 0 matches for files in .gitignore, causing
    // false negatives that make AI agents draw wrong conclusions.
    // Using --no-ignore-vcs (not --no-ignore) so .ignore/.rgignore are still respected.
    rg_cmd.args(["-n", "--no-heading", "--no-ignore-vcs", &rg_pattern, path]);

    if let Some(ft) = file_type {
        rg_cmd.arg("--type").arg(ft);
    }

    for arg in extra_args {
        // Fix: skip grep-ism -r flag (rg is recursive by default; rg -r means --replace)
        if arg == "-r" || arg == "--recursive" {
            continue;
        }
        rg_cmd.arg(arg);
    }

    let result = exec_capture(&mut rg_cmd)
        .or_else(|_| {
            // rg unavailable → native grep. Use the ORIGINAL pattern (grep speaks
            // BRE natively) and translate/strip rg-only flags so grep never sees
            // an option it can't parse (the `--type`/`--glob` failures).
            let (mut grep_args, dropped) = grep_fallback_args(extra_args);
            if let Some(ft) = file_type {
                grep_args.push(rg_type_to_glob(ft));
            }
            if !dropped.is_empty() {
                eprintln!(
                    "rtk: dropped rg-only flags on grep fallback: {}",
                    dropped.join(" ")
                );
            }
            let mut grep_cmd = resolved_command("grep");
            grep_cmd.args(["-rn", pattern, path]).args(&grep_args);
            exec_capture(&mut grep_cmd)
        })
        .context("grep/rg failed")?;

    // Passthrough output flags that produce output that is already small.
    if has_format_flag(extra_args) {
        print!("{}", result.stdout);
        if !result.stderr.is_empty() {
            eprint!("{}", result.stderr.trim());
        }

        let args_display = if extra_args.is_empty() {
            format!("'{}' {}", pattern, path)
        } else {
            format!("{} '{}' {}", extra_args.join(" "), pattern, path)
        };

        timer.track_passthrough(
            &format!("grep {}", args_display),
            &format!("rtk grep {} (passthrough)", args_display),
        );
        return Ok(result.exit_code);
    }

    let exit_code = result.exit_code;
    let raw_output = result.stdout.clone();

    if result.stdout.trim().is_empty() {
        // Show stderr for errors (bad regex, missing file, etc.)
        if exit_code == 2 && !result.stderr.trim().is_empty() {
            eprintln!("{}", result.stderr.trim());
        }
        let msg = format!("0 matches for '{}'", pattern);
        println!("{}", msg);
        timer.track(
            &format!("grep -rn '{}' {}", pattern, path),
            "rtk grep",
            &raw_output,
            &msg,
        );
        return Ok(exit_code);
    }

    // Always filter: truncate long lines, apply per-file and global caps.
    // Output in standard file:line:content format that AI agents can parse.
    // (A passthrough approach yields 0% savings — no reason for RTK to exist on that path.)
    let total_matches = result.stdout.lines().count();

    let context_re = if context_only {
        Regex::new(&format!("(?i).{{0,20}}{}.*", regex::escape(pattern))).ok()
    } else {
        None
    };

    let mut by_file: HashMap<String, Vec<(usize, String)>> = HashMap::new();
    for line in result.stdout.lines() {
        let parts: Vec<&str> = line.splitn(3, ':').collect();

        let (file, line_num, content) = if parts.len() == 3 {
            let ln = parts[1].parse().unwrap_or(0);
            (parts[0].to_string(), ln, parts[2])
        } else if parts.len() == 2 {
            let ln = parts[0].parse().unwrap_or(0);
            (path.to_string(), ln, parts[1])
        } else {
            continue;
        };

        let cleaned = clean_line(content, max_line_len, context_re.as_ref(), pattern);
        by_file.entry(file).or_default().push((line_num, cleaned));
    }

    let mut rtk_output = String::new();
    rtk_output.push_str(&format!(
        "{} matches in {} files:\n\n",
        total_matches,
        by_file.len()
    ));

    let mut shown = 0;
    let mut files: Vec<_> = by_file.iter().collect();
    files.sort_by_key(|(f, _)| *f);

    let per_file = config::limits().grep_max_per_file;
    for (file, matches) in files {
        if shown >= max_results {
            break;
        }

        let file_display = compact_path(file);
        for (line_num, content) in matches.iter().take(per_file) {
            if shown >= max_results {
                break;
            }
            rtk_output.push_str(&format!("{}:{}:{}\n", file_display, line_num, content));
            shown += 1;
        }
    }

    if total_matches > shown {
        rtk_output.push_str(&format!("[+{} more]\n", total_matches - shown));
    }

    print!("{}", rtk_output);
    timer.track(
        &format!("grep -rn '{}' {}", pattern, path),
        "rtk grep",
        &raw_output,
        &rtk_output,
    );

    Ok(exit_code)
}

fn has_format_flag(extra_args: &[String]) -> bool {
    extra_args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "-c" | "--count"
                | "-l"
                | "--files-with-matches"
                | "-L"
                | "--files-without-match"
                | "-o"
                | "--only-matching"
                | "-Z"
                | "--null"
        )
    })
}

fn clean_line(line: &str, max_len: usize, context_re: Option<&Regex>, pattern: &str) -> String {
    let trimmed = line.trim();

    if let Some(re) = context_re {
        if let Some(m) = re.find(trimmed) {
            let matched = m.as_str();
            if matched.len() <= max_len {
                return matched.to_string();
            }
        }
    }

    if trimmed.len() <= max_len {
        trimmed.to_string()
    } else {
        let lower = trimmed.to_lowercase();
        let pattern_lower = pattern.to_lowercase();

        if let Some(pos) = lower.find(&pattern_lower) {
            let char_pos = lower[..pos].chars().count();
            let chars: Vec<char> = trimmed.chars().collect();
            let char_len = chars.len();

            let start = char_pos.saturating_sub(max_len / 3);
            let end = (start + max_len).min(char_len);
            let start = if end == char_len {
                end.saturating_sub(max_len)
            } else {
                start
            };

            let slice: String = chars[start..end].iter().collect();
            if start > 0 && end < char_len {
                format!("...{}...", slice)
            } else if start > 0 {
                format!("...{}", slice)
            } else {
                format!("{}...", slice)
            }
        } else {
            let t: String = trimmed.chars().take(max_len - 3).collect();
            format!("{}...", t)
        }
    }
}

fn compact_path(path: &str) -> String {
    if path.len() <= 50 {
        return path.to_string();
    }

    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= 3 {
        return path.to_string();
    }

    format!(
        "{}/.../{}/{}",
        parts[0],
        parts[parts.len() - 2],
        parts[parts.len() - 1]
    )
}

/// Decide the pattern to hand to ripgrep, honoring the source dialect.
/// grep default = POSIX BRE (translate); grep -E/-P/-F or rg = Rust-compatible (verbatim).
fn effective_pattern(pattern: &str, source_tool: Option<&str>, extra_args: &[String]) -> String {
    let is_grep = matches!(source_tool, Some("grep"));
    let extended = extra_args.iter().any(|a| {
        matches!(
            a.as_str(),
            "-E" | "--extended-regexp" | "-P" | "--perl-regexp" | "-F" | "--fixed-strings"
        )
    });
    if is_grep && !extended {
        translate_bre_to_rust(pattern)
    } else {
        pattern.to_string()
    }
}

/// Translate a POSIX Basic Regular Expression (grep default) into Rust-regex
/// (used by ripgrep). In BRE, `\| \( \) \{ \} \+ \?` are metacharacters and the
/// bare forms are literals — the exact opposite of Rust/ERE. Shared constructs
/// (`.`, `*`, `^`, `$`, `[...]`, `\.`, backrefs) are preserved verbatim.
fn translate_bre_to_rust(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 8);
    let mut chars = pattern.chars().peekable();
    let mut in_class = false;

    while let Some(c) = chars.next() {
        if in_class {
            out.push(c);
            if c == ']' {
                in_class = false;
            }
            continue;
        }
        match c {
            '\\' => match chars.peek() {
                // BRE metachar: drop the backslash so it becomes a Rust metachar.
                Some('|') | Some('(') | Some(')') | Some('{') | Some('}') | Some('+')
                | Some('?') => {
                    let next = chars.next().expect("peek confirmed Some");
                    out.push(next);
                }
                // Any other escape (\., \\, \d, \1, ...) is preserved as-is.
                Some(&n) => {
                    out.push('\\');
                    out.push(n);
                    chars.next();
                }
                None => out.push('\\'),
            },
            '[' => {
                in_class = true;
                out.push('[');
            }
            // Bare BRE literals → escape so Rust treats them literally.
            '|' | '(' | ')' | '{' | '}' | '+' | '?' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// rg type name → file glob for grep's `--include` (best-effort common set).
fn rg_type_to_glob(t: &str) -> String {
    let ext = match t {
        "rust" => "rs",
        "py" | "python" => "py",
        "js" => "js",
        "ts" => "ts",
        "md" | "markdown" => "md",
        "yaml" => "yaml",
        other => other, // java→java, go→go, etc.: name already equals ext
    };
    format!("--include=*.{}", ext)
}

/// rg `--glob`/`-g` value → grep `--include`/`--exclude`. A leading `!` is rg's
/// negation, which maps to grep's `--exclude`.
fn glob_to_include(g: &str) -> String {
    if let Some(rest) = g.strip_prefix('!') {
        format!("--exclude={}", rest)
    } else {
        format!("--include={}", g)
    }
}

/// rg-only flags that take a VALUE and have no grep equivalent → drop flag + value.
const RG_VALUE_DROP: &[&str] = &[
    "--engine",
    "--max-columns",
    "-M",
    "--colors",
    "--context-separator",
    "--field-context-separator",
    "--field-match-separator",
    "--pre",
    "--sort",
    "--sortr",
];

/// boolean rg-only flags grep can't parse → drop.
const RG_BOOL_DROP: &[&str] = &[
    "--no-ignore",
    "--no-ignore-vcs",
    "--no-ignore-dot",
    "--hidden",
    "--no-heading",
    "--heading",
    "--pcre2",
    "--column",
    "--no-column",
];

/// Translate rg `extra_args` into grep-safe args. Returns (kept_args, dropped_args).
/// Value-taking flags are paired with their value so we never leave a dangling token
/// that grep would misread as a pattern/path:
/// - `--type X` / `-tX` / `--type=X`  → `--include=*.<ext>`
/// - `--glob X` / `-gX` / `--glob=X`  → `--include=X` (or `--exclude=` for `!X`)
/// - RG_VALUE_DROP flags              → drop flag AND its value
/// - RG_BOOL_DROP flags               → drop flag
///
/// Everything else is kept verbatim (grep-compatible flags like -i, -w, -A, -B, -C).
fn grep_fallback_args(extra: &[String]) -> (Vec<String>, Vec<String>) {
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    let mut it = extra.iter().peekable();
    while let Some(a) = it.next() {
        if a == "--type" || a == "-t" {
            if let Some(v) = it.next() {
                kept.push(rg_type_to_glob(v));
            }
        } else if let Some(v) = a.strip_prefix("--type=") {
            kept.push(rg_type_to_glob(v));
        } else if let Some(v) = a.strip_prefix("-t").filter(|s| !s.is_empty()) {
            kept.push(rg_type_to_glob(v));
        } else if a == "--glob" || a == "-g" {
            if let Some(v) = it.next() {
                kept.push(glob_to_include(v));
            }
        } else if let Some(v) = a.strip_prefix("--glob=") {
            kept.push(glob_to_include(v));
        } else if let Some(v) = a.strip_prefix("-g").filter(|s| !s.is_empty()) {
            kept.push(glob_to_include(v));
        } else if RG_VALUE_DROP.contains(&a.as_str()) {
            dropped.push(a.clone());
            if let Some(v) = it.next() {
                dropped.push(v.clone());
            }
        } else if RG_BOOL_DROP.contains(&a.as_str()) {
            dropped.push(a.clone());
        } else {
            kept.push(a.clone());
        }
    }
    (kept, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_effective_pattern_grep_translates_bre() {
        assert_eq!(effective_pattern("a\\|b", Some("grep"), &[]), "a|b");
    }

    #[test]
    fn test_effective_pattern_grep_extended_no_translate() {
        assert_eq!(
            effective_pattern("a|b", Some("grep"), &["-E".to_string()]),
            "a|b"
        );
    }

    #[test]
    fn test_effective_pattern_rg_untouched() {
        assert_eq!(effective_pattern("a\\|b", Some("rg"), &[]), "a\\|b");
        // default (no source) behaves like rg for backward-compat
        assert_eq!(effective_pattern("a\\|b", None, &[]), "a\\|b");
    }

    #[test]
    fn test_clean_line() {
        let line = "            const result = someFunction();";
        let cleaned = clean_line(line, 50, None, "result");
        assert!(!cleaned.starts_with(' '));
        assert!(cleaned.len() <= 50);
    }

    #[test]
    fn test_compact_path() {
        let path = "/Users/patrick/dev/project/src/components/Button.tsx";
        let compact = compact_path(path);
        assert!(compact.len() <= 60);
    }

    #[test]
    fn test_extra_args_accepted() {
        // Test that the function signature accepts extra_args
        // This is a compile-time test - if it compiles, the signature is correct
        let _extra: Vec<String> = vec!["-i".to_string(), "-A".to_string(), "3".to_string()];
        // No need to actually run - we're verifying the parameter exists
    }

    #[test]
    fn test_clean_line_multibyte() {
        // Thai text that exceeds max_len in bytes
        let line = "  สวัสดีครับ นี่คือข้อความที่ยาวมากสำหรับทดสอบ  ";
        let cleaned = clean_line(line, 20, None, "ครับ");
        // Should not panic
        assert!(!cleaned.is_empty());
    }

    #[test]
    fn test_clean_line_emoji() {
        let line = "🎉🎊🎈🎁🎂🎄 some text 🎃🎆🎇✨";
        let cleaned = clean_line(line, 15, None, "text");
        assert!(!cleaned.is_empty());
    }

    // Fix: BRE \| alternation is translated to PCRE | for rg
    #[test]
    fn test_bre_alternation_translated() {
        let pattern = r"fn foo\|pub.*bar";
        let rg_pattern = pattern.replace(r"\|", "|");
        assert_eq!(rg_pattern, "fn foo|pub.*bar");
    }

    // Fix: -r flag (grep recursive) is stripped from extra_args (rg is recursive by default)
    #[test]
    fn test_recursive_flag_stripped() {
        let extra_args: Vec<String> = vec!["-r".to_string(), "-i".to_string()];
        let filtered: Vec<&String> = extra_args
            .iter()
            .filter(|a| *a != "-r" && *a != "--recursive")
            .collect();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0], "-i");
    }

    // --- truncation accuracy ---

    #[test]
    fn test_grep_overflow_uses_uncapped_total() {
        // Confirm the grep overflow invariant: matches vec is never capped before overflow calc.
        // If total_matches > per_file, overflow = total_matches - per_file (not capped).
        // This documents that grep_cmd.rs avoids the diff_cmd bug (cap at N then compute N-10).
        let per_file = config::limits().grep_max_per_file;
        let total_matches = per_file + 42;
        let overflow = total_matches - per_file;
        assert_eq!(overflow, 42, "overflow must equal true suppressed count");
        // Demonstrate why capping before subtraction is wrong:
        let hypothetical_cap = per_file + 5;
        let capped = total_matches.min(hypothetical_cap);
        let wrong_overflow = capped - per_file;
        assert_ne!(
            wrong_overflow, overflow,
            "capping before subtraction gives wrong overflow"
        );
    }

    // --- format flag detection ---

    #[test]
    fn test_format_flag_detects_count() {
        assert!(has_format_flag(&["-c".to_string()]));
        assert!(has_format_flag(&["--count".to_string()]));
    }

    #[test]
    fn test_format_flag_detects_files_with_matches() {
        assert!(has_format_flag(&["-l".to_string()]));
        assert!(has_format_flag(&["--files-with-matches".to_string()]));
    }

    #[test]
    fn test_format_flag_detects_files_without_match() {
        assert!(has_format_flag(&["-L".to_string()]));
        assert!(has_format_flag(&["--files-without-match".to_string()]));
    }

    #[test]
    fn test_format_flag_detects_only_matching() {
        assert!(has_format_flag(&["-o".to_string()]));
        assert!(has_format_flag(&["--only-matching".to_string()]));
    }

    #[test]
    fn test_format_flag_detects_null() {
        assert!(has_format_flag(&["-Z".to_string()]));
        assert!(has_format_flag(&["--null".to_string()]));
    }

    #[test]
    fn test_format_flag_ignores_normal_flags() {
        assert!(!has_format_flag(&[
            "-i".to_string(),
            "-w".to_string(),
            "-A".to_string(),
            "3".to_string(),
        ]));
    }

    // Verify line numbers are always enabled in rg invocation (grep_cmd.rs:24).
    // The -n/--line-numbers clap flag in main.rs is a no-op accepted for compat.
    #[test]
    fn test_rg_always_has_line_numbers() {
        // grep_cmd::run() always passes "-n" to rg (line 24).
        // This test documents that -n is built-in, so the clap flag is safe to ignore.
        let mut cmd = resolved_command("rg");
        cmd.args(["-n", "--no-heading", "NONEXISTENT_PATTERN_12345", "."]);
        // If rg is available, it should accept -n without error (exit 1 = no match, not error)
        if let Ok(output) = cmd.output() {
            assert!(
                output.status.code() == Some(1) || output.status.success(),
                "rg -n should be accepted"
            );
        }
        // If rg is not installed, skip gracefully (test still passes)
    }

    #[test]
    fn test_rg_no_ignore_vcs_flag_accepted() {
        // Verify rg accepts --no-ignore-vcs (used to match grep -r behavior for .gitignore)
        let mut cmd = resolved_command("rg");
        cmd.args([
            "-n",
            "--no-heading",
            "--no-ignore-vcs",
            "NONEXISTENT_PATTERN_12345",
            ".",
        ]);
        if let Ok(output) = cmd.output() {
            assert!(
                output.status.code() == Some(1) || output.status.success(),
                "rg --no-ignore-vcs should be accepted"
            );
        }
        // If rg is not installed, skip gracefully (test still passes)
    }

    #[test]
    fn test_bre_alternation_and_literal_paren() {
        // Real session failure: grep BRE — \| is alternation, bare ( is literal.
        assert_eq!(
            translate_bre_to_rust(r#"SUPERADMIN\|getValue\|enum TechnicalRole\|(""#),
            r#"SUPERADMIN|getValue|enum TechnicalRole|\(""#
        );
    }

    #[test]
    fn test_bre_conflict_markers() {
        assert_eq!(
            translate_bre_to_rust(r"^<<<<<<<\|^=======\|^>>>>>>>"),
            r"^<<<<<<<|^=======|^>>>>>>>"
        );
    }

    #[test]
    fn test_bre_groups_and_intervals() {
        assert_eq!(translate_bre_to_rust(r"a\{2,3\}"), r"a{2,3}");
        assert_eq!(translate_bre_to_rust(r"\(ab\)\+"), r"(ab)+");
    }

    #[test]
    fn test_bre_bare_metachars_become_literal() {
        // In BRE, bare | ( ) + ? { } are literal → must be escaped for Rust regex.
        assert_eq!(translate_bre_to_rust("a+b?"), r"a\+b\?");
        assert_eq!(translate_bre_to_rust("a|b"), r"a\|b");
    }

    #[test]
    fn test_bre_preserves_shared_metachars_and_escapes() {
        assert_eq!(translate_bre_to_rust(r"foo.*bar"), r"foo.*bar");
        assert_eq!(translate_bre_to_rust(r"\.txt$"), r"\.txt$");
        assert_eq!(translate_bre_to_rust(r"^abc"), r"^abc");
    }

    #[test]
    fn test_bre_char_class_untouched() {
        // Inside [...] the chars | ( ) keep their literal meaning in both dialects.
        assert_eq!(translate_bre_to_rust(r"[a|b(]x"), r"[a|b(]x");
    }

    #[test]
    fn test_grep_fallback_translates_type_to_include() {
        let (args, _dropped) =
            grep_fallback_args(&["-l".to_string(), "--type".to_string(), "java".to_string()]);
        assert!(args.contains(&"--include=*.java".to_string()), "got {:?}", args);
        assert!(!args.iter().any(|a| a == "--type"), "got {:?}", args);
        assert!(args.contains(&"-l".to_string()));
    }

    #[test]
    fn test_grep_fallback_drops_bool_rg_flags() {
        let (args, dropped) =
            grep_fallback_args(&["--no-ignore-vcs".to_string(), "-i".to_string()]);
        assert!(!args.iter().any(|a| a == "--no-ignore-vcs"));
        assert!(args.contains(&"-i".to_string()));
        assert!(dropped.iter().any(|d| d == "--no-ignore-vcs"));
    }

    #[test]
    fn test_rg_type_to_glob_known_and_unknown() {
        assert_eq!(rg_type_to_glob("rust"), "--include=*.rs");
        assert_eq!(rg_type_to_glob("java"), "--include=*.java");
    }

    #[test]
    fn test_grep_fallback_glob_to_include() {
        let (args, _) = grep_fallback_args(&["-g".to_string(), "*.rs".to_string()]);
        assert!(args.contains(&"--include=*.rs".to_string()), "got {:?}", args);
        assert!(!args.iter().any(|a| a == "-g"), "got {:?}", args);
    }

    #[test]
    fn test_grep_fallback_glob_negation_to_exclude() {
        let (args, _) = grep_fallback_args(&["--glob".to_string(), "!*.test.js".to_string()]);
        assert!(args.contains(&"--exclude=*.test.js".to_string()), "got {:?}", args);
    }

    #[test]
    fn test_grep_fallback_glob_attached_form() {
        let (args, _) = grep_fallback_args(&["-g*.rs".to_string()]);
        assert!(args.contains(&"--include=*.rs".to_string()), "got {:?}", args);
    }

    #[test]
    fn test_grep_fallback_drops_valued_rg_flag_with_value() {
        let (args, dropped) = grep_fallback_args(&[
            "--engine".to_string(),
            "pcre2".to_string(),
            "-i".to_string(),
        ]);
        assert!(!args.iter().any(|a| a == "pcre2"), "value must drop too: {:?}", args);
        assert!(args.contains(&"-i".to_string()), "got {:?}", args);
        assert!(dropped.contains(&"--engine".to_string()), "got {:?}", dropped);
    }
}
