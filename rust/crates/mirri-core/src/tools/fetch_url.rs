//! FetchURL tool — fetch content from a URL.
//!
//! The `FetchURL` tool calls a host-injected [`FetchUrlProvider`] to
//! retrieve page content and returns it to the model as text.
//!
//! Registration is conditional: the tool is only registered when a
//! `FetchUrlProvider` is available in `BuiltinToolServices`.

use std::sync::Arc;

use serde_json::json;

use crate::error::EngineError;
use crate::tool::{
    ExecutableToolContext, ExecutionPlan, Tool, ToolExecution, ToolFuture, ToolInputDisplay,
    ToolResult,
};
use crate::tools::tool_execution_error;
use crate::tools::web_providers::{FetchContentKind, FetchUrlProvider};

/// The `FetchURL` tool — fetch content from a URL.
pub struct FetchUrlTool {
    provider: Arc<dyn FetchUrlProvider>,
}

impl FetchUrlTool {
    pub fn new(provider: Arc<dyn FetchUrlProvider>) -> Self {
        Self { provider }
    }
}

impl Tool for FetchUrlTool {
    fn name(&self) -> &str {
        "FetchURL"
    }

    fn description(&self) -> &str {
        "Fetch content from a URL. Returns the page text — either the full \
         response body (for text/plain, text/markdown) or the main text \
         extracted from the page (for HTML). Use this when you need to read \
         a specific web page. Only public http(s) URLs are supported."
    }

    fn capabilities(&self) -> &[String] {
        static CAPABILITIES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
        CAPABILITIES.get_or_init(|| vec!["web.fetch".to_string()])
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to fetch content from. Must start with http:// or https://."
                    }
                },
                "required": ["url"],
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> Result<ToolExecution, EngineError> {
        let url: String = input
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| EngineError::RequestInvalid("missing 'url'".into()))?
            .to_string();

        if url.trim().is_empty() {
            return Ok(tool_execution_error("URL must not be empty."));
        }

        // Basic URL validation — must start with http:// or https://.
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Ok(tool_execution_error(
                "URL must start with 'http://' or 'https://'.",
            ));
        }

        let provider = Arc::clone(&self.provider);

        let plan = ExecutionPlan {
            accesses: Vec::new(),
            display: ToolInputDisplay::UrlFetch { url: url.clone() },
            approval_rule: "FetchURL".to_string(),
            stop_batch_after_this: false,
        };

        let execute: Box<dyn FnOnce(ExecutableToolContext) -> ToolFuture + Send> =
            Box::new(move |ctx: ExecutableToolContext| {
                Box::pin(async move {
                    // Fetch on the blocking pool: `FetchUrlProvider::fetch` is a
                    // synchronous, network-blocking call, and the reqwest
                    // blocking client behind it may only be built and driven
                    // off the async runtime. On cancellation the blocking task
                    // detaches and its result is discarded.
                    let result = match tokio::select! {
                        r = tokio::task::spawn_blocking(move || provider.fetch(&url)) => r,
                        _ = ctx.cancel.cancelled() => return ToolResult::error("cancelled by user"),
                    } {
                        Ok(result) => result,
                        Err(join_error) => Err(EngineError::Internal(format!(
                            "fetch worker failed: {join_error}"
                        ))),
                    };
                    match result {
                        Ok(result) => {
                            if result.content.is_empty() {
                                return ToolResult::success("The response body is empty.");
                            }
                            let note = match result.kind {
                                FetchContentKind::Passthrough => {
                                    "The full response body is returned verbatim.\n\n"
                                }
                                FetchContentKind::Extracted => {
                                    "The main text was extracted from the page.\n\n"
                                }
                            };
                            ToolResult::success(format!(
                                "{note}{content}\n\n\
                                 If you use it in your answer, cite this page as a markdown \
                                 link, e.g. [title](url).",
                                content = result.content
                            ))
                        }
                        Err(e) => ToolResult::error(format!("Failed to fetch URL: {e}")),
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
    use crate::tools::web_providers::{FetchContentKind, FetchUrlProvider, FetchUrlResult};

    struct MockProvider {
        result: Option<FetchUrlResult>,
        should_fail: bool,
    }

    impl FetchUrlProvider for MockProvider {
        fn fetch(&self, _url: &str) -> Result<FetchUrlResult, EngineError> {
            if self.should_fail {
                Err(EngineError::RequestInvalid("mock fetch failure".into()))
            } else {
                self.result
                    .clone()
                    .ok_or_else(|| EngineError::RequestInvalid("no mock result".into()))
            }
        }
    }

    fn make_tool(result: FetchUrlResult) -> FetchUrlTool {
        FetchUrlTool::new(Arc::new(MockProvider {
            result: Some(result),
            should_fail: false,
        }))
    }

    fn make_failing_tool() -> FetchUrlTool {
        FetchUrlTool::new(Arc::new(MockProvider {
            result: None,
            should_fail: true,
        }))
    }

    async fn run_tool(tool: &FetchUrlTool, input: serde_json::Value) -> ToolResult {
        let execution = tool.resolve_execution(input).expect("resolve");
        match execution {
            ToolExecution::Runnable { execute, plan } => {
                assert_eq!(plan.approval_rule, "FetchURL");
                let ctx = ExecutableToolContext {
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
    async fn should_return_content_with_passthrough_note_when_fetching_text() {
        let tool = make_tool(FetchUrlResult {
            content: "Hello, world!".into(),
            kind: FetchContentKind::Passthrough,
        });
        let result = run_tool(&tool, json!({"url": "https://example.com/hello.txt"})).await;
        assert!(!result.is_error);
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert!(text.contains("returned verbatim"), "text: {text}");
        assert!(text.contains("Hello, world!"), "text: {text}");
    }

    #[tokio::test]
    async fn should_return_content_with_extracted_note_when_fetching_html() {
        let tool = make_tool(FetchUrlResult {
            content: "Page title\n\nMain content here.".into(),
            kind: FetchContentKind::Extracted,
        });
        let result = run_tool(&tool, json!({"url": "https://example.com/page"})).await;
        assert!(!result.is_error);
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert!(text.contains("extracted from the page"), "text: {text}");
        assert!(text.contains("Main content here."), "text: {text}");
    }

    #[tokio::test]
    async fn should_return_empty_message_when_content_is_empty() {
        let tool = make_tool(FetchUrlResult {
            content: String::new(),
            kind: FetchContentKind::Passthrough,
        });
        let result = run_tool(&tool, json!({"url": "https://example.com/empty"})).await;
        assert!(!result.is_error);
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert_eq!(text, "The response body is empty.");
    }

    #[tokio::test]
    async fn should_return_error_when_provider_fails() {
        let tool = make_failing_tool();
        let result = run_tool(&tool, json!({"url": "https://example.com/fail"})).await;
        assert!(result.is_error);
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert!(text.contains("Failed to fetch URL"), "text: {text}");
    }

    #[tokio::test]
    async fn should_return_error_when_url_is_missing() {
        let tool = make_tool(FetchUrlResult {
            content: "test".into(),
            kind: FetchContentKind::Passthrough,
        });
        let result = tool.resolve_execution(json!({}));
        assert!(result.is_err(), "should fail without url");
    }

    #[tokio::test]
    async fn should_return_error_when_url_is_empty() {
        let tool = make_tool(FetchUrlResult {
            content: "test".into(),
            kind: FetchContentKind::Passthrough,
        });
        let result = run_tool(&tool, json!({"url": "  "})).await;
        assert!(result.is_error);
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert!(text.contains("must not be empty"), "text: {text}");
    }

    #[tokio::test]
    async fn should_return_error_when_url_lacks_http_scheme() {
        let tool = make_tool(FetchUrlResult {
            content: "test".into(),
            kind: FetchContentKind::Passthrough,
        });
        let result = run_tool(&tool, json!({"url": "ftp://example.com"})).await;
        assert!(result.is_error);
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert!(text.contains("http://"), "text: {text}");
    }

    #[tokio::test]
    async fn should_include_citation_reminder_when_content_present() {
        let tool = make_tool(FetchUrlResult {
            content: "Some content".into(),
            kind: FetchContentKind::Extracted,
        });
        let result = run_tool(&tool, json!({"url": "https://example.com/article"})).await;
        let ToolOutput::Text(text) = &result.output else {
            panic!("expected text output");
        };
        assert!(
            text.contains("cite this page as a markdown link"),
            "should include citation reminder with format example: {text}"
        );
    }

    #[tokio::test]
    async fn should_declare_web_fetch_capability() {
        let tool = make_tool(FetchUrlResult {
            content: "test".into(),
            kind: FetchContentKind::Passthrough,
        });
        assert_eq!(tool.capabilities(), &["web.fetch".to_string()]);
    }
}
