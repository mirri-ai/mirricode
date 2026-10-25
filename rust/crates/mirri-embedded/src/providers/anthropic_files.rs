//! [`AnthropicFiles`] — the Anthropic Files API client: the image upload
//! channel behind the media materializer (T11a, F-P8-5).
//!
//! Mirrors [`KimiFiles`](super::kimi_files::KimiFiles) structurally (a
//! reqwest multipart client over the same endpoint inputs), with the
//! Anthropic dialect: the `x-api-key` + `anthropic-version: 2023-06-01`
//! header pair the Messages transport uses, **no `purpose` field** (the GA
//! Files API takes the file part alone), and an image-only surface — the
//! client rejects any media type outside `image/{jpeg,png,gif,webp}`
//! before any network request.
//!
//! The upload runs *inside* the materializer (unlike the Kimi video
//! channel, which callers invoke explicitly): the returned
//! [`UploadedFile`] file id becomes the `fileref:anthropic;{id}` URL the
//! transient chat params carry, which
//! [`AnthropicLlm`](super::anthropic::AnthropicLlm) then emits as the
//! image block's `source.type == "file"` reference.
//!
//! Beyond the TS parity surface: the TypeScript kosong Anthropic channel
//! only ever inlines base64/URL images (`anthropic.ts` has no Files API
//! use) — this client is a rust-only extension, recorded in the ROADMAP as
//! a deliberate divergence.

use mirri_core::error::{EngineError, EngineResult};
use mirri_kosong::join_url;
use serde::Deserialize;

/// The uploaded file's Files API reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedFile {
    /// The Files API file id (e.g. `"file-abc123"`).
    pub file_id: String,
    /// The provider-reported expiry (RFC 3339), when one was returned.
    /// Anthropic files default to no expiry (they persist until deleted).
    pub expires_at: Option<String>,
}

/// A client for the Anthropic Files API.
///
/// Constructed with the same endpoint inputs as
/// [`AnthropicLlm`](super::anthropic::AnthropicLlm): API key and base URL
/// (no trailing slash; `/v1/files` is joined onto it, requiring the base to
/// carry the version segment).
pub struct AnthropicFiles {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
    /// Extra headers from the provider config (`customHeaders`), forwarded
    /// on the upload request like the chat transport forwards them.
    extra_headers: Vec<(String, String)>,
}

impl AnthropicFiles {
    /// Construct a client for `base_url` authenticated with `api_key`.
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            client: reqwest::Client::new(),
            extra_headers: Vec::new(),
        }
    }

    /// Attach config-declared custom headers forwarded on every upload.
    pub fn with_extra_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.extra_headers = headers;
        self
    }

    /// The files endpoint URL (`{base}/v1/files`).
    fn files_url(&self) -> String {
        join_url(&self.base_url, "/files")
    }

    /// Upload an image and return its Files API reference.
    ///
    /// Media types outside the Files API's image set
    /// (`image/{jpeg,png,gif,webp}`) are rejected before any network request
    /// (`request.invalid`); a non-2xx response or transport failure surfaces
    /// as a provider error.
    pub async fn upload_image(
        &self,
        data: &[u8],
        mime_type: &str,
        filename: Option<&str>,
    ) -> EngineResult<UploadedFile> {
        let extension = image_extension(mime_type).ok_or_else(|| {
            EngineError::RequestInvalid(format!(
                "AnthropicFiles.upload_image: unsupported image media type {mime_type} \
                 (expected one of image/jpeg, image/png, image/gif, image/webp)"
            ))
        })?;
        let filename = filename
            .map(str::to_string)
            .unwrap_or_else(|| format!("upload.{extension}"));

        let file_part = reqwest::multipart::Part::bytes(data.to_vec())
            .file_name(filename)
            .mime_str(mime_type)
            .map_err(|error| {
                EngineError::RequestInvalid(format!(
                    "AnthropicFiles.upload_image: invalid media type {mime_type}: {error}"
                ))
            })?;
        // The GA Files API takes the file part alone — no `purpose` field.
        let form = reqwest::multipart::Form::new().part("file", file_part);

        let mut request = self
            .client
            .post(self.files_url())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01");
        for (name, value) in &self.extra_headers {
            request = request.header(name, value);
        }
        let response = request.multipart(form).send().await.map_err(|error| {
            EngineError::ProviderError(format!("Anthropic files upload failed: {error}"))
        })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(EngineError::ProviderError(format!(
                "Anthropic files API error {status}: {body}"
            )));
        }

        #[derive(Debug, Deserialize)]
        struct FilesUploadResponse {
            id: String,
            expires_at: Option<String>,
        }
        let uploaded: FilesUploadResponse = response.json().await.map_err(|error| {
            EngineError::ProviderError(format!(
                "Anthropic files upload response was not the expected JSON shape: {error}"
            ))
        })?;
        Ok(UploadedFile {
            file_id: uploaded.id,
            expires_at: uploaded.expires_at,
        })
    }
}

/// The filename extension for a Files-API-accepted image media type, `None`
/// for anything else (the client-side validation set).
fn image_extension(mime_type: &str) -> Option<&'static str> {
    match mime_type {
        "image/jpeg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::*};

    use super::*;

    /// Read a request header value (wiremock's `Request` headers are an
    /// `http_types` map keyed by `HeaderName` with multi-values).
    fn header_value(request: &wiremock::Request, name: &str) -> Option<String> {
        request
            .headers
            .get(name)
            .map(|value| value.to_str().unwrap_or_default().to_string())
    }

    fn image_bytes() -> Vec<u8> {
        b"\x89PNG\r\n\x1a\nfake-image-payload".to_vec()
    }

    #[tokio::test]
    async fn should_upload_an_image_and_return_the_files_api_reference() {
        // Given a mock Anthropic files endpoint answering an id
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/files"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "id": "file-abc123", "type": "file" })),
            )
            .mount(&server)
            .await;

        // When an image is uploaded
        let files = AnthropicFiles::new("test-api-key", format!("{}/v1", server.uri()));
        let uploaded = files
            .upload_image(&image_bytes(), "image/png", Some("cat.png"))
            .await
            .expect("upload succeeds");

        // Then the reference carries the returned id and no expiry
        assert_eq!(uploaded.file_id, "file-abc123");
        assert_eq!(uploaded.expires_at, None);

        // And the request was a multipart POST without a purpose field,
        // authenticated the Anthropic way
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        let content_type = header_value(request, "content-type").expect("content-type");
        assert!(
            content_type.starts_with("multipart/form-data"),
            "multipart content type: {content_type}"
        );
        let body = String::from_utf8_lossy(&request.body);
        assert!(
            body.contains("filename=\"cat.png\""),
            "filename in body:\n{body}"
        );
        assert!(
            !body.contains("purpose"),
            "the Anthropic Files API takes no purpose field:\n{body}"
        );
        assert_eq!(
            header_value(request, "x-api-key"),
            Some("test-api-key".to_string()),
            "x-api-key on the upload"
        );
        assert_eq!(
            header_value(request, "anthropic-version"),
            Some("2023-06-01".to_string()),
            "anthropic-version on the upload"
        );
    }

    #[tokio::test]
    async fn should_reject_non_image_media_types_before_any_request() {
        // Given a mock endpoint that would record any request
        let server = MockServer::start().await;

        // When a non-image media type is uploaded
        let files = AnthropicFiles::new("test-api-key", format!("{}/v1", server.uri()));
        let result = files
            .upload_image(b"not-an-image".as_slice(), "video/mp4", None)
            .await;

        // Then the upload is rejected client-side (request.invalid) and no
        // network request was made
        let error = result.expect_err("non-image mime must be rejected");
        assert!(matches!(error, EngineError::RequestInvalid(_)), "{error:?}");
        assert!(error.to_string().contains("video/mp4"), "{error}");
        assert!(
            server
                .received_requests()
                .await
                .expect("requests")
                .is_empty(),
            "no request may be sent for a rejected media type"
        );
    }

    #[tokio::test]
    async fn should_accept_every_files_api_image_media_type() {
        // Given every accepted image media type
        let accepted = ["image/jpeg", "image/png", "image/gif", "image/webp"];

        // When each is checked against the validation set
        // Then all pass (and each maps to a sensible default extension)
        for mime_type in accepted {
            assert!(image_extension(mime_type).is_some(), "{mime_type}");
        }
        assert_eq!(image_extension("image/jpeg"), Some("jpg"));
        assert_eq!(image_extension("image/tiff"), None);
    }

    #[tokio::test]
    async fn should_surface_a_provider_error_when_the_files_endpoint_fails() {
        // Given a mock files endpoint answering an error status
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/files"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        // When an image is uploaded
        let files = AnthropicFiles::new("test-api-key", format!("{}/v1", server.uri()));
        let result = files.upload_image(&image_bytes(), "image/png", None).await;

        // Then the failure surfaces as a provider error carrying the status
        let error = result.expect_err("a 500 must surface an error");
        assert!(matches!(error, EngineError::ProviderError(_)), "{error:?}");
        assert!(error.to_string().contains("500"), "{error}");
    }

    #[tokio::test]
    async fn should_hit_the_version_segment_from_the_base_when_base_carries_it() {
        // Given a mock endpoint and a base URL carrying /v1
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/files"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": "file-x" })),
            )
            .mount(&server)
            .await;

        // When an image is uploaded through the versioned base
        let files = AnthropicFiles::new("test-api-key", format!("{}/v1", server.uri()));
        let uploaded = files
            .upload_image(&image_bytes(), "image/png", None)
            .await
            .expect("upload succeeds");

        // Then the request hit the versioned path exactly — the version
        // segment comes from the base, never from the engine
        assert_eq!(uploaded.file_id, "file-x");
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/v1/files");
    }
}
