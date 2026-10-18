use anyhow::Result;
use serde_json::Value;

use crate::provider::{
    ChatProvider, HTTP_CONNECT_TIMEOUT, HTTP_READ_TIMEOUT, http_client_with_timeouts,
};
use crate::streaming::{AnthropicDispatcher, SseStream};

/// Anthropic Messages API streaming client.
///
/// Sends a raw JSON request body (matching the Anthropic `/v1/messages`
/// schema, including `model`, `max_tokens`, `messages`, and optional
/// `stream: true`) and returns an [`SseStream`] over the response body on
/// success. Non-2xx responses are converted into an `anyhow::Error` before
/// any streaming begins.
///
/// The provider only owns transport concerns: API key header injection,
/// the `anthropic-version` header, endpoint routing, and HTTP status
/// validation. All SSE parsing lives in [`SseStream`].
pub struct AnthropicProvider {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
    /// Extra headers from the provider config (`customHeaders`), applied to
    /// every request in addition to the auth and version headers.
    extra_headers: Vec<(String, String)>,
}

impl AnthropicProvider {
    /// Construct a provider pointing at `base_url` (no trailing slash) using
    /// `api_key` for the `x-api-key` header.
    pub fn new(api_key: String, base_url: String) -> Self {
        Self {
            api_key,
            base_url,
            client: http_client_with_timeouts(HTTP_CONNECT_TIMEOUT, HTTP_READ_TIMEOUT)
                .unwrap_or_else(|error| panic!("static client config builds: {error}")),
            extra_headers: Vec::new(),
        }
    }

    /// Attach config-declared custom headers (e.g. a gateway-required
    /// `User-Agent`) applied to every request.
    pub fn with_extra_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.extra_headers = headers;
        self
    }

    /// Swap the HTTP client (tests inject short-timeout clients).
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Full Messages API endpoint URL.
    fn messages_url(&self) -> String {
        crate::provider::join_url(&self.base_url, "/messages")
    }
}

impl ChatProvider for AnthropicProvider {
    async fn chat(&self, request: Value) -> Result<SseStream> {
        let mut request_builder = self
            .client
            .post(self.messages_url())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json");
        for (name, value) in &self.extra_headers {
            request_builder = request_builder.header(name, value);
        }
        let response = request_builder.json(&request).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic API error {status}: {body}");
        }

        Ok(SseStream::new(
            response,
            Box::new(AnthropicDispatcher::default()),
        ))
    }
}
