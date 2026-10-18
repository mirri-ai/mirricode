use serde::{Deserialize, Serialize};

/// Message role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A content part within a message.
///
/// These are the wire-level content-block shapes sent to and received from
/// chat providers. The `type` discriminator matches the TS `ContentPart`
/// union.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "think")]
    Think { text: String },
    #[serde(rename = "image_url")]
    ImageUrl {
        image_url: ImageUrlData,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    #[serde(rename = "audio_url")]
    AudioUrl { audio_url: String },
    #[serde(rename = "video_url")]
    VideoUrl { video_url: String },
    /// A generic file attachment (the v1 TS REST `type: "file"` part).
    ///
    /// Two spellings share the variant: the inbound wire form carries the
    /// files-store reference (`file_id`, plus the display metadata), and the
    /// context-memory form carries the resolved inline content (`data`, a
    /// `data:<mime>;base64,...` URI). The `message.send` entry resolves the
    /// former into the latter — a bare `file_id` is never persisted into
    /// context memory, so a turn survives the files store's expiry.
    #[serde(rename = "file")]
    File {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<u64>,
    },
}

/// The nested `image_url` payload of an [`ContentPart::ImageUrl`] part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageUrlData {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Hidden archive-path channel (multimodal spec E2 / ADR-004): the
    /// filesystem path of the persisted original bytes, written at prompt
    /// ingestion and consumed by capability-gap placeholders and subagent
    /// delegation. Never sent to the provider; absent for remote URLs that
    /// pass through unarchived. Old records without this field parse
    /// with `id == None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// A tool call within an assistant message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned call identifier (e.g. `"call_abc123"`).
    pub id: String,
    /// Tool-call discriminator. Always `"function"` for OpenAI-style tool
    /// calls; serialized as `type` to match the wire format.
    #[serde(rename = "type", default = "default_tool_call_type")]
    pub call_type: String,
    pub function: ToolCallFunction,
}

fn default_tool_call_type() -> String {
    "function".into()
}

/// The `function` sub-object of a [`ToolCall`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    /// Raw JSON string of arguments (as sent by the provider).
    pub arguments: String,
}

/// A rewritten-user-message skills attachment (ADR-006).
///
/// Rewritten user messages optionally carry a `with_skills` array whose
/// entries name the skills wired into the rewrite. Product-record only —
/// never forwarded to the model as context. Field names are all single
/// words, matching the TS wire key `with_skills` (snake_case on the wire;
/// the fields themselves need no renaming).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WithSkillsEntry {
    pub name: String,
    pub target: String,
    pub trigger: String,
}

/// A wire-level message (the shape sent to / received from providers).
///
/// Field names serialize to `camelCase` to match the TS wire format persisted
/// in `context.jsonl` via `context.append_message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub content: Vec<ContentPart>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Partial/streaming flag — message was interrupted mid-stream.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            name: None,
            content: vec![ContentPart::Text { text: text.into() }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            partial: false,
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            name: None,
            content: vec![ContentPart::Text { text: text.into() }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            partial: false,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            name: None,
            content: vec![ContentPart::Text {
                text: output.into(),
            }],
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            partial: false,
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            name: None,
            content: vec![ContentPart::Text { text: text.into() }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            partial: false,
        }
    }

    /// Whether the message carries no visible content and no tool calls.
    pub fn is_empty(&self) -> bool {
        self.content.is_empty() && self.tool_calls.is_empty()
    }

    /// Get all text content joined together with newlines.
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_round_trip_file_part_when_serializing_inbound_wire_form() {
        // Given a file part in the inbound wire form (the v1 TS REST shape)
        let raw = serde_json::json!({
            "type": "file",
            "file_id": "f_abc123",
            "name": "notes.pdf",
            "media_type": "application/pdf",
            "size": 1024,
        });

        // When it deserializes into a ContentPart and back
        let part: ContentPart = serde_json::from_value(raw.clone()).expect("should parse");
        let round: serde_json::Value = serde_json::to_value(&part).expect("should serialize");

        // Then the round trip preserves the wire shape (minus absent fields)
        assert_eq!(round, raw);
        match part {
            ContentPart::File {
                file_id,
                data,
                name,
                media_type,
                size,
            } => {
                assert_eq!(file_id.as_deref(), Some("f_abc123"));
                assert_eq!(data, None);
                assert_eq!(name.as_deref(), Some("notes.pdf"));
                assert_eq!(media_type.as_deref(), Some("application/pdf"));
                assert_eq!(size, Some(1024));
            }
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[test]
    fn should_round_trip_file_part_when_serializing_context_memory_form() {
        // Given a file part in the context-memory form (resolved inline data)
        let part = ContentPart::File {
            file_id: None,
            data: Some("data:application/pdf;base64,AAECAw==".to_string()),
            name: Some("notes.pdf".to_string()),
            media_type: Some("application/pdf".to_string()),
            size: Some(4),
        };

        // When it serializes to the wire shape
        let wire = serde_json::to_value(&part).expect("should serialize");
        assert_eq!(
            wire,
            serde_json::json!({
                "type": "file",
                "data": "data:application/pdf;base64,AAECAw==",
                "name": "notes.pdf",
                "media_type": "application/pdf",
                "size": 4,
            })
        );

        // Then it deserializes back into the same part
        let back: ContentPart = serde_json::from_value(wire).expect("should parse");
        assert_eq!(back, part);
    }

    #[test]
    fn should_parse_minimal_file_part_when_only_file_id_present() {
        // Given a bare file reference without display metadata
        let raw = serde_json::json!({ "type": "file", "file_id": "f_x" });

        // When it deserializes
        let part: ContentPart = serde_json::from_value(raw).expect("should parse");

        // Then the optional fields are absent
        match part {
            ContentPart::File {
                file_id,
                data,
                name,
                media_type,
                size,
            } => {
                assert_eq!(file_id.as_deref(), Some("f_x"));
                assert_eq!(data, None);
                assert_eq!(name, None);
                assert_eq!(media_type, None);
                assert_eq!(size, None);
            }
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[test]
    fn should_leave_existing_part_wire_shapes_unchanged_when_file_variant_added() {
        // Given the pre-existing wire shapes
        let raw = serde_json::json!([
            { "type": "text", "text": "hi" },
            { "type": "image_url", "image_url": { "url": "https://example.com/a.png" } },
            { "type": "audio_url", "audio_url": "https://example.com/a.wav" },
            { "type": "video_url", "video_url": "https://example.com/a.mp4" },
        ]);

        // When they deserialize and re-serialize
        let parts: Vec<ContentPart> = serde_json::from_value(raw.clone()).expect("should parse");
        let back = serde_json::to_value(&parts).expect("should serialize");

        // Then the wire shapes are byte-identical (backward compatibility)
        assert_eq!(back, raw);
    }

    #[test]
    fn should_default_missing_id_to_none_for_legacy_image_parts() {
        // Given 一个不含 id 的历史形态 image 部件（skip_serializing_if 使
        // None 不出现在 JSON 中——恰为旧会话文件的实际形态）
        // When 反序列化回 ContentPart
        // Then id 为 None（旧文件兼容，wire append-only 红线）
        let part = ContentPart::ImageUrl {
            image_url: ImageUrlData {
                url: "data:image/png;base64,AAAA".into(),
                detail: None,
                id: None,
            },
            detail: None,
        };
        let json = serde_json::to_value(&part).unwrap();
        let inner = json.get("image_url").unwrap().as_object().unwrap();
        assert!(!inner.contains_key("id"), "None 不应序列化出 id 键");
        let back: ContentPart = serde_json::from_value(json).unwrap();
        assert_eq!(back, part);
    }

    #[test]
    fn should_roundtrip_image_part_with_id() {
        // Given 一个带留档路径 id 的 image 部件
        // When 序列化再反序列化
        // Then id 保留（context.jsonl 往返，US2.1 恢复句柄随历史流转）
        let part = ContentPart::ImageUrl {
            image_url: ImageUrlData {
                url: "data:image/png;base64,AAAA".into(),
                detail: None,
                id: Some("/home/u/.mirri-code/sessions/s1/blobs/ab12".into()),
            },
            detail: None,
        };
        let json = serde_json::to_value(&part).unwrap();
        assert_eq!(
            json["image_url"]["id"],
            "/home/u/.mirri-code/sessions/s1/blobs/ab12"
        );
        let back: ContentPart = serde_json::from_value(json).unwrap();
        assert_eq!(back, part);
    }
}
