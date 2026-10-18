//! Tests for the completion budget (`calculate_max_tokens`) and system prompt
//! placeholder replacement (`replace_placeholders`).
//!
//! - Completion budget: derives the max output tokens for a request from the
//!   current context size and the model's context-window limit, reserving a
//!   fixed safety margin for the response.
//! - Prompt placeholders: substitutes `{{workspace}}` / `{{platform}}` /
//!   `{{date}}` in system prompt templates without touching undefined tokens.

use mirri_core::context_size::calculate_max_tokens;
use mirri_core::prompt::{PlaceholderContext, replace_placeholders};

/// The response safety margin reserved by `calculate_max_tokens` (1024 tokens).
const SAFETY_MARGIN: usize = 1024;

// ---------------------------------------------------------------------------
// Completion budget
// ---------------------------------------------------------------------------

#[test]
fn should_calculate_max_tokens_as_model_limit_minus_context() {
    // Given a model with a 100k context window and a 20k-token context
    let context_tokens = 20_000;
    let model_limit = 100_000;

    // When computing the completion budget
    let max_tokens = calculate_max_tokens(context_tokens, model_limit);

    // Then the result is the leftover window minus the safety margin
    assert_eq!(max_tokens, model_limit - context_tokens - SAFETY_MARGIN);
}

#[test]
fn should_not_exceed_model_max_output_tokens() {
    // Given a model with a 200k context window
    let model_limit = 200_000;

    // When computing budgets for a range of context sizes
    for context_tokens in [0usize, 1, 1024, 50_000, 150_000, 199_000] {
        let max_tokens = calculate_max_tokens(context_tokens, model_limit);

        // Then the budget never exceeds the model's total limit
        assert!(max_tokens <= model_limit, "{max_tokens} > {model_limit}");

        // And never exceeds the remaining window (context + completion fits)
        assert!(
            max_tokens <= model_limit.saturating_sub(context_tokens),
            "context + completion would exceed the window"
        );
    }
}

#[test]
fn should_return_zero_when_context_exceeds_model_limit() {
    // Given a model with a 100k context window
    let model_limit = 100_000;

    // When the context is at or beyond the limit
    let at_limit = calculate_max_tokens(model_limit, model_limit);
    let beyond_limit = calculate_max_tokens(model_limit + 1, model_limit);
    let far_beyond = calculate_max_tokens(model_limit * 2, model_limit);

    // Then no completion tokens are granted
    assert_eq!(at_limit, 0);
    assert_eq!(beyond_limit, 0);
    assert_eq!(far_beyond, 0);
}

#[test]
fn should_return_zero_when_context_leaves_less_than_safety_margin() {
    // Given a model with a 100k context window
    let model_limit = 100_000;

    // When the leftover window is smaller than the safety margin
    let max_tokens = calculate_max_tokens(model_limit - SAFETY_MARGIN + 1, model_limit);

    // Then the budget saturates to zero rather than going negative
    assert_eq!(max_tokens, 0);
}

#[test]
fn should_reserve_safety_margin_for_response() {
    // Given a model with a 200k window and a 30k-token context
    let context_tokens = 30_000;
    let model_limit = 200_000;

    // When computing the completion budget
    let max_tokens = calculate_max_tokens(context_tokens, model_limit);

    // Then the safety margin is reserved: context + margin + budget == limit
    assert_eq!(
        context_tokens + SAFETY_MARGIN + max_tokens,
        model_limit,
        "the full exchange (context + completion) should fit in the window"
    );
}

// ---------------------------------------------------------------------------
// Prompt placeholder replacement
// ---------------------------------------------------------------------------

/// A fully-populated placeholder context for system prompt tests.
fn populated_context() -> PlaceholderContext {
    PlaceholderContext {
        workspace: Some("/home/dev/project".to_string()),
        platform: Some("linux".to_string()),
        date: Some("2025-03-15".to_string()),
    }
}

#[test]
fn should_replace_workspace_placeholder_in_system_prompt() {
    // Given a template with a workspace placeholder and a populated context
    let template = "Work in {{workspace}} and stay there.";
    let ctx = populated_context();

    // When replacing placeholders
    let rendered = replace_placeholders(template, &ctx);

    // Then the workspace value is substituted
    assert_eq!(rendered, "Work in /home/dev/project and stay there.");
}

#[test]
fn should_replace_platform_placeholder_in_system_prompt() {
    // Given a template with a platform placeholder and a populated context
    let template = "You are running on {{platform}}.";
    let ctx = populated_context();

    // When replacing placeholders
    let rendered = replace_placeholders(template, &ctx);

    // Then the platform value is substituted
    assert_eq!(rendered, "You are running on linux.");
}

#[test]
fn should_replace_date_placeholder_in_system_prompt() {
    // Given a template with a date placeholder and a populated context
    let template = "Today is {{date}}.";
    let ctx = populated_context();

    // When replacing placeholders
    let rendered = replace_placeholders(template, &ctx);

    // Then the date value is substituted
    assert_eq!(rendered, "Today is 2025-03-15.");
}

#[test]
fn should_replace_all_placeholders_when_context_is_populated() {
    // Given a template using every supported placeholder
    let template = "[{{date}}] {{platform}} agent in {{workspace}}";
    let ctx = populated_context();

    // When replacing placeholders
    let rendered = replace_placeholders(template, &ctx);

    // Then each placeholder is substituted in a single pass
    assert_eq!(rendered, "[2025-03-15] linux agent in /home/dev/project");
}

#[test]
fn should_leave_undefined_placeholders_unchanged() {
    // Given a template referencing placeholders with no context values
    let template = "Run in {{workspace}} on {{platform}} at {{date}}.";

    // When replacing with an empty context
    let rendered = replace_placeholders(template, &PlaceholderContext::default());

    // Then every token stays verbatim
    assert_eq!(rendered, template);
}

#[test]
fn should_leave_unknown_placeholders_unchanged() {
    // Given a template with an unsupported token alongside known ones
    let template = "{{workspace}} via {{unknown}}";
    let ctx = populated_context();

    // When replacing placeholders
    let rendered = replace_placeholders(template, &ctx);

    // Then the known token is replaced and the unknown token stays verbatim
    assert_eq!(rendered, "/home/dev/project via {{unknown}}");
}

#[test]
fn should_not_recursively_expand_nested_placeholders() {
    // Given a context whose value contains another placeholder token
    let ctx = PlaceholderContext {
        workspace: Some("{{platform}}".to_string()),
        platform: Some("linux".to_string()),
        ..Default::default()
    };

    // When replacing placeholders in a template using that value
    let rendered = replace_placeholders("echo {{workspace}}", &ctx);

    // Then the substituted value is emitted as-is, without re-expansion
    assert_eq!(rendered, "echo {{platform}}");
}

#[test]
fn should_not_recursively_expand_self_referential_placeholder() {
    // Given a context whose value references its own placeholder
    let ctx = PlaceholderContext {
        workspace: Some("{{workspace}}".to_string()),
        ..Default::default()
    };

    // When replacing placeholders
    let rendered = replace_placeholders("cd {{workspace}}", &ctx);

    // Then the value is emitted verbatim (no infinite recursion)
    assert_eq!(rendered, "cd {{workspace}}");
}

#[test]
fn should_keep_unclosed_placeholder_verbatim() {
    // Given a template with an unclosed `{{` token
    let template = "prefix {{workspace and trailing";
    let ctx = populated_context();

    // When replacing placeholders
    let rendered = replace_placeholders(template, &ctx);

    // Then the unclosed token is untouched
    assert_eq!(rendered, template);
}
