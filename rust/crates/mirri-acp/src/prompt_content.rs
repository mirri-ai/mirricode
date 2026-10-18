//! ACP v1 prompt `ContentBlock`s → engine `message.send` input.
//!
//! The v1 `session/prompt` `prompt` parameter is an array of
//! [`ContentBlock`](https://agentclientprotocol.com/protocol/content)s.
//! The advertised capability surface decides what is honored
//! (`promptCapabilities: {image: true, audio: false, embeddedContext:
//! true}`, matching the TS host):
//!
//! - `text` — the canonical text part (a prompt of text blocks only keeps
//!   the plain-`text` wire spelling, byte-identical to the pre-v1 single
//!   string form);
//! - `image` — converted to the canonical `image_url` part carrying the
//!   `data:{mimeType};base64,{data}` URI (the client-declared MIME rides
//!   along verbatim; the engine's turn pipeline owns the wire-log blob
//!   offload, the P4 media infrastructure). A MIME that is not an
//!   `image/*` type is rejected as invalid params — an honest entry gate,
//!   the rust analogue of the TS `assertMediaFile` check that refuses a
//!   PDF sent as an image (F-P4-2);
//! - `audio` — **warn-dropped** (the capability is advertised `false`,
//!   TS parity);
//! - `resource_link` — a `file:` URI becomes the plain path text ref
//!   (with an optional line range, e.g. `file:///a/b.rs#L3-L7` →
//!   `/a/b.rs:3-7`); anything else is wrapped as a `<resource_link … />`
//!   text part so the model sees the reference (TS `convert.ts` parity);
//! - `resource` — a text resource is wrapped as a `<resource uri=…>`
//!   text part (the URI provenance rides along); a blob resource is
//!   **warn-dropped** (the TS D3 drop+warn rule — the same posture the
//!   TS host keeps under `embeddedContext: true`);
//! - unknown block kinds — warn-dropped (future-proof, TS parity).
//!
//! Prompt-image compression (the TS `compressPromptImageParts` step:
//! re-encode oversized images, prepend a caption, persist the original)
//! has **no rust engine counterpart yet** — the conversion here passes
//! the bytes through untouched and the engine's wire-log `blobref:`
//! offload keeps the persisted log bounded (see the P5f report's
//! followup list).

use serde_json::{Value, json};

/// JSON-RPC error: invalid method parameters.
const ERROR_INVALID_PARAMS: i64 = -32602;

/// The converted prompt payload for `message.send`: the plain-text fast
/// path or a full canonical part list.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EnginePromptInput {
    /// A prompt made of text blocks only — sent as the `text` parameter
    /// (byte-identical to the pre-v1 single-string spelling).
    Text(String),
    /// A prompt carrying non-text blocks — sent as the `input` parameter
    /// (the engine's canonical content-part array).
    Parts(Vec<Value>),
}

/// Convert the ACP `prompt` parameter (a bare string or an array of v1
/// `ContentBlock`s) into the engine's `message.send` input (see the
/// module docs for the per-kind treatment).
pub(crate) fn convert_prompt(prompt: &Value) -> Result<EnginePromptInput, (i64, String)> {
    match prompt {
        Value::String(text) => Ok(EnginePromptInput::Text(text.clone())),
        Value::Array(blocks) => {
            if blocks.is_empty() {
                return Err((
                    ERROR_INVALID_PARAMS,
                    "invalid params: 'prompt' must carry at least one content block".to_string(),
                ));
            }
            let mut text_only = String::new();
            let mut parts = Vec::new();
            let mut has_non_text = false;
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        text_only.push_str(text);
                        parts.push(json!({"type": "text", "text": text}));
                    }
                    Some("image") => {
                        has_non_text = true;
                        parts.push(image_part(block)?);
                    }
                    Some("audio") => {
                        // Advertised `audio: false` — the block is dropped
                        // loudly, never silently (TS parity).
                        tracing::warn!(
                            mimeType = block.get("mimeType").and_then(|v| v.as_str()).unwrap_or(""),
                            "dropping unsupported audio prompt block (audio capability is false)"
                        );
                    }
                    Some("resource_link") => {
                        has_non_text = true;
                        let uri = block.get("uri").and_then(Value::as_str).unwrap_or_default();
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let text = match file_link_to_text_ref(uri) {
                            Some(reference) => reference,
                            None => format!(
                                "<resource_link uri=\"{}\" name=\"{}\" />",
                                escape_xml_attr(uri),
                                escape_xml_attr(name)
                            ),
                        };
                        text_only.push_str(&text);
                        parts.push(json!({"type": "text", "text": text}));
                    }
                    Some("resource") => {
                        let resource = block.get("resource").cloned().unwrap_or(Value::Null);
                        if let Some(text) = resource.get("text").and_then(Value::as_str) {
                            has_non_text = true;
                            let uri = resource
                                .get("uri")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            let wrapped = format!(
                                "<resource uri=\"{}\">{}</resource>",
                                escape_xml_attr(uri),
                                text
                            );
                            text_only.push_str(&wrapped);
                            parts.push(json!({"type": "text", "text": wrapped}));
                        } else {
                            // BlobResourceContents — drop + warn (TS D3 rule).
                            tracing::warn!(
                                uri = resource.get("uri").and_then(|v| v.as_str()).unwrap_or(""),
                                "dropping blob embedded resource from the prompt"
                            );
                        }
                    }
                    other => {
                        // Future-proof: an unknown block kind is dropped
                        // with a warning, never a crash (TS parity).
                        tracing::warn!(
                            kind = other.unwrap_or("<missing>"),
                            "dropping unknown prompt content block"
                        );
                    }
                }
            }
            if !has_non_text {
                if text_only.is_empty() {
                    return Err((
                        ERROR_INVALID_PARAMS,
                        "invalid params: 'prompt' carries no supported content blocks".to_string(),
                    ));
                }
                return Ok(EnginePromptInput::Text(text_only));
            }
            if parts.is_empty() {
                return Err((
                    ERROR_INVALID_PARAMS,
                    "invalid params: 'prompt' carries no supported content blocks".to_string(),
                ));
            }
            Ok(EnginePromptInput::Parts(parts))
        }
        _ => Err((
            ERROR_INVALID_PARAMS,
            "invalid params: 'prompt' must be a string or an array of content blocks".to_string(),
        )),
    }
}

/// Convert one v1 `ImageContent` block into the engine's canonical
/// `image_url` part, rejecting a non-`image/*` MIME at the entry (the
/// ACP-side face of F-P4-2: a PDF sent as an image is a client error,
/// not a payload the engine should choke on later).
fn image_part(block: &Value) -> Result<Value, (i64, String)> {
    let data = block.get("data").and_then(Value::as_str).ok_or_else(|| {
        (
            ERROR_INVALID_PARAMS,
            "invalid params: an image block requires base64 'data'".to_string(),
        )
    })?;
    let mime_type = block
        .get("mimeType")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            (
                ERROR_INVALID_PARAMS,
                "invalid params: an image block requires 'mimeType'".to_string(),
            )
        })?;
    if !mime_type.to_ascii_lowercase().starts_with("image/") {
        return Err((
            ERROR_INVALID_PARAMS,
            format!(
                "invalid params: an image block's mimeType '{mime_type}' is not an image/* type"
            ),
        ));
    }
    Ok(json!({
        "type": "image_url",
        "image_url": {"url": format!("data:{mime_type};base64,{data}")},
    }))
}

/// Project a `file:` `resource_link` URI onto the plain text ref the model
/// reads (TS `fileLinkToTextRef`): the percent-decoded path with an
/// optional line range (`#L3-L7` / `?lines=3-7` / `?line=3` → `:3-7`).
/// Any other scheme returns `None` (the caller wraps it instead).
fn file_link_to_text_ref(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    // Split the fragment / query off before decoding the path.
    let (path, suffix) = match rest.find(['#', '?']) {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    let decoded = percent_decode(path);
    let range = parse_line_range(suffix);
    Some(match range {
        Some(range) => format!("{decoded}:{range}"),
        None => decoded,
    })
}

/// Percent-decode a URI path component (best effort: an invalid escape
/// passes through verbatim).
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = input.get(index + 1..index + 3).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse a `#L3-L7` / `?lines=3-7` / `?line=3` suffix into its `3-7` / `3`
/// text-ref range form (TS `parseLineRange`).
fn parse_line_range(suffix: &str) -> Option<String> {
    let body = suffix.strip_prefix(['#', '?']).unwrap_or(suffix);
    let body = body
        .strip_prefix("lines=")
        .or_else(|| body.strip_prefix("line="))
        .unwrap_or(body);
    let body = body.strip_prefix('L').unwrap_or(body);
    let (first, second) = match body.split_once(['-', ':']) {
        Some((first, second)) => (first, Some(second)),
        None => (body, None),
    };
    let first = first.strip_prefix('L').unwrap_or(first);
    let first: u64 = first.parse().ok()?;
    let second = match second {
        Some(second) => {
            let second: u64 = second.strip_prefix('L').unwrap_or(second).parse().ok()?;
            Some(second)
        }
        None => None,
    };
    Some(match second {
        Some(second) => format!("{first}-{second}"),
        None => first.to_string(),
    })
}

/// Minimum-viable XML-attribute escaping for the prompt-embedded resource
/// wrappers (TS parity: only the five characters that would change the
/// apparent tag structure; the consumer is an LLM, not an XML parser).
fn escape_xml_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- text ------------------------------------------------------------

    /// Given a bare string prompt, when converted, then it stays the
    /// plain `text` spelling.
    #[test]
    fn should_pass_a_string_prompt_through_as_text() {
        assert_eq!(
            convert_prompt(&json!("hello")).unwrap(),
            EnginePromptInput::Text("hello".to_string())
        );
    }

    /// Given a prompt of text blocks only, when converted, then the parts
    /// concatenate into the plain `text` spelling (the byte-identical
    /// fast path keeps the pure-text wire form).
    #[test]
    fn should_concatenate_text_only_blocks_into_the_text_spelling() {
        let prompt = json!([
            {"type": "text", "text": "hello "},
            {"type": "text", "text": "world"},
        ]);
        assert_eq!(
            convert_prompt(&prompt).unwrap(),
            EnginePromptInput::Text("hello world".to_string())
        );
    }

    /// Given a prompt that is neither a string nor an array, when
    /// converted, then it is rejected as invalid params.
    #[test]
    fn should_reject_a_prompt_that_is_neither_string_nor_array() {
        let (code, _) = convert_prompt(&json!(42)).unwrap_err();
        assert_eq!(code, ERROR_INVALID_PARAMS);
    }

    // --- image -----------------------------------------------------------

    /// Given an image block, when converted, then it becomes the
    /// canonical `image_url` part carrying the data URL.
    #[test]
    fn should_convert_an_image_block_into_an_image_url_part() {
        let prompt = json!([
            {"type": "text", "text": "look at this:"},
            {"type": "image", "mimeType": "image/png", "data": "aGk="},
        ]);
        assert_eq!(
            convert_prompt(&prompt).unwrap(),
            EnginePromptInput::Parts(vec![
                json!({"type": "text", "text": "look at this:"}),
                json!({
                    "type": "image_url",
                    "image_url": {"url": "data:image/png;base64,aGk="},
                }),
            ])
        );
    }

    /// Given an image block whose MIME is not an image type (a PDF sent as
    /// an image), when converted, then the prompt is rejected with invalid
    /// params — the entry gate the advertised `image: true` capability
    /// must back (F-P4-2).
    #[test]
    fn should_reject_an_image_block_whose_mime_is_not_an_image_type() {
        let prompt = json!([
            {"type": "image", "mimeType": "application/pdf", "data": "aGk="},
        ]);
        let (code, message) = convert_prompt(&prompt).unwrap_err();
        assert_eq!(code, ERROR_INVALID_PARAMS);
        assert!(message.contains("application/pdf"), "error: {message}");
    }

    /// Given an image block missing its base64 data or MIME, when
    /// converted, then it is rejected as invalid params.
    #[test]
    fn should_reject_a_malformed_image_block() {
        for prompt in [
            json!([{"type": "image", "mimeType": "image/png"}]),
            json!([{"type": "image", "data": "aGk="}]),
        ] {
            let (code, _) = convert_prompt(&prompt).unwrap_err();
            assert_eq!(code, ERROR_INVALID_PARAMS);
        }
    }

    // --- audio / blob / unknown ------------------------------------------

    /// Given an audio block (the `audio` capability is advertised false),
    /// when converted, then it is dropped and the surviving text keeps
    /// the plain spelling.
    #[test]
    fn should_drop_an_audio_block_and_keep_the_text() {
        let prompt = json!([
            {"type": "audio", "mimeType": "audio/wav", "data": "aGk="},
            {"type": "text", "text": "hi"},
        ]);
        assert_eq!(
            convert_prompt(&prompt).unwrap(),
            EnginePromptInput::Text("hi".to_string())
        );
    }

    /// Given a blob embedded resource, when converted, then it is
    /// dropped (the text resource is the supported embedded context).
    #[test]
    fn should_drop_a_blob_resource_and_wrap_a_text_resource() {
        let prompt = json!([
            {"type": "resource", "resource": {
                "uri": "file:///blob.bin",
                "mimeType": "application/octet-stream",
                "blob": "aGk=",
            }},
            {"type": "resource", "resource": {
                "uri": "file:///notes.txt",
                "mimeType": "text/plain",
                "text": "the notes",
            }},
        ]);
        assert_eq!(
            convert_prompt(&prompt).unwrap(),
            EnginePromptInput::Parts(vec![json!({
                "type": "text",
                "text": "<resource uri=\"file:///notes.txt\">the notes</resource>",
            })])
        );
    }

    /// Given an unknown block kind, when converted, then it is dropped
    /// without failing the request.
    #[test]
    fn should_drop_an_unknown_block_kind() {
        let prompt = json!([
            {"type": "hologram", "data": "..."},
            {"type": "text", "text": "still here"},
        ]);
        assert_eq!(
            convert_prompt(&prompt).unwrap(),
            EnginePromptInput::Text("still here".to_string())
        );
    }

    // --- resource_link -----------------------------------------------------

    /// Given a `file:` resource link with a line range, when converted,
    /// then it becomes the plain text ref the model reads.
    #[test]
    fn should_convert_a_file_resource_link_into_a_text_ref() {
        let prompt = json!([
            {"type": "resource_link", "uri": "file:///a/b.rs#L3-L7", "name": "b.rs"},
        ]);
        assert_eq!(
            convert_prompt(&prompt).unwrap(),
            EnginePromptInput::Parts(vec![json!({
                "type": "text",
                "text": "/a/b.rs:3-7",
            })])
        );
    }

    /// Given a non-file resource link, when converted, then it is wrapped
    /// as a `<resource_link>` text part (the model sees the reference).
    #[test]
    fn should_wrap_a_non_file_resource_link() {
        let prompt = json!([
            {"type": "resource_link", "uri": "db://rows?q=1&x=<y>", "name": "the rows"},
        ]);
        assert_eq!(
            convert_prompt(&prompt).unwrap(),
            EnginePromptInput::Parts(vec![json!({
                "type": "text",
                "text": "<resource_link uri=\"db://rows?q=1&amp;x=&lt;y&gt;\" name=\"the rows\" />",
            })])
        );
    }

    /// Given an empty prompt array (or one whose every block was dropped),
    /// when converted, then it is rejected as invalid params.
    #[test]
    fn should_reject_an_empty_or_fully_dropped_prompt() {
        let (code, _) = convert_prompt(&json!([])).unwrap_err();
        assert_eq!(code, ERROR_INVALID_PARAMS);
        let (code, _) = convert_prompt(&json!([
            {"type": "audio", "mimeType": "audio/wav", "data": "aGk="},
        ]))
        .unwrap_err();
        assert_eq!(code, ERROR_INVALID_PARAMS);
    }

    // --- file-link helpers -------------------------------------------------

    /// The line-range parser mirrors the TS `parseLineRange` shapes.
    #[test]
    fn should_parse_the_ts_line_range_suffixes() {
        assert_eq!(parse_line_range("#L3"), Some("3".to_string()));
        assert_eq!(parse_line_range("#L3-L7"), Some("3-7".to_string()));
        assert_eq!(parse_line_range("?lines=3-7"), Some("3-7".to_string()));
        assert_eq!(parse_line_range("?line=3"), Some("3".to_string()));
        assert_eq!(parse_line_range("#L3:L7"), Some("3-7".to_string()));
        assert_eq!(parse_line_range(""), None);
        assert_eq!(parse_line_range("#anchor"), None);
    }

    /// Percent escapes decode in file-link paths.
    #[test]
    fn should_percent_decode_file_link_paths() {
        assert_eq!(
            file_link_to_text_ref("file:///a/b%20c.rs#L1").unwrap(),
            "/a/b c.rs:1"
        );
        assert_eq!(file_link_to_text_ref("https://example/x"), None);
    }
}
