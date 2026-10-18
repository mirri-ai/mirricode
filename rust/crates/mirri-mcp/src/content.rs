//! MCP content block conversion.
//!
//! MCP servers return tool results as a list of [`McpContentBlock`] values.
//! These need to be converted into a form the rest of the system can consume.
//!
//! To avoid a circular dependency on `mirri-core`, this module provides a
//! lightweight conversion that returns `(text, Option<(mime_type, base64_data)>)`
//! pairs. Callers in `mirri-core` or `mirri-server` can then construct their
//! own `ContentPart` types from these tuples.
//!
//! ## Conversion rules
//!
//! | MCP block  | Text portion                          | Image portion                |
//! |------------|---------------------------------------|------------------------------|
//! | Text       | the text                              | None                         |
//! | Image      | `"[image: <mime_type>]"` placeholder  | Some((mime_type, data))      |
//! | Audio      | `"[audio: <mime_type>]"` placeholder  | None (audio has no image)    |
//! | Resource   | extracted text or JSON of the resource| None                         |

use crate::types::{McpContentBlock, McpEmbeddedResourceContents};

/// The result of converting a single MCP content block.
///
/// `text` is always present (possibly a placeholder). `image` is present
/// only for image blocks, containing `(mime_type, base64_data)`.
pub type ConvertedContent = (String, Option<(String, String)>);

/// Convert an MCP content block into a `(text, Option<(mime_type, base64_data)>)` pair.
///
/// This function does not depend on `mirri-core` types; it produces a
/// minimal intermediate representation that the caller can map to whatever
/// content-part type the rest of the system uses.
pub fn convert_content_block(block: &McpContentBlock) -> ConvertedContent {
    match block {
        McpContentBlock::Text { text } => (text.clone(), None),

        McpContentBlock::Image { data, mime_type } => (
            format!("[image: {mime_type}]"),
            Some((mime_type.clone(), data.clone())),
        ),

        McpContentBlock::Audio { data, mime_type } => {
            // Audio is not an image, but we include the base64 data with the
            // audio mime type so the caller can decide how to handle it.
            // We use the text field as a descriptive placeholder.
            (
                format!("[audio: {mime_type}]"),
                Some((mime_type.clone(), data.clone())),
            )
        }

        McpContentBlock::Resource { resource } => (convert_resource_to_text(resource), None),
    }
}

/// Convert a list of MCP content blocks, returning the concatenated text
/// and the first image (if any).
///
/// This is the common case for tool results: most tools return a single
/// text block, but some may return text + image.
pub fn convert_content_blocks(blocks: &[McpContentBlock]) -> ConvertedContent {
    let mut texts = Vec::new();
    let mut image: Option<(String, String)> = None;

    for block in blocks {
        let (text, img) = convert_content_block(block);
        texts.push(text);
        if image.is_none() {
            image = img;
        }
    }

    (texts.join("\n"), image)
}

/// Extract a text representation from an embedded resource.
///
/// - Text resources: return the text content.
/// - Blob resources: return a placeholder with the URI.
fn convert_resource_to_text(resource: &McpEmbeddedResourceContents) -> String {
    if let Some(text) = &resource.text {
        text.clone()
    } else if let Some(blob) = &resource.blob {
        format!("[resource blob: {} ({} bytes)]", resource.uri, blob.len())
    } else {
        format!("[resource: {}]", resource.uri)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_return_text_when_converting_text_block() {
        let block = McpContentBlock::Text {
            text: "hello".to_string(),
        };
        let (text, image) = convert_content_block(&block);
        assert_eq!(text, "hello");
        assert!(image.is_none());
    }

    #[test]
    fn should_return_image_data_when_converting_image_block() {
        let block = McpContentBlock::Image {
            data: "iVBORw0KGgo=".to_string(),
            mime_type: "image/png".to_string(),
        };
        let (text, image) = convert_content_block(&block);
        assert_eq!(text, "[image: image/png]");
        assert!(image.is_some());
        let (mime, data) = image.unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(data, "iVBORw0KGgo=");
    }

    #[test]
    fn should_return_audio_placeholder_when_converting_audio_block() {
        let block = McpContentBlock::Audio {
            data: "UklGRiQ=".to_string(),
            mime_type: "audio/wav".to_string(),
        };
        let (text, image) = convert_content_block(&block);
        assert_eq!(text, "[audio: audio/wav]");
        assert!(image.is_some());
    }

    #[test]
    fn should_return_text_content_when_resource_has_text() {
        let block = McpContentBlock::Resource {
            resource: McpEmbeddedResourceContents {
                uri: "file:///path/to/file.txt".to_string(),
                mime_type: Some("text/plain".to_string()),
                text: Some("file contents".to_string()),
                blob: None,
            },
        };
        let (text, image) = convert_content_block(&block);
        assert_eq!(text, "file contents");
        assert!(image.is_none());
    }

    #[test]
    fn should_return_blob_placeholder_when_resource_has_blob() {
        let block = McpContentBlock::Resource {
            resource: McpEmbeddedResourceContents {
                uri: "file:///path/to/binary.bin".to_string(),
                mime_type: None,
                text: None,
                blob: Some("AAAA".to_string()),
            },
        };
        let (text, _) = convert_content_block(&block);
        assert!(text.contains("file:///path/to/binary.bin"));
        assert!(text.contains("resource blob"));
    }

    #[test]
    fn should_concatenate_text_when_converting_multiple_blocks() {
        let blocks = vec![
            McpContentBlock::Text {
                text: "line 1".to_string(),
            },
            McpContentBlock::Text {
                text: "line 2".to_string(),
            },
        ];
        let (text, image) = convert_content_blocks(&blocks);
        assert_eq!(text, "line 1\nline 2");
        assert!(image.is_none());
    }

    #[test]
    fn should_return_first_image_when_converting_multiple_blocks() {
        let blocks = vec![
            McpContentBlock::Text {
                text: "description".to_string(),
            },
            McpContentBlock::Image {
                data: "img1".to_string(),
                mime_type: "image/png".to_string(),
            },
            McpContentBlock::Image {
                data: "img2".to_string(),
                mime_type: "image/jpeg".to_string(),
            },
        ];
        let (text, image) = convert_content_blocks(&blocks);
        assert!(text.contains("description"));
        assert!(text.contains("[image: image/png]"));
        assert!(image.is_some());
        let (mime, data) = image.unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(data, "img1");
    }

    #[test]
    fn should_return_empty_text_when_blocks_list_is_empty() {
        let (text, image) = convert_content_blocks(&[]);
        assert_eq!(text, "");
        assert!(image.is_none());
    }
}
