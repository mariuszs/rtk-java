//! Fork-only: keep a read-only command plain where Claude Code's worktree
//! isolation would refuse its rtk rewrite.
//!
//! A session isolated in a worktree (`claude -w`, `.claude/worktrees/<name>`)
//! refuses any command that "cannot be shown not to be git". It runs `ls`,
//! `grep`, `find` … plain without objection, but `rtk` can run any program
//! (`rtk proxy`, `rtk git`), so it refuses an rtk call whose operands could
//! turn out to be git: a `$…` expansion, a glob that could expand to `git`, or
//! any text inside a loop. Measured 2026-09-07..10-07: 490 refusals in 243
//! worktree sessions, each a wasted turn, for commands that pass plain.
//!
//! Here a read-only segment of that shape stays plain and everything else is
//! rewritten as usual. Maven included: the isolation refuses a plain `./mvnw`
//! with a `$…` operand just the same, so keeping it plain would only lose the
//! filter.

use regex::Regex;
use std::path::Path;
use std::sync::LazyLock;

use crate::discover::registry::{
    ExcludePattern, compile_exclude_patterns, normalize_transparent_prefixes,
    rewrite_command_precompiled,
};

/// Programs the isolation runs plain: it can show them not to be git.
const READ_ONLY: &[&str] = &[
    "ls", "grep", "egrep", "fgrep", "rg", "find", "du", "stat", "tree", "wc",
];

/// A loop keyword in command position. The isolation calls any loop "a
/// construct too complex to verify" and refuses every rtk call inside it.
static LOOP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[;&|(\n])\s*(?:for|while|until|select)\s").expect("valid loop regex")
});

/// Whether the hook runs in a session isolated in a Claude Code worktree.
pub(crate) fn session_is_isolated() -> bool {
    crate::core::user_dirs::working_dir().is_some_and(|dir| in_worktree(&dir))
}

fn in_worktree(dir: &Path) -> bool {
    let names: Vec<_> = dir.components().map(|c| c.as_os_str()).collect();
    names
        .windows(2)
        .any(|pair| pair[0] == ".claude" && pair[1] == "worktrees")
}

/// [`crate::discover::registry::rewrite_command`] for an isolated session:
/// the same rewrite, except that a segment [`keep_plain`] refuses is left as
/// the agent wrote it.
pub(crate) fn rewrite_command(
    cmd: &str,
    excluded: &[String],
    transparent_prefixes: &[String],
) -> Option<String> {
    let mut compiled = compile_exclude_patterns(excluded);
    compiled.push(ExcludePattern::WorktreeGuard {
        in_loop: LOOP_RE.is_match(cmd),
    });
    rewrite_command_precompiled(
        cmd,
        &compiled,
        &normalize_transparent_prefixes(transparent_prefixes),
    )
}

/// Whether the isolation would refuse `segment` rewritten to rtk while it
/// runs it plain: a read-only program inside a loop, or with an operand that
/// could turn out to be git.
pub(crate) fn keep_plain(segment: &str, in_loop: bool) -> bool {
    let words = words(segment);
    let mut rest = words
        .iter()
        .skip_while(|w| w.is_assignment() || w.is_keyword());
    let Some(program) = rest.next() else {
        return false;
    };
    let text = program.text();
    let name = text.rsplit('/').next().unwrap_or(&text);
    if !READ_ONLY.contains(&name) {
        return false;
    }
    in_loop || rest.any(|w| w.expands || w.glob_could_be_git())
}

/// One shell word: its characters, each marked when it is an unquoted glob
/// character, and whether a `$…` or backtick expansion feeds it.
#[derive(Default)]
struct Word {
    chars: Vec<(char, bool)>,
    expands: bool,
}

impl Word {
    fn text(&self) -> String {
        self.chars.iter().map(|&(c, _)| c).collect()
    }

    fn is_assignment(&self) -> bool {
        let text = self.text();
        text.split_once('=').is_some_and(|(name, _)| {
            name.chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
    }

    fn is_keyword(&self) -> bool {
        matches!(self.text().as_str(), "do" | "then" | "else" | "elif" | "!")
    }

    /// Whether the last path component, as a glob, matches `git`.
    fn glob_could_be_git(&self) -> bool {
        if !self.chars.iter().any(|&(_, glob)| glob) {
            return false;
        }
        let start = self
            .chars
            .iter()
            .rposition(|&(c, glob)| c == '/' && !glob)
            .map_or(0, |i| i + 1);
        let name: Vec<char> = "git".chars().collect();
        glob_matches(&self.chars[start..], &name)
    }
}

/// Split `segment` into words the way bash quotes them.
fn words(segment: &str) -> Vec<Word> {
    let chars: Vec<char> = segment.chars().collect();
    let mut out = Vec::new();
    let mut cur = Word::default();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        i += 1;
        match quote {
            Some('\'') if c == '\'' => quote = None,
            Some('\'') => cur.chars.push((c, false)),
            Some(_) if c == '"' => quote = None,
            Some(_) if c == '\\' && next.is_some() => {
                cur.chars.push((chars[i], false));
                i += 1;
            }
            Some(_) => {
                cur.expands |= starts_expansion(c, next);
                cur.chars.push((c, false));
            }
            None => {
                started = true;
                match c {
                    '\'' | '"' => quote = Some(c),
                    '\\' if next.is_some() => {
                        cur.chars.push((chars[i], false));
                        i += 1;
                    }
                    ' ' | '\t' | '\n' => {
                        if !cur.chars.is_empty() || cur.expands {
                            out.push(std::mem::take(&mut cur));
                        }
                        started = false;
                    }
                    '*' | '?' | '[' => cur.chars.push((c, true)),
                    _ => {
                        cur.expands |= starts_expansion(c, next) || c == '$' && next == Some('\'');
                        cur.chars.push((c, false));
                    }
                }
            }
        }
    }
    if started || !cur.chars.is_empty() {
        out.push(cur);
    }
    out
}

/// Whether `c` followed by `next` opens a bash expansion.
fn starts_expansion(c: char, next: Option<char>) -> bool {
    match c {
        '`' => true,
        '$' => next.is_some_and(|n| {
            n.is_ascii_alphanumeric()
                || matches!(n, '_' | '{' | '(' | '@' | '*' | '#' | '?' | '!' | '-' | '$')
        }),
        _ => false,
    }
}

/// Bash glob matching: `*`, `?` and `[…]` (ranges, `!`/`^` negation) when
/// marked as glob characters, everything else literal.
fn glob_matches(pattern: &[(char, bool)], name: &[char]) -> bool {
    let Some((&(c, glob), rest)) = pattern.split_first() else {
        return name.is_empty();
    };
    match (c, glob) {
        ('*', true) => (0..=name.len()).any(|i| glob_matches(rest, &name[i..])),
        ('?', true) => !name.is_empty() && glob_matches(rest, &name[1..]),
        ('[', true) => match class_end(rest) {
            Some(end) => {
                name.first()
                    .is_some_and(|&n| class_matches(&rest[..end], n))
                    && glob_matches(&rest[end + 1..], &name[1..])
            }
            None => name.first() == Some(&'[') && glob_matches(rest, &name[1..]),
        },
        _ => name.first() == Some(&c) && glob_matches(rest, &name[1..]),
    }
}

/// Index of the `]` closing a bracket class; a `]` first in the class is a
/// member, as in bash.
fn class_end(body: &[(char, bool)]) -> Option<usize> {
    let skip = match body.first() {
        Some(&('!' | '^', _)) => 2,
        _ => 1,
    };
    body.iter()
        .enumerate()
        .skip(skip)
        .find(|&(_, &(c, _))| c == ']')
        .map(|(i, _)| i)
}

fn class_matches(body: &[(char, bool)], n: char) -> bool {
    let (negated, body) = match body.first() {
        Some(&('!' | '^', _)) => (true, &body[1..]),
        _ => (false, body),
    };
    let chars: Vec<char> = body.iter().map(|&(c, _)| c).collect();
    let mut hit = false;
    let mut i = 0;
    while i < chars.len() {
        if i + 2 < chars.len() && chars[i + 1] == '-' {
            hit |= (chars[i]..=chars[i + 2]).contains(&n);
            i += 3;
        } else {
            hit |= chars[i] == n;
            i += 1;
        }
    }
    hit != negated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_is_a_dot_claude_worktrees_directory() {
        assert!(in_worktree(Path::new("/home/u/repo/.claude/worktrees/x")));
        assert!(in_worktree(Path::new(
            "/home/u/repo/.claude/worktrees/x/src/main"
        )));
        assert!(!in_worktree(Path::new("/home/u/repo")));
        assert!(!in_worktree(Path::new("/home/u/worktrees/x")));
        assert!(!in_worktree(Path::new("/home/u/repo/.claude")));
    }

    #[test]
    fn glob_that_could_expand_to_git_stays_plain() {
        assert!(keep_plain("ls -la docs/diagrams/*", false));
        assert!(keep_plain("wc -l docs/diagrams/*", false));
        assert!(keep_plain(
            "grep -l -i personio src/test/resources/store/*",
            false
        ));
        assert!(keep_plain("ls src/[a-h]*", false));
        assert!(keep_plain("ls g?t", false));
        assert!(keep_plain("FOO=1 ls *", false));
    }

    #[test]
    fn glob_that_cannot_be_git_is_rewritten() {
        assert!(!keep_plain("ls *.md", false));
        assert!(!keep_plain("grep -rn foo --include=*.java src", false));
        assert!(!keep_plain("ls src/[a-c]*", false));
        assert!(!keep_plain("ls src/[!g]*", false));
        assert!(!keep_plain("find . -name '*'", false));
        assert!(!keep_plain(r"ls src/\*", false));
    }

    #[test]
    fn expansion_stays_plain_literal_dollar_does_not() {
        assert!(keep_plain(r#"grep -n "$PAT" f"#, false));
        assert!(keep_plain("grep -rn x ${DIR}/src", false));
        assert!(keep_plain(r"grep -c $'\t' brief.md", false));
        assert!(keep_plain("ls `pwd`", false));
        assert!(!keep_plain("grep -n 'end$' f", false));
        assert!(!keep_plain(r#"grep -n "end$" f"#, false));
        assert!(!keep_plain("grep -n '$HOME' f", false));
    }

    #[test]
    fn loop_keeps_every_read_only_segment_plain() {
        assert!(keep_plain(r#"grep -n "ImportResult" src/Foo.java"#, true));
        assert!(keep_plain("do grep -n x src", true));
        assert!(!keep_plain("./mvnw test", true));
    }

    #[test]
    fn non_read_only_programs_are_never_kept_plain() {
        assert!(!keep_plain("./mvnw test -Dtest=$T", false));
        assert!(!keep_plain("cargo test *", false));
        assert!(!keep_plain("gh pr view $PR", false));
        assert!(!keep_plain("", false));
    }

    #[test]
    fn loop_detection_needs_command_position() {
        assert!(LOOP_RE.is_match("for f in a b; do grep x $f; done"));
        assert!(LOOP_RE.is_match("echo a; while true; do ls; done"));
        assert!(!LOOP_RE.is_match("grep for x src"));
        assert!(!LOOP_RE.is_match("grep -rn 'until ' src"));
    }

    #[test]
    fn rewrite_keeps_refused_segments_and_rewrites_the_rest() {
        let none: [String; 0] = [];
        assert_eq!(
            rewrite_command("ls -la docs/ && ls docs/*", &none, &none).as_deref(),
            Some("rtk ls -la docs/ && ls docs/*")
        );
        assert_eq!(
            rewrite_command("grep -rn foo src", &none, &none).as_deref(),
            Some("rtk grep -rn foo src")
        );
        assert_eq!(
            rewrite_command("./mvnw test -Dtest=$T", &none, &none).as_deref(),
            Some("rtk mvn test -Dtest=$T")
        );
        assert_eq!(rewrite_command("ls src/*", &none, &none), None);
        assert_eq!(
            rewrite_command("for f in a b; do grep -n x $f; done", &none, &none),
            None
        );
    }

    #[test]
    fn only_an_isolated_session_keeps_the_segment_plain() {
        use crate::hooks::decision::{HookDecision, decide_in_session};
        use crate::hooks::permissions::PermissionVerdict;

        let none: [String; 0] = [];
        assert_eq!(
            decide_in_session("ls src/*", PermissionVerdict::Default, &none, &none, true),
            HookDecision::Defer
        );
        assert_eq!(
            decide_in_session("ls src/*", PermissionVerdict::Default, &none, &none, false),
            HookDecision::AskRewrite("rtk ls src/*".into())
        );
    }
}
