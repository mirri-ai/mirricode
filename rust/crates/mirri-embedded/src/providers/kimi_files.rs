//! [`KimiFiles`] — the Kimi (Moonshot) files API client: the video upload
//! channel (T7, F-P8-3).
//!
//! Mirrors the TypeScript `MirriFiles`
//! (`packages/agent-core-v2/src/kosong/provider/providers/kimi/kimi-files.ts`):
//! a video (from a filesystem path or in-memory bytes) is uploaded to the
//! files endpoint (`POST {base}/v1/files`, multipart with `purpose=video`)
//! and the response's file id becomes the `ms://<id>` URL a wire message's
//! `video_url` part references — [`KimiLlm`](super::kimi::KimiLlm) then
//! emits it as the nested `video_url` content block.
//!
//! Like the TS client, the upload runs **outside** the chat call: the
//! caller uploads first, then composes the message with the returned
//! reference. Validation failures (a non-video media type) are rejected
//! client-side before any network request, as `request.invalid`; upload
//! and transport failures surface as provider errors (classified through
//! the shared provider-error layer when they reach the engine).
//! Quota-exhausted 429s (Moonshot billing wordings or the structured
//! `exceeded_current_quota_error` code) short-circuit into the terminal
//! [`EngineError::ProviderQuotaExhausted`] before the generic provider
//! error — TS `classifyKimiQuotaError` parity.

use std::path::Path;

use mirri_core::error::{EngineError, EngineResult};
use mirri_core::llm::classify_quota_response;
use mirri_kosong::join_url;
use serde::Deserialize;

/// The video upload input: a filesystem path (media type guessed from the
/// file extension) or in-memory bytes carrying an explicit media type.
#[derive(Debug, Clone)]
pub enum VideoUploadInput<'a> {
    /// A path to a video file on disk.
    Path(&'a Path),
    /// In-memory video bytes with an explicit media type.
    Bytes {
        /// The raw video bytes.
        data: &'a [u8],
        /// The media type; must start with `video/`.
        mime_type: &'a str,
        /// An optional display filename (defaults to `upload.<ext>`).
        filename: Option<&'a str>,
    },
}

/// The uploaded video's files-API reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedVideo {
    /// The files-API file id (e.g. `"file-abc123"`).
    pub file_id: String,
    /// The `ms://<file-id>` URL a wire message's `video_url` part carries.
    pub url: String,
}

/// A client for the Kimi files API (the OpenAI-style `files` endpoint the
/// Moonshot backend serves).
///
/// Constructed with the same endpoint inputs as [`KimiLlm`]: API key and
/// base URL (no trailing slash; `/v1/files` is joined onto it, requiring
/// the base to carry the version segment).
pub struct KimiFiles {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
    /// Extra headers from the provider config (`customHeaders`), forwarded
    /// on the upload request like the chat transport forwards them.
    extra_headers: Vec<(String, String)>,
}

impl KimiFiles {
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

    /// Upload a video and return its `ms://` files-API reference.
    ///
    /// Non-video media types are rejected before any network request
    /// (`request.invalid`); a non-2xx response or transport failure
    /// surfaces as a provider error.
    pub async fn upload_video(&self, input: VideoUploadInput<'_>) -> EngineResult<UploadedVideo> {
        let (data, mime_type, filename) = match input {
            VideoUploadInput::Path(path) => {
                let filename = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| {
                        EngineError::RequestInvalid(format!(
                            "KimiFiles.upload_video: video path has no file name: {}",
                            path.display()
                        ))
                    })?
                    .to_string();
                let mime_type = mime_type_from_extension(&filename).ok_or_else(|| {
                    EngineError::RequestInvalid(format!(
                        "KimiFiles.upload_video: file extension does not indicate a video type: {filename}"
                    ))
                })?;
                let data = std::fs::read(path).map_err(|error| {
                    EngineError::RequestInvalid(format!(
                        "KimiFiles.upload_video: video file not found or not readable ({}): {error}",
                        path.display()
                    ))
                })?;
                (data, mime_type, filename)
            }
            VideoUploadInput::Bytes {
                data,
                mime_type,
                filename,
            } => {
                if !mime_type.starts_with("video/") {
                    return Err(EngineError::RequestInvalid(format!(
                        "KimiFiles.upload_video: expected a video mime type, got {mime_type}"
                    )));
                }
                let filename = filename
                    .map(str::to_string)
                    .unwrap_or_else(|| default_filename(mime_type));
                (data.to_vec(), mime_type.to_string(), filename)
            }
        };

        tracing::info!(
            channel = "kimi_files",
            filename = %filename,
            mime_type = %mime_type,
            size_bytes = data.len(),
            "uploading video through the Kimi files API"
        );
        let file_part = reqwest::multipart::Part::bytes(data)
            .file_name(filename)
            .mime_str(&mime_type)
            .map_err(|error| {
                EngineError::RequestInvalid(format!(
                    "KimiFiles.upload_video: invalid media type {mime_type}: {error}"
                ))
            })?;
        let form = reqwest::multipart::Form::new()
            .text("purpose", "video")
            .part("file", file_part);

        let mut request = self
            .client
            .post(self.files_url())
            .bearer_auth(&self.api_key);
        for (name, value) in &self.extra_headers {
            request = request.header(name, value);
        }
        let response = request.multipart(form).send().await.map_err(|error| {
            EngineError::ProviderError(format!("Kimi files upload failed: {error}"))
        })?;

        if !response.status().is_success() {
            let status = response.status();
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let body = response.text().await.unwrap_or_default();
            // Quota-exhausted 429 is terminal: classify it before the
            // generic provider error so callers (and any retry ladder the
            // upload sits behind) never re-send a request that can only
            // succeed after the account is topped up (TS
            // classifyKimiQuotaError parity, cdbd33c13).
            if let Some(retry_after_ms) =
                classify_quota_response(status.as_u16(), &body, retry_after.as_deref())
            {
                return Err(EngineError::ProviderQuotaExhausted {
                    message: format!("Kimi files API error {status}: {body}"),
                    retry_after_ms: (retry_after_ms > 0).then_some(retry_after_ms),
                });
            }
            return Err(EngineError::ProviderError(format!(
                "Kimi files API error {status}: {body}"
            )));
        }

        #[derive(Debug, Deserialize)]
        struct FilesUploadResponse {
            id: String,
        }
        let uploaded: FilesUploadResponse = response.json().await.map_err(|error| {
            EngineError::ProviderError(format!(
                "Kimi files upload response was not the expected JSON shape: {error}"
            ))
        })?;
        tracing::info!(
            channel = "kimi_files",
            file_id = %uploaded.id,
            url = %format!("ms://{}", uploaded.id),
            "video uploaded through the Kimi files API"
        );
        Ok(UploadedVideo {
            url: format!("ms://{}", uploaded.id),
            file_id: uploaded.id,
        })
    }
}

/// Media types the files endpoint accepts for `purpose=video`, with their
/// filename extensions — mirrored from the TS `MIME_TO_EXT` table.
const VIDEO_MIME_TO_EXT: [(&str, &str); 8] = [
    ("video/mp4", "mp4"),
    ("video/mpeg", "mpeg"),
    ("video/quicktime", "mov"),
    ("video/webm", "webm"),
    ("video/x-matroska", "mkv"),
    ("video/x-msvideo", "avi"),
    ("video/x-flv", "flv"),
    ("video/3gpp", "3gp"),
];

/// Guess a video media type from a filename extension (None for
/// non-video or unknown extensions).
fn mime_type_from_extension(filename: &str) -> Option<String> {
    let extension = filename.rsplit_once('.')?.1.to_ascii_lowercase();
    VIDEO_MIME_TO_EXT
        .iter()
        .find(|(_, ext)| *ext == extension)
        .map(|(mime, _)| (*mime).to_string())
}

/// The default upload filename for a media type (`upload.mp4`, …) — the
/// TS `guessFilename` behavior.
fn default_filename(mime_type: &str) -> String {
    let extension = VIDEO_MIME_TO_EXT
        .iter()
        .find(|(mime, _)| *mime == mime_type)
        .map(|(_, ext)| *ext)
        .unwrap_or("bin");
    format!("upload.{extension}")
}

#[cfg(test)]
mod tests {
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::*};

    use super::*;

    /// A tiny valid MP4 header (the bytes only need to travel; the mock
    /// does not decode them).
    fn video_bytes() -> Vec<u8> {
        b"\x00\x00\x00\x18ftypmp42fake-video-payload".to_vec()
    }

    /// Read a request header value (wiremock 0.6's `Request` headers are the
    /// single-value `http::HeaderMap`).
    fn header_value(request: &wiremock::Request, name: &str) -> Option<String> {
        request
            .headers
            .get(name)
            .map(|value| value.to_str().unwrap_or_default().to_string())
    }

    #[tokio::test]
    async fn should_upload_video_and_return_the_ms_reference() {
        // Given a mock Kimi files endpoint answering an id
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/files"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "id": "file-abc123", "object": "file" })),
            )
            .mount(&server)
            .await;

        // When a video is uploaded
        let files = KimiFiles::new("test-api-key", format!("{}/v1", server.uri()));
        let uploaded = files
            .upload_video(VideoUploadInput::Bytes {
                data: &video_bytes(),
                mime_type: "video/mp4",
                filename: Some("clip.mp4"),
            })
            .await
            .expect("upload succeeds");

        // Then the reference is the ms:// URL over the returned id
        assert_eq!(uploaded.file_id, "file-abc123");
        assert_eq!(uploaded.url, "ms://file-abc123");

        // And the request was a multipart POST with the video fields
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        let content_type = header_value(request, "content-type").expect("content-type header");
        assert!(
            content_type.starts_with("multipart/form-data"),
            "multipart content type: {content_type}"
        );
        let body = String::from_utf8_lossy(&request.body);
        assert!(
            body.contains("name=\"purpose\""),
            "purpose field in body:\n{body}"
        );
        assert!(body.contains("video"), "purpose value in body:\n{body}");
        assert!(
            body.contains("filename=\"clip.mp4\""),
            "filename in body:\n{body}"
        );
        assert_eq!(
            header_value(request, "authorization"),
            Some("Bearer test-api-key".to_string()),
            "bearer auth on the upload"
        );
    }

    #[tokio::test]
    async fn should_reject_non_video_media_types_before_any_request() {
        // Given a mock endpoint that would record any request
        let server = MockServer::start().await;

        // When a non-video media type is uploaded
        let files = KimiFiles::new("test-api-key", format!("{}/v1", server.uri()));
        let result = files
            .upload_video(VideoUploadInput::Bytes {
                data: b"not-a-video".as_slice(),
                mime_type: "image/png",
                filename: None,
            })
            .await;

        // Then the upload is rejected client-side (request.invalid) and no
        // network request was made
        let error = result.expect_err("non-video mime must be rejected");
        assert!(matches!(error, EngineError::RequestInvalid(_)), "{error:?}");
        assert!(error.to_string().contains("image/png"), "{error}");
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
    async fn should_reject_a_path_whose_extension_is_not_a_video_type() {
        // Given a mock endpoint and a non-video file
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let text_file = dir.path().join("notes.txt");
        std::fs::write(&text_file, "just text").expect("write");

        // When the path is uploaded
        let files = KimiFiles::new("test-api-key", format!("{}/v1", server.uri()));
        let result = files.upload_video(VideoUploadInput::Path(&text_file)).await;

        // Then the extension is rejected before any request
        let error = result.expect_err("non-video extension must be rejected");
        assert!(matches!(error, EngineError::RequestInvalid(_)), "{error:?}");
        assert!(
            error.to_string().contains("does not indicate a video type"),
            "{error}"
        );
        assert!(
            server
                .received_requests()
                .await
                .expect("requests")
                .is_empty(),
            "no request may be sent for a rejected extension"
        );
    }

    #[tokio::test]
    async fn should_upload_a_video_file_from_a_path() {
        // Given a mock files endpoint and a video file on disk
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/files"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "id": "file-from-path" })),
            )
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().expect("tempdir");
        let video_file = dir.path().join("clip.webm");
        std::fs::write(&video_file, video_bytes()).expect("write");

        // When the path is uploaded
        let files = KimiFiles::new("test-api-key", format!("{}/v1", server.uri()));
        let uploaded = files
            .upload_video(VideoUploadInput::Path(&video_file))
            .await
            .expect("upload succeeds");

        // Then the reference is returned and the multipart carried the
        // extension-guessed media type
        assert_eq!(uploaded.url, "ms://file-from-path");
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        let body = String::from_utf8_lossy(&requests[0].body);
        assert!(body.contains("clip.webm"), "filename in body:\n{body}");
    }

    #[tokio::test]
    async fn should_surface_a_provider_error_when_the_files_endpoint_rejects_the_upload() {
        // Given a mock files endpoint answering an error status
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/files"))
            .respond_with(ResponseTemplate::new(413).set_body_string("file too large"))
            .mount(&server)
            .await;

        // When a video is uploaded
        let files = KimiFiles::new("test-api-key", format!("{}/v1", server.uri()));
        let result = files
            .upload_video(VideoUploadInput::Bytes {
                data: &video_bytes(),
                mime_type: "video/mp4",
                filename: None,
            })
            .await;

        // Then the failure surfaces as a provider error carrying the status
        let error = result.expect_err("a 413 must surface an error");
        assert!(matches!(error, EngineError::ProviderError(_)), "{error:?}");
        assert!(error.to_string().contains("413"), "{error}");
    }

    #[tokio::test]
    async fn should_surface_quota_exhausted_when_the_files_endpoint_returns_a_billing_429() {
        // Given a mock files endpoint answering the Moonshot quota body
        // with a Retry-After header
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/files"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "7")
                    .set_body_json(serde_json::json!({
                        "error": {
                            "type": "exceeded_current_quota_error",
                            "message": "You exceeded your current token quota, please check your account balance"
                        }
                    })),
            )
            .mount(&server)
            .await;

        // When a video is uploaded
        let files = KimiFiles::new("test-api-key", format!("{}/v1", server.uri()));
        let result = files
            .upload_video(VideoUploadInput::Bytes {
                data: &video_bytes(),
                mime_type: "video/mp4",
                filename: None,
            })
            .await;

        // Then the failure is the terminal quota-exhausted error carrying
        // the Retry-After hint (7s → 7000ms) — not a retryable ProviderError
        match result.expect_err("a quota 429 must surface quota exhaustion") {
            EngineError::ProviderQuotaExhausted {
                message,
                retry_after_ms,
            } => {
                assert!(
                    message.contains("429"),
                    "message carries the status: {message}"
                );
                assert_eq!(retry_after_ms, Some(7_000));
            }
            other => panic!("expected ProviderQuotaExhausted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn should_keep_a_transient_throttle_429_as_a_plain_provider_error() {
        // Given a mock files endpoint answering a per-minute throttle 429
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/files"))
            .respond_with(
                ResponseTemplate::new(429)
                    .set_body_string("token quota per minute exceeded, slow down"),
            )
            .mount(&server)
            .await;

        // When a video is uploaded
        let files = KimiFiles::new("test-api-key", format!("{}/v1", server.uri()));
        let result = files
            .upload_video(VideoUploadInput::Bytes {
                data: &video_bytes(),
                mime_type: "video/mp4",
                filename: None,
            })
            .await;

        // Then it stays a plain provider error (the caller may retry a
        // transient throttle) — quota classification did not over-fire
        let error = result.expect_err("a throttle 429 must surface an error");
        assert!(matches!(error, EngineError::ProviderError(_)), "{error:?}");
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

        // When a video is uploaded through the versioned base
        let files = KimiFiles::new("test-api-key", format!("{}/v1", server.uri()));
        let uploaded = files
            .upload_video(VideoUploadInput::Bytes {
                data: &video_bytes(),
                mime_type: "video/mp4",
                filename: None,
            })
            .await
            .expect("upload succeeds");

        // Then the request hit the versioned path exactly — the version
        // segment comes from the base, never from the engine
        assert_eq!(uploaded.url, "ms://file-x");
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url.path(), "/v1/files");
    }

    #[test]
    fn should_guess_media_types_from_video_extensions_only() {
        assert_eq!(
            mime_type_from_extension("clip.MP4").as_deref(),
            Some("video/mp4")
        );
        assert_eq!(
            mime_type_from_extension("home.movie.mov").as_deref(),
            Some("video/quicktime")
        );
        assert_eq!(mime_type_from_extension("notes.txt"), None);
        assert_eq!(mime_type_from_extension("noext"), None);
    }

    #[test]
    fn should_default_the_filename_from_the_media_type() {
        assert_eq!(default_filename("video/mp4"), "upload.mp4");
        assert_eq!(default_filename("video/x-matroska"), "upload.mkv");
    }
}
