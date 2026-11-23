//! Command-level facet matching for Bash permission rules.
//!
//! Pure functions, no I/O (apart from the sandbox classification delegated to
//! [`WorkspacePaths`]). The DSL parser in `rule_match.rs` decides how a rule
//! string is parsed; this module decides how a *command text* is matched
//! against the argument pattern of such a rule (`Bash(git *)` → `git *`).
//!
//! Pattern semantics are [`globset`] semantics, identical to the tool-name
//! matching in `rule_match.rs` (`*` crosses `/`, no negation support).
//!
//! Segmentation is deliberately shallow: no quote state machine and no
//! command-substitution parsing. A segment that contains a substitution is
//! [`CommandSegment::Unparsed`] and conservatively never matches (the call
//! falls through to an ask, it is never denied by this module).

use std::path::{Path, PathBuf};

use globset::Glob;

use crate::tools::{PathClassification, WorkspacePaths};

/// One `&&` / `||` / `;` / `|` / newline separated piece of a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandSegment {
    /// A whitespace-normalized segment that can be matched against patterns.
    Resolved(String),
    /// A segment that cannot be reasoned about statically (command
    /// substitution). Never matches any pattern.
    Unparsed,
}

/// Escape `command` so it matches only itself when used as a glob pattern.
///
/// Glob metacharacters and backslashes are backslash-escaped; a leading `!`
/// is escaped too so the facet can never be read as a negated pattern.
pub fn escape_facet(command: &str) -> String {
    let mut out = String::with_capacity(command.len() + 2);
    for (i, c) in command.chars().enumerate() {
        let needs_escape = matches!(c, '*' | '?' | '[' | ']' | '{' | '}' | '(' | ')' | '\\')
            || (i == 0 && c == '!');
        if needs_escape {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Remove glob escapes: a backslash makes the next character literal.
fn unescape(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn normalize_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Split into raw (untrimmed) segment texts. Two-character operators are
/// consumed before the single-character `|`, so `a || b` yields two segments.
fn split_raw(command: &str) -> Vec<&str> {
    let bytes = command.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let prev = if i > 0 { bytes[i - 1] } else { b' ' };
        let next = bytes.get(i + 1).copied();
        let op_len = match bytes[i] {
            b'&' if next == Some(b'&') => 2,
            // Redirect forms `>&`, `<&`, `&>` are not separators.
            b'&' if matches!(prev, b'>' | b'<') || next == Some(b'>') => 0,
            // A lone `&` backgrounds the head; the tail is a separate command.
            b'&' => 1,
            b'|' if matches!(next, Some(b'|') | Some(b'&')) => 2, // `||`, `|&`
            b'|' | b';' | b'\n' => 1,
            _ => 0,
        };
        if op_len == 0 {
            i += 1;
        } else {
            parts.push(&command[start..i]);
            i += op_len;
            start = i;
        }
    }
    parts.push(&command[start..]);
    parts.into_iter().filter(|p| !p.trim().is_empty()).collect()
}

fn has_substitution(segment: &str) -> bool {
    segment.contains('`') || ["$(", "${", "<(", ">("].iter().any(|p| segment.contains(p))
}

/// Split `command` into segments; empty segments are dropped.
pub fn split_command_segments(command: &str) -> Vec<CommandSegment> {
    split_raw(command)
        .into_iter()
        .map(|raw| {
            if has_substitution(raw) {
                CommandSegment::Unparsed
            } else {
                CommandSegment::Resolved(normalize_whitespace(raw))
            }
        })
        .collect()
}

fn glob_matches(pattern: &str, text: &str) -> bool {
    Glob::new(pattern)
        .map(|g| g.compile_matcher().is_match(text))
        .unwrap_or(false)
}

/// Whether `command` is covered by the rule argument `pattern`.
///
/// Hits when the whole command equals the (unescaped) pattern, or when every
/// segment matches the pattern and none is [`CommandSegment::Unparsed`].
pub fn command_matches_pattern(command: &str, pattern: &str) -> bool {
    let normalized = normalize_whitespace(command);
    if normalized.is_empty() {
        return false;
    }
    if normalized == normalize_whitespace(&unescape(pattern)) {
        return true;
    }
    let segments = split_command_segments(command);
    !segments.is_empty()
        && segments.iter().all(|seg| match seg {
            CommandSegment::Resolved(text) => glob_matches(pattern, text),
            CommandSegment::Unparsed => false,
        })
}

fn unquote(token: &str) -> &str {
    let t = token.trim();
    for q in ['"', '\''] {
        if t.len() >= 2 && t.starts_with(q) && t.ends_with(q) {
            return &t[1..t.len() - 1];
        }
    }
    t
}

/// Recognize `cd` / `chdir` / `pushd` (also behind `builtin` / `command`) and
/// return the statically known target.
///
/// Returns `None` when the segment is not a directory switch, has no target
/// (`cd` alone), uses `cd -`, or the target contains an expansion (`$`,
/// backtick, glob characters). A `~`-prefixed target is returned verbatim;
/// callers must not treat it as workspace-relative.
///
/// **Backslash is platform-dependent.** On POSIX it introduces an escape
/// sequence (`cd foo\ bar`), so a token carrying one is not a static target
/// and is declined. On Windows it is *the* path separator: declining it there
/// declined every absolute path, which silently disabled the whole
/// outside-directory review — `cd C:\somewhere\outside && ...` returned `None`
/// and [`BashExecutionDirectoryAskPolicy`] never asked. The Bash tool layer
/// performs no cwd validation of its own, so on Windows that ask is the only
/// review such a command ever gets.
///
/// [`BashExecutionDirectoryAskPolicy`]: crate::permission::policies::BashExecutionDirectoryAskPolicy
pub fn extract_directory_switch(segment: &str) -> Option<PathBuf> {
    let mut tokens = segment.split_whitespace().peekable();
    while matches!(tokens.peek(), Some(&"builtin") | Some(&"command")) {
        tokens.next();
    }
    if !matches!(tokens.next()?, "cd" | "chdir" | "pushd") {
        return None;
    }
    let mut target = None;
    for token in tokens {
        if target.is_none() && token.starts_with('-') && token != "-" {
            continue; // options such as -P, -L, --
        }
        if target.is_some() {
            return None; // extra arguments: not a plain switch
        }
        target = Some(unquote(token));
    }
    let target = target?;
    // The expansion/glob metacharacters are shell syntax on every platform.
    // The backslash is only an escape introducer on POSIX — see the doc above.
    let metacharacters: &[char] = if cfg!(windows) {
        &['$', '`', '*', '?', '[', '{']
    } else {
        &['$', '`', '*', '?', '[', '{', '\\']
    };
    if target.is_empty() || target == "-" || target.contains(metacharacters) {
        return None;
    }
    Some(PathBuf::from(target))
}

fn is_static_workspace_dir(target: &Path, workspace: &WorkspacePaths) -> bool {
    if target.to_string_lossy().starts_with('~') {
        return false;
    }
    matches!(workspace.classify(target), PathClassification::Inside(_))
}

/// Drop statically resolvable, in-workspace directory-switch segments so that
/// `cd src && git status` is matched as `git status`. Remaining segments are
/// re-joined with ` && `; an all-`cd` command yields an empty string. The
/// result is for rule matching only — the original separators are lost, so it
/// must never be executed.
pub fn strip_resolved_dir_segments(command: &str, workspace: &WorkspacePaths) -> String {
    split_raw(command)
        .into_iter()
        .filter(|raw| {
            has_substitution(raw)
                || !extract_directory_switch(raw)
                    .is_some_and(|target| is_static_workspace_dir(&target, workspace))
        })
        .map(normalize_whitespace)
        .collect::<Vec<_>>()
        .join(" && ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_hit_the_whole_command_when_the_pattern_equals_it() {
        assert!(command_matches_pattern("git status", "git status"));
    }

    #[test]
    fn should_miss_when_only_the_first_word_matches() {
        assert!(!command_matches_pattern("git push", "git"));
    }

    #[test]
    fn should_hit_when_every_segment_matches_a_wildcard_rule() {
        assert!(command_matches_pattern("git status && git log", "git *"));
    }

    #[test]
    fn should_miss_when_a_segment_is_not_covered() {
        assert!(!command_matches_pattern(
            "git status && rm -rf /",
            "git status"
        ));
    }

    #[test]
    fn should_split_pipe_or_before_single_pipe() {
        assert_eq!(
            split_command_segments("a || b"),
            vec![
                CommandSegment::Resolved("a".into()),
                CommandSegment::Resolved("b".into())
            ]
        );
        assert_eq!(split_command_segments("a | b ; c\nd").len(), 4);
    }

    #[test]
    fn should_treat_a_segment_with_command_substitution_as_unmatched() {
        assert_eq!(
            split_command_segments("echo $(rm x)"),
            vec![CommandSegment::Unparsed]
        );
        assert!(!command_matches_pattern("git `rm x`", "git *"));
    }

    #[test]
    fn should_escape_glob_metacharacters_when_generating_a_facet() {
        let facet = escape_facet("rm *");
        assert!(command_matches_pattern("rm *", &facet));
        assert!(!command_matches_pattern("rm -rf /", &facet));
    }

    #[test]
    fn should_not_turn_a_facet_into_a_negation_when_the_command_starts_with_a_bang() {
        let facet = escape_facet("!x");
        assert!(facet.starts_with("\\!"));
        assert!(command_matches_pattern("!x", &facet));
        assert!(!command_matches_pattern("ls", &facet));
    }

    #[test]
    fn should_split_a_single_ampersand_so_a_backgrounded_tail_cannot_ride_along() {
        assert!(!command_matches_pattern("sleep 1 & rm -rf x", "sleep *"));
        assert!(command_matches_pattern("sleep 1 & sleep 2", "sleep *"));
        // Redirect forms stay one segment.
        assert_eq!(split_command_segments("ls 2>&1").len(), 1);
        assert_eq!(split_command_segments("ls &> out").len(), 1);
        assert_eq!(split_command_segments("a |& b").len(), 2);
    }

    #[test]
    fn should_treat_process_substitution_as_unmatched() {
        for cmd in ["cat <(rm x)", "tee >(rm x)", "echo ${IFS}x"] {
            assert_eq!(split_command_segments(cmd), vec![CommandSegment::Unparsed]);
        }
        assert!(!command_matches_pattern("cat <(ls)", "cat *"));
    }

    #[test]
    fn should_miss_when_an_unparsed_segment_sits_in_a_matching_compound() {
        assert!(!command_matches_pattern("git status && git `x`", "git *"));
    }

    #[test]
    fn should_miss_an_empty_command_even_with_an_empty_pattern() {
        assert!(!command_matches_pattern("  ", ""));
    }

    #[test]
    fn should_drop_a_cd_segment_in_the_middle_of_a_compound() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        let ws = WorkspacePaths::new(root.path().into());
        assert_eq!(
            strip_resolved_dir_segments("git add . && cd src && make", &ws),
            "git add . && make"
        );
    }

    #[test]
    fn should_extract_the_target_of_a_builtin_cd_segment() {
        assert_eq!(
            extract_directory_switch("builtin cd src"),
            Some(PathBuf::from("src"))
        );
        assert_eq!(
            extract_directory_switch("command pushd -P \"a/b\""),
            Some(PathBuf::from("a/b"))
        );
        assert_eq!(extract_directory_switch("cd $HOME"), None);
        assert_eq!(extract_directory_switch("git status"), None);
    }

    /// A Windows absolute `cd` target must be recognized — it is the whole
    /// point of the outside-directory review, and the Bash tool layer has no
    /// cwd validation of its own.
    ///
    /// The regression this pins: the metacharacter screen rejected every
    /// token containing a backslash, and on Windows every absolute path has
    /// one, so `cd C:\outside && ...` returned `None` and the ask never
    /// fired.
    #[test]
    #[cfg(windows)]
    fn should_extract_a_windows_path_target_of_a_cd_segment() {
        assert_eq!(
            extract_directory_switch(r"cd C:\Users\me\Desktop"),
            Some(PathBuf::from(r"C:\Users\me\Desktop"))
        );
        // The drive-less MSYS spelling is a plain token and stays recognized.
        assert_eq!(
            extract_directory_switch("cd /c/Users/me/Desktop"),
            Some(PathBuf::from("/c/Users/me/Desktop"))
        );
        // Shell expansion is still declined on Windows.
        assert_eq!(extract_directory_switch(r"cd C:\a\$b"), None);
    }

    /// On POSIX a backslash introduces an escape sequence, so the token is
    /// not a static target — that behavior is unchanged.
    #[test]
    #[cfg(unix)]
    fn should_decline_an_escaped_token_when_extracting_a_cd_segment() {
        assert_eq!(extract_directory_switch(r"cd a\ b"), None);
        assert_eq!(extract_directory_switch("cd /tmp/plain"), {
            Some(PathBuf::from("/tmp/plain"))
        });
    }

    #[test]
    fn should_drop_an_in_workspace_cd_segment_from_the_match_string() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        let ws = WorkspacePaths::new(root.path().into());
        assert_eq!(
            strip_resolved_dir_segments("cd src && git status", &ws),
            "git status"
        );
        // Outside / unresolvable switches stay as command segments.
        assert_eq!(
            strip_resolved_dir_segments("cd /etc && git status", &ws),
            "cd /etc && git status"
        );
        assert_eq!(strip_resolved_dir_segments("cd ~ && ls", &ws), "cd ~ && ls");
    }
}
