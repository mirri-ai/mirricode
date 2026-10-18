//! Rule matching DSL.
//!
//! Permission rules use a simple grammar:
//!
//! ```text
//! pattern    := ( "!" )? toolName ( "(" argPattern ")" )?
//! ```
//!
//! Examples:
//! - `"Write"` — matches any `Write` tool call.
//! - `"Write(/foo/**)"` — matches `Write` calls whose argument matches
//!   `/foo/**`.
//! - `"Tool()"` — matches any `Tool` call (empty parens = no arg pattern).
//! - `"mcp__github__*"` — matches any tool whose name starts with
//!   `mcp__github__`.
//! - `"!Bash"` — negated: matches any tool call **except** `Bash`.
//! - `"!Write(/foo/**)"` — negated: matches any `Write` call whose argument
//!   does **not** match `/foo/**`, plus any non-`Write` tool call.

use globset::{Glob, GlobSetBuilder};

/// Parsed permission rule pattern.
#[derive(Debug, Clone)]
pub struct ParsedPattern {
    /// The tool name glob (e.g. `"Bash"`, `"mcp__github__*"`, `"*"`).
    pub tool_name: String,
    /// Optional argument pattern (e.g. `"/foo/**"`). Empty parens produce
    /// `None` (matches on tool name alone).
    pub arg_pattern: Option<String>,
    /// Whether this pattern is negated (prefixed with `!`).
    ///
    /// A negated pattern matches when the **non-negated** pattern does **not**
    /// match. For example, `!Bash` matches every tool call except `Bash`.
    pub negated: bool,
}

/// Parse a permission rule pattern.
///
/// Returns `None` if the pattern is malformed (e.g. opening paren without
/// closing paren).
pub fn parse_pattern(pattern: &str) -> Option<ParsedPattern> {
    let pattern = pattern.trim();

    // Check for negation prefix.
    let (negated, pattern) = if let Some(rest) = pattern.strip_prefix('!') {
        (true, rest)
    } else {
        (false, pattern)
    };

    if let Some(open) = pattern.find('(') {
        let tool_name = pattern[..open].trim().to_string();
        let rest = &pattern[open + 1..];
        if let Some(close) = rest.rfind(')') {
            let arg_pattern = rest[..close].trim().to_string();
            if arg_pattern.is_empty() {
                return Some(ParsedPattern {
                    tool_name,
                    arg_pattern: None,
                    negated,
                });
            }
            return Some(ParsedPattern {
                tool_name,
                arg_pattern: Some(arg_pattern),
                negated,
            });
        }
        // Malformed: opening paren but no closing paren.
        return None;
    }

    Some(ParsedPattern {
        tool_name: pattern.to_string(),
        arg_pattern: None,
        negated,
    })
}

/// Match a tool name against a glob pattern using [`globset`] semantics.
fn match_tool_name(pattern: &str, tool_name: &str) -> bool {
    match Glob::new(pattern) {
        Ok(glob) => GlobSetBuilder::new()
            .add(glob)
            .build()
            .map(|set| set.is_match(tool_name))
            .unwrap_or_else(|_| pattern == tool_name),
        Err(_) => pattern == tool_name,
    }
}

/// Match a permission rule against a tool call.
///
/// Returns `true` if the rule matches.
///
/// If the pattern is negated (prefixed with `!`), this returns `true` when
/// the non-negated pattern does **not** match.
///
/// The `tool_matches_rule` closure, if provided, is called with the arg
/// pattern when the rule has an arg-pattern component. Tools that expose a
/// `matches_rule` method should pass it here; tools without one should pass
/// `None` (arg-pattern rules will not match).
pub fn match_permission_rule(
    rule_pattern: &str,
    tool_name: &str,
    tool_matches_rule: Option<&dyn Fn(&str) -> bool>,
) -> bool {
    let parsed = match parse_pattern(rule_pattern) {
        Some(p) => p,
        None => return false,
    };

    let positive_match = matches_positive(&parsed, tool_name, tool_matches_rule);

    if parsed.negated {
        !positive_match
    } else {
        positive_match
    }
}

/// Evaluate the non-negated (positive) match for a parsed pattern.
fn matches_positive(
    parsed: &ParsedPattern,
    tool_name: &str,
    tool_matches_rule: Option<&dyn Fn(&str) -> bool>,
) -> bool {
    // Match tool name (glob). "*" matches any tool.
    if parsed.tool_name != "*" && !match_tool_name(&parsed.tool_name, tool_name) {
        return false;
    }

    // If no arg pattern, the rule matches on tool name alone.
    match parsed.arg_pattern {
        None => true,
        Some(ref arg_pattern) => match tool_matches_rule {
            Some(matcher) => matcher(arg_pattern),
            None => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_parse_bare_tool_name_when_pattern_has_no_parens() {
        let p = parse_pattern("Bash").unwrap();
        assert_eq!(p.tool_name, "Bash");
        assert_eq!(p.arg_pattern, None);
        assert!(!p.negated);
    }

    #[test]
    fn should_parse_tool_name_and_arg_pattern_when_pattern_contains_parens() {
        let p = parse_pattern("Write(/foo/**)").unwrap();
        assert_eq!(p.tool_name, "Write");
        assert_eq!(p.arg_pattern.as_deref(), Some("/foo/**"));
        assert!(!p.negated);
    }

    #[test]
    fn should_parse_tool_without_arg_pattern_when_parens_are_empty() {
        let p = parse_pattern("Tool()").unwrap();
        assert_eq!(p.tool_name, "Tool");
        assert_eq!(p.arg_pattern, None);
        assert!(!p.negated);
    }

    #[test]
    fn should_parse_negation_flag_when_pattern_has_bang_prefix() {
        let p = parse_pattern("!Bash").unwrap();
        assert_eq!(p.tool_name, "Bash");
        assert_eq!(p.arg_pattern, None);
        assert!(p.negated);
    }

    #[test]
    fn should_parse_negated_tool_with_arg_pattern_when_pattern_has_bang_and_parens() {
        let p = parse_pattern("!Write(/foo/**)").unwrap();
        assert_eq!(p.tool_name, "Write");
        assert_eq!(p.arg_pattern.as_deref(), Some("/foo/**"));
        assert!(p.negated);
    }

    #[test]
    fn should_return_none_when_pattern_has_unclosed_paren() {
        assert!(parse_pattern("Write(/foo").is_none());
    }

    #[test]
    fn should_match_when_tool_name_equals_pattern() {
        assert!(match_permission_rule("Read", "Read", None));
    }

    #[test]
    fn should_not_match_when_tool_name_differs_from_pattern() {
        assert!(!match_permission_rule("Write", "Read", None));
    }

    #[test]
    fn should_match_when_tool_name_matches_glob_pattern() {
        assert!(match_permission_rule(
            "mcp__github__*",
            "mcp__github__create_pr",
            None
        ));
    }

    #[test]
    fn should_match_any_tool_when_pattern_is_wildcard() {
        assert!(match_permission_rule("*", "Anything", None));
    }

    #[test]
    fn should_not_match_when_rule_has_arg_pattern_without_tool_matcher() {
        // Without a tool matcher, arg-pattern rules never match.
        assert!(!match_permission_rule("Write(/foo/**)", "Write", None));
    }

    #[test]
    fn should_match_when_tool_matcher_accepts_arg_pattern() {
        let matcher = |arg: &str| arg == "/foo/**";
        assert!(match_permission_rule(
            "Write(/foo/**)",
            "Write",
            Some(&matcher)
        ));
    }

    // -----------------------------------------------------------------------
    // Negation semantics tests
    // -----------------------------------------------------------------------

    #[test]
    fn should_not_match_when_negated_pattern_targets_same_tool() {
        // !Bash should NOT match "Bash"
        assert!(!match_permission_rule("!Bash", "Bash", None));
    }

    #[test]
    fn should_match_when_negated_pattern_targets_different_tool() {
        // !Bash should match "Read"
        assert!(match_permission_rule("!Bash", "Read", None));
    }

    #[test]
    fn should_not_match_any_tool_when_pattern_is_negated_wildcard() {
        // !* should not match any tool
        assert!(!match_permission_rule("!*", "Anything", None));
        assert!(!match_permission_rule("!*", "Bash", None));
    }

    #[test]
    fn should_match_other_tools_when_negated_pattern_is_tool_specific() {
        // !Write should match Read, Bash, etc.
        assert!(match_permission_rule("!Write", "Read", None));
        assert!(match_permission_rule("!Write", "Bash", None));
    }

    #[test]
    fn should_not_match_when_negated_arg_pattern_matches_tool_args() {
        let matcher = |arg: &str| arg == "/foo/**";
        // !Write(/foo/**) should NOT match Write with /foo/** args
        assert!(!match_permission_rule(
            "!Write(/foo/**)",
            "Write",
            Some(&matcher)
        ));
    }

    #[test]
    fn should_match_when_negated_arg_pattern_does_not_match_tool_args() {
        let matcher = |arg: &str| arg == "/foo/**";
        // !Write(/foo/**) should match Write with /bar/** args
        let bar_matcher = |arg: &str| arg == "/bar/**";
        assert!(match_permission_rule(
            "!Write(/foo/**)",
            "Write",
            Some(&bar_matcher)
        ));
        // Also matches non-Write tools
        assert!(match_permission_rule(
            "!Write(/foo/**)",
            "Read",
            Some(&matcher)
        ));
    }

    #[test]
    fn should_not_match_when_negated_glob_matches_tool_name() {
        // !mcp__github__* should NOT match mcp__github__create_pr
        assert!(!match_permission_rule(
            "!mcp__github__*",
            "mcp__github__create_pr",
            None
        ));
        // But should match other tools
        assert!(match_permission_rule("!mcp__github__*", "Read", None));
    }
}
