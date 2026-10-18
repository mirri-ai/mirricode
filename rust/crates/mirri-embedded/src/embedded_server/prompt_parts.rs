//! `message.send` input normalization — the files-face entry resolution.
//!
//! The `input` parameter accepts two spellings per part:
//!
//! - the canonical [`ContentPart`](mirri_core::ContentPart) wire form
//!   (`text` / `image_url` / `audio_url` / `video_url` / `think`), passed
//!   through untouched (backward compatibility);
//! - the v1 TS REST prompt form — `{type:"image"|"video", source:{kind:
//!   "base64"|"file"|"url", …}}` and `{type:"file", file_id, name,
//!   media_type, size}` — normalized onto the canonical parts here.
//!
//! File references (`source.kind === "file"` and `type === "file"`) are
//! resolved **at the entry**: the bytes are read from the files store and
//! inlined as `data:` URIs, so context memory never carries a bare
//! `file_id` (which would not survive the files store's expiry). The turn's
//! wire records then offload the large inline URIs to `blobref:` (the
//! caller's [`AgentBlobService::offload_parts`](mirri_core::AgentBlobService::offload_parts)
//! pass), and the existing resume rehydration chain restores them — zero
//! extra recovery logic.

use serde_json::{Value, json};

use mirri_core::blob::format_data_uri;
use mirri_transport::adapter::EngineError;

use super::{EmbeddedServer, invalid_params, not_found};
use crate::file_store::FileStore;
use crate::providers::media_ref::FILEREF_PREFIX;

impl EmbeddedServer {
    /// Normalize a raw `message.send` / `message.steer` input part list.
    ///
    /// Canonical parts pass through; the v1 TS REST spellings are mapped
    /// onto canonical parts with their file references resolved inline. A
    /// missing file answers not-found, and an unrecognized shape fails as
    /// invalid params at the [`ContentPart`] deserialization step.
    pub(super) fn normalize_input_parts(
        &self,
        parts: Vec<Value>,
    ) -> Result<Vec<mirri_core::ContentPart>, EngineError> {
        let files = FileStore::new(self.files_root());
        let mut normalized = Vec::with_capacity(parts.len());
        for raw in parts {
            normalized.push(normalize_part(raw, &files)?);
        }
        serde_json::from_value(Value::Array(normalized))
            .map_err(|e| invalid_params(format!("input part is not a valid content part: {e}")))
    }
}

/// Normalize one raw input part (see the module docs for the accepted
/// spellings).
fn normalize_part(raw: Value, files: &FileStore) -> Result<Value, EngineError> {
    let Some(part_type) = raw.get("type").and_then(Value::as_str) else {
        // Not an object / no discriminator — hand it to the ContentPart
        // deserializer so it produces the canonical error shape.
        return Ok(raw);
    };
    match part_type {
        "image" | "video" => normalize_media_part(part_type, &raw, files),
        "file" => normalize_file_part(&raw, files),
        // Red line (T11a §4.6): a canonical image part may never carry a
        // `fileref:` URL — those are engine-internal, minted only by the
        // media materializer on the transient chat params ("never accept
        // file IDs from untrusted sources"). The check is a prefix match:
        // `fileref:` is a scheme prefix and the materializer's output
        // always sits at the head of the URL, so a remote URL that merely
        // *contains* the substring mid-URL (e.g. `https://…/fileref:x.png`)
        // is legal (F-T11a-2). A `blobref:` URL is the resume-chain's
        // legal restored form and passes through.
        "image_url" => {
            let url = raw
                .get("image_url")
                .and_then(|image_url| image_url.get("url"))
                .and_then(Value::as_str);
            if url.is_some_and(|url| url.starts_with(FILEREF_PREFIX)) {
                return Err(invalid_params(format!(
                    "image_url may not carry a '{FILEREF_PREFIX}' reference \
                     (engine-internal; submit the image bytes or a URL instead)"
                )));
            }
            Ok(raw)
        }
        _ => Ok(raw),
    }
}

/// Map a v1 TS REST media part (`{type:"image"|"video", source}`) onto the
/// canonical wire part, resolving a `file` source against the files store.
///
/// F-P4-2 media-kind honesty: the declared part type must match the media
/// type — an `image` part carries an `image/*` media type, a `video` part
/// a `video/*` one. A mismatch (a PDF declared as an image) fails here as
/// invalid params, at the entry, instead of surfacing later as an opaque
/// engine/model failure. The check runs wherever a concrete media type is
/// known: a `base64` source that declares one, and a resolved `file`
/// source (the store always records one). A `base64` source without a
/// declared media type keeps the historical `application/octet-stream`
/// default and passes through (backward compatibility for engine
/// clients; the ACP face's conversion rejects those before they ever
/// reach here). A `url` source has no media type to check.
fn normalize_media_part(
    part_type: &str,
    raw: &Value,
    files: &FileStore,
) -> Result<Value, EngineError> {
    let source = raw
        .get("source")
        .ok_or_else(|| invalid_params(format!("'{part_type}' part requires a 'source' object")))?;
    let source_kind = source.get("kind").and_then(Value::as_str).unwrap_or("");
    let url = match source_kind {
        "base64" => {
            let media_type = match source.get("media_type").and_then(Value::as_str) {
                Some(media_type) => {
                    validate_media_kind(part_type, media_type)?;
                    media_type
                }
                None => "application/octet-stream",
            };
            let data = source
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_params("base64 source requires 'data'"))?;
            format!("data:{media_type};base64,{data}")
        }
        "url" => source
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| invalid_params("url source requires 'url'"))?,
        "file" => {
            let file_id = source
                .get("file_id")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_params("file source requires 'file_id'"))?;
            let (meta, bytes) = resolve_file(file_id, files)?;
            validate_media_kind(part_type, &meta.media_type)?;
            format_data_uri(&meta.media_type, &bytes)
        }
        other => {
            return Err(invalid_params(format!(
                "unknown '{part_type}' source kind: {other}"
            )));
        }
    };
    // The same red line as the canonical parts (T11a §4.6): a `url` source
    // flows verbatim into the canonical `image_url.url`, so a forged
    // `fileref:` must be rejected here too. Prefix match, same as the
    // canonical branch — only a genuine fileref scheme (at the head) is
    // forged materializer output; a mid-URL substring is a legal remote
    // URL (F-T11a-2), and a `data:` URI's base64 alphabet carries no
    // colon so the prefix cannot appear inside one.
    if part_type == "image" && url.starts_with(FILEREF_PREFIX) {
        return Err(invalid_params(format!(
            "image url source may not carry a '{FILEREF_PREFIX}' reference \
             (engine-internal; submit the image bytes or a URL instead)"
        )));
    }
    if part_type == "image" {
        Ok(json!({ "type": "image_url", "image_url": { "url": url } }))
    } else {
        Ok(json!({ "type": "video_url", "video_url": url }))
    }
}

/// The F-P4-2 media-kind check (see [`normalize_media_part`]): the media
/// type must carry the part type's top-level prefix.
fn validate_media_kind(part_type: &str, media_type: &str) -> Result<(), EngineError> {
    let expected_prefix = if part_type == "image" {
        "image/"
    } else {
        "video/"
    };
    if media_type.starts_with(expected_prefix) {
        Ok(())
    } else {
        Err(invalid_params(format!(
            "'{part_type}' part requires an '{expected_prefix}*' media type, got \
             '{media_type}'"
        )))
    }
}

/// Map a v1 TS REST file part onto the resolved [`ContentPart::File`] shape:
/// the reference is dropped, the inline data URI takes its place, and the
/// display metadata falls back to the files-store record. An
/// already-resolved part (no `file_id`, `data` present) passes through.
fn normalize_file_part(raw: &Value, files: &FileStore) -> Result<Value, EngineError> {
    let Some(file_id) = raw.get("file_id").and_then(Value::as_str) else {
        return Ok(raw.clone());
    };
    let (meta, bytes) = resolve_file(file_id, files)?;
    let data = format_data_uri(&meta.media_type, &bytes);
    Ok(json!({
        "type": "file",
        "data": data,
        "name": raw.get("name").and_then(Value::as_str).unwrap_or(&meta.name),
        "media_type": raw
            .get("media_type")
            .and_then(Value::as_str)
            .unwrap_or(&meta.media_type),
        "size": raw
            .get("size")
            .and_then(Value::as_u64)
            .unwrap_or(meta.size),
    }))
}

/// Look a file reference up, answering not-found for unknown ids.
fn resolve_file(
    file_id: &str,
    files: &FileStore,
) -> Result<(crate::file_store::FileMeta, Vec<u8>), EngineError> {
    files
        .get(file_id)
        .ok_or_else(|| not_found(format!("file not found: {file_id}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_pass_canonical_parts_through_when_normalizing() {
        // Given canonical wire parts (the pre-P4 spellings)
        let raw = vec![
            json!({ "type": "text", "text": "hi" }),
            json!({ "type": "image_url", "image_url": { "url": "https://example.com/a.png" } }),
        ];

        // When normalized without any file references
        // (the files root is irrelevant — no part touches it)
        let files = FileStore::new("/nonexistent-files-root");
        let mut normalized = Vec::new();
        for part in raw.clone() {
            normalized.push(normalize_part(part, &files).unwrap());
        }

        // Then every part is untouched
        assert_eq!(normalized, raw);
    }

    #[test]
    fn should_map_base64_and_url_sources_when_normalizing_media_parts() {
        // Given v1 REST media parts
        let files = FileStore::new("/nonexistent-files-root");
        let image =
            normalize_part(json!({ "type": "image", "source": { "kind": "base64", "media_type": "image/png", "data": "aGk=" } }), &files)
                .unwrap();
        let video = normalize_part(
            json!({ "type": "video", "source": { "kind": "url", "url": "https://example.com/v.mp4" } }),
            &files,
        )
        .unwrap();

        // Then they map onto the canonical parts
        assert_eq!(
            image,
            json!({ "type": "image_url", "image_url": { "url": "data:image/png;base64,aGk=" } })
        );
        assert_eq!(
            video,
            json!({ "type": "video_url", "video_url": "https://example.com/v.mp4" })
        );
    }

    #[test]
    fn should_reject_unknown_source_kind_when_normalizing_media_part() {
        // Given a media part with an exotic source kind
        let files = FileStore::new("/nonexistent-files-root");
        let error = normalize_part(
            json!({ "type": "image", "source": { "kind": "webrtc", "peer": "x" } }),
            &files,
        )
        .unwrap_err();

        // When the error is rendered
        // Then it is an invalid-params error naming the kind
        let message = error.to_string();
        assert!(message.contains("webrtc"), "error: {message}");
    }

    #[test]
    fn should_reject_a_base64_image_declared_with_a_non_image_media_type() {
        // Given an image part whose base64 source declares a non-image
        // media type (a PDF passed off as an image)
        let files = FileStore::new("/nonexistent-files-root");
        let error = normalize_part(
            json!({
                "type": "image",
                "source": { "kind": "base64", "media_type": "application/pdf", "data": "aGk=" }
            }),
            &files,
        )
        .unwrap_err();

        // When the error is rendered
        // Then it is an invalid-params error at the entry (F-P4-2) — never
        // an opaque engine/model failure later
        let message = error.to_string();
        assert!(message.contains("image/"), "error: {message}");
        assert!(message.contains("application/pdf"), "error: {message}");
    }

    #[test]
    fn should_reject_a_file_source_whose_media_type_mismatches_the_part_type() {
        // Given an image part referencing a files-store entry whose
        // recorded media type is audio
        let files_dir = tempfile::tempdir().unwrap();
        let files = FileStore::new(files_dir.path());
        let meta = files
            .save(
                b"audio-bytes",
                "note.wav",
                crate::file_store::SaveOptions {
                    mime_type: Some("audio/wav".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
        let error = normalize_part(
            json!({
                "type": "image",
                "source": { "kind": "file", "file_id": meta.id }
            }),
            &files,
        )
        .unwrap_err();

        // When the error is rendered
        // Then the resolved media type fails the media-kind check (F-P4-2)
        let message = error.to_string();
        assert!(message.contains("image/"), "error: {message}");
        assert!(message.contains("audio/wav"), "error: {message}");
    }

    #[test]
    fn should_admit_a_file_source_whose_media_type_matches_the_part_type() {
        // Given a video part referencing a files-store entry recorded as
        // video/mp4
        let files_dir = tempfile::tempdir().unwrap();
        let files = FileStore::new(files_dir.path());
        let meta = files
            .save(
                b"video-bytes",
                "clip.mp4",
                crate::file_store::SaveOptions {
                    mime_type: Some("video/mp4".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
        let part = normalize_part(
            json!({
                "type": "video",
                "source": { "kind": "file", "file_id": meta.id }
            }),
            &files,
        )
        .unwrap();

        // When normalized
        // Then the resolved bytes inline as the canonical video part
        let url = part["video_url"].as_str().unwrap();
        assert!(url.starts_with("data:video/mp4;base64,"), "url: {url}");
    }

    #[test]
    fn should_reject_a_user_submitted_fileref_image_url_at_the_entry() {
        // Given a canonical image part whose URL is a forged fileref
        // (engine-internal, minted only by the media materializer)
        let files = FileStore::new("/nonexistent-files-root");

        // When the part is normalized
        let error = normalize_part(
            json!({
                "type": "image_url",
                "image_url": { "url": "fileref:anthropic;file-stolen-id" }
            }),
            &files,
        )
        .unwrap_err();

        // Then it is rejected as invalid params at the entry (T11a §4.6
        // red line — file ids must never be accepted from untrusted sources)
        let message = error.to_string();
        assert!(message.contains("fileref:"), "error: {message}");
    }

    #[test]
    fn should_reject_a_fileref_disguised_as_a_url_source_media_part() {
        // Given a v1 REST image part whose url source carries a fileref
        // (it would flow verbatim into the canonical image_url.url)
        let files = FileStore::new("/nonexistent-files-root");

        // When the part is normalized
        let error = normalize_part(
            json!({
                "type": "image",
                "source": { "kind": "url", "url": "fileref:anthropic;file-stolen-id" }
            }),
            &files,
        )
        .unwrap_err();

        // Then it is rejected as invalid params too (defense in depth)
        let message = error.to_string();
        assert!(message.contains("fileref:"), "error: {message}");
    }

    #[test]
    fn should_pass_a_blobref_image_url_through_when_normalizing() {
        // Given a canonical image part whose URL is a blobref (the resume
        // chain's legal restored form — rehydration may hand these back)
        let raw = json!({
            "type": "image_url",
            "image_url": { "url": "blobref:image/png;sha256-deadbeef" }
        });
        let files = FileStore::new("/nonexistent-files-root");

        // When the part is normalized
        let normalized = normalize_part(raw.clone(), &files).unwrap();

        // Then it passes through untouched (only fileref is the red line)
        assert_eq!(normalized, raw);
    }

    #[test]
    fn should_admit_a_remote_image_url_when_it_contains_a_fileref_substring() {
        // Given a canonical image part whose remote URL merely contains
        // the "fileref:" substring mid-URL (it is not a fileref scheme)
        let raw = json!({
            "type": "image_url",
            "image_url": { "url": "https://cdn.example.com/fileref:glyph.png" }
        });
        let files = FileStore::new("/nonexistent-files-root");

        // When the part is normalized
        let normalized = normalize_part(raw.clone(), &files).unwrap();

        // Then it is accepted untouched — the red line is a leading
        // `fileref:` scheme only, never a mid-URL substring (F-T11a-2)
        assert_eq!(normalized, raw);
    }

    #[test]
    fn should_admit_a_url_source_media_part_when_its_url_contains_a_fileref_substring() {
        // Given a v1 REST image part whose url source contains the
        // "fileref:" substring mid-URL
        let files = FileStore::new("/nonexistent-files-root");

        // When the part is normalized
        let part = normalize_part(
            json!({
                "type": "image",
                "source": { "kind": "url", "url": "https://cdn.example.com/fileref:glyph.png" }
            }),
            &files,
        )
        .unwrap();

        // Then the URL flows verbatim into the canonical image part (the
        // same prefix-only red line as the canonical branch, F-T11a-2)
        assert_eq!(
            part,
            json!({
                "type": "image_url",
                "image_url": { "url": "https://cdn.example.com/fileref:glyph.png" }
            })
        );
    }
}
