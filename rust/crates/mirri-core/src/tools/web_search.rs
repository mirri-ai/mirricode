//! WebSearch tool — search the web for information.
//!
//! The `WebSearch` tool calls a host-injected [`WebSearchProvider`] to
//! execute a search query and returns formatted results to the model.
//!
//! Registration is conditional: the tool is only registered when a
//! `WebSearchProvider` is available in `BuiltinToolServices`.

use std::sync::Arc;

use serde_json::json;

use crate::error::EngineError;
use crate::tool::{
    ExecutableToolContext, ExecutionPlan, Tool, ToolExecution, ToolFuture, ToolInputDisplay,
    ToolResult,
};
use crate::tools::tool_execution_error;
use crate::tools::web_providers::WebSearchProvider;

/// The `WebSearch` tool — search the web.
pub struct WebSearchTool {
    provider: Arc<dyn WebSearchProvider>,
}

impl WebSearchTool {
    pub fn new(provider: Arc<dyn WebSearchProvider>) -> Self {
        Self { provider }
    }
}

impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "WebSearch"
    }

    fn description(&self) -> &str {
        "Search the web for current information. Returns titles, URLs, and \
         snippets for each result. Use this when you need up-to-date \
         information that may not be in your training data."
    }

    fn capabilities(&self) -> &[String] {
        static CAPABILITIES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
        CAPABILITIES.get_or_init(|| vec!["web.search".to_string()])
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The query text to search for."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> Result<ToolExecution, EngineError> {
        let query: String = input
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| EngineError::RequestInvalid("missing 'query'".into()))?
            .to_string();

        if query.trim().is_empty() {
            return Ok(tool_execution_error("Search query must not be empty."));
        }

        let provider = Arc::clone(&self.provider);

        let plan = ExecutionPlan {
            accesses: Vec::new(),
            display: ToolInputDisplay::Generic {
                summary: format!("WebSearch: {query}"),
            },
            approval_rule: "WebSearch".to_string(),
            stop_batch_after_this: false,
        };

        let execute: Box<dyn FnOnce(ExecutableToolContext) -> ToolFuture + Send> =
            Box::new(move |ctx: ExecutableToolContext| {
                Box::pin(async move {
                    let result = tokio::select! {
                        r = std::future::ready(provider.search(&query)) => r,
                        _ = ctx.cancel.cancelled() => return ToolResult::error("cancelled by user"),
                    };
                    match result {
                        Ok(results) if results.is_empty() => {
                            ToolResult::success("No search results found.")
                        }
                        Ok(results) => {
                            let mut text = String::new();
                            for (i, r) in results.iter().enumerate() {
                                if i > 0 {
                                    text.push_str("\n---\n\n");
                                }
                                text.push_str(&format!("Title: {}\n", r.title));
                                if let Some(ref site) = r.site_name {
                                    text.push_str(&format!("Site: {site}\n"));
                                }
                                if let Some(ref date) = r.date {
                                    text.push_str(&format!("Date: {date}\n"));
                                }
                                text.push_str(&format!("URL: {}\n", r.url));
                                text.push_str(&format!("Snippet: {}\n", r.snippet));
                            }
                            text.push_str(
                                "\nWhen you rely on a result in your answer, cite it inline \
                                 as a markdown link, e.g. [title](url).",
                            );
                            ToolResult::success(text)
                        }
                        Err(e) => ToolResult::error(format!("Search failed: {e}")),
                    }
                })
            });

        Ok(ToolExecution::Runnable { plan, execute })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::EngineError;
    use crate::tool::ToolOutput;
    use crate::tools::web_providers::{WebSearchProvider, WebSearchResult};

    struct MockProvider {
        results: Vec<WebSearchResult>,
        should_fail: bool,
    }

    impl WebSearchProvider for MockProvider {
        fn search(&self, _query: &str) -> Result<Vec<WebSearchResult>, EngineError> {
            if self.should_fail {
                Err(EngineError::RequestInvalid("mock search failure".into()))
            } else {
                Ok(self.results.clone())
            }
        }
    }

    fn make_tool(results: Vec<WebSearchResult>) -> WebSearchTool {
        WebSearchTool::new(Arc::new(MockProvider {
            results,
            should_fail: false,
        }))
    }

    fn make_failing_tool() -> WebSearchTool {
        WebSearchTool::new(Arc::new(MockProvider {
            results: Vec::new(),
            should_fail: true,
        }))
    }

    async fn run_tool(tool: &WebSearchTool, input: serde_json::Value) -> ToolResult {
        let execution = tool.resolve_execution(input).expect("resolve");
        match execution {
            ToolExecution::Runnable { execute, plan } => {
                assert_eq!(plan.approval_rule, "WebSearch");
                let ctx = ExecutableToolContext {
                    approval_metadata: None,
                    wait_registry: None,
                    turn_id: "test-turn".into(),
                    tool_call_id: "test-call".into(),
                    cancel: crate::cancel::CancelToken::new(),
                    progress: crate::tool::ToolProgressHandle::default(),
                };
                execute(ctx).await
            }
            ToolExecution::Error(err) => ToolResult::error(err.message),
        }
    }

    #[tokio::test]
    async fn should_return_formatted_results_when_search_succeeds() {
        let tool = make_tool(vec![
            WebSearchResult {
                title: "Rust Programming".into(),
                url: "https://rust-lang.org".into(),
                snippet: "A language empowering everyone to build reliable software.".into(),
                site_name: Some("rust-lang.org".into()),
                date: Some("2024-01-01".into()),
            },
            WebSearchResult {
                title: "Learn Rust".into(),
                url: "https://doc.rust-lang.org".into(),
                snippet: "The Rust Programming Language book.".into(),
                site_name: None,
                date: None,
            },
        ]);
        let result = run_tool(&tool, json!({"query": "rust programming"})).await;
        assert!(!result.is_error);
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert!(text.contains("Title: Rust Programming"), "text: {text}");
        assert!(text.contains("URL: https://rust-lang.org"), "text: {text}");
        assert!(text.contains("Site: rust-lang.org"), "text: {text}");
        assert!(text.contains("Date: 2024-01-01"), "text: {text}");
        assert!(text.contains("---"), "should have separator: {text}");
        assert!(text.contains("Title: Learn Rust"), "text: {text}");
    }

    #[tokio::test]
    async fn should_return_no_results_message_when_search_returns_empty() {
        let tool = make_tool(Vec::new());
        let result = run_tool(&tool, json!({"query": "obscure query"})).await;
        assert!(!result.is_error);
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert_eq!(text, "No search results found.");
    }

    #[tokio::test]
    async fn should_return_error_when_provider_fails() {
        let tool = make_failing_tool();
        let result = run_tool(&tool, json!({"query": "test"})).await;
        assert!(result.is_error);
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert!(text.contains("Search failed"), "text: {text}");
    }

    #[tokio::test]
    async fn should_return_error_when_query_is_empty() {
        let tool = make_tool(Vec::new());
        let result = run_tool(&tool, json!({"query": "  "})).await;
        assert!(result.is_error);
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert!(text.contains("must not be empty"), "text: {text}");
    }

    #[tokio::test]
    async fn should_return_error_when_query_is_missing() {
        let tool = make_tool(Vec::new());
        let result = tool.resolve_execution(json!({}));
        assert!(result.is_err(), "should fail without query");
    }

    #[tokio::test]
    async fn should_include_citation_reminder_when_results_present() {
        let tool = make_tool(vec![WebSearchResult {
            title: "Test".into(),
            url: "https://example.com".into(),
            snippet: "Snippet".into(),
            site_name: None,
            date: None,
        }]);
        let result = run_tool(&tool, json!({"query": "test"})).await;
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert!(
            text.contains("cite it inline as a markdown link"),
            "should include citation reminder with format example: {text}"
        );
    }

    #[tokio::test]
    async fn should_declare_web_search_capability() {
        let tool = make_tool(Vec::new());
        assert_eq!(tool.capabilities(), &["web.search".to_string()]);
    }
}
