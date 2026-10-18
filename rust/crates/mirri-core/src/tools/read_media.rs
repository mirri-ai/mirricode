//! `ReadMediaFileTool` — read an image file and return it as inline media.
//!
//! Mirrors the TypeScript `ReadMediaFile` tool: reads a local image file,
//! validates it by magic bytes, encodes the raw bytes as a base64 data URL,
//! and returns a [`ToolOutput::Parts`] containing text wrappers and an
//! `image_url` content part. Supported formats: PNG, JPEG, GIF, WebP.
//! BMP is detected but rejected with conversion guidance because most
//! providers do not accept it.

use std::path::Path;
use std::sync::Arc;

use base64::Engine;
use mirri_os::DynFileSystem;

use crate::error::EngineResult;
use crate::image_pipeline::{self, ImageFormat, Region, detect_image_format};
use crate::message::{ContentPart, ImageUrlData};
use crate::tool::{
    AccessKind, AccessOperation, ExecutionPlan, Tool, ToolAccess, ToolExecution, ToolInputDisplay,
    ToolOutput, ToolResult,
};
use crate::tools::tool_execution_error;
use crate::tools::{PathClassification, WorkspacePaths};

/// Maximum file size accepted by the tool (100 MB).
const MAX_MEDIA_BYTES: usize = 100 * 1024 * 1024;

/// Image MIME types that providers commonly accept.
const SUPPORTED_IMAGE_MIMES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// `ReadMediaFile` — read an image file and return it as inline media.
///
/// When constructed with a workspace sandbox ([`ReadMediaFileTool::new`]),
/// every path is validated against the workspace root or any registered
/// additional dir, and paths escaping all of them (traversal, absolute
/// outside paths, symlinks) are refused before any I/O.
pub struct ReadMediaFileTool {
    /// Workspace sandbox this tool may read inside; `None` disables
    /// validation.
    workspace_paths: Option<Arc<WorkspacePaths>>,
    /// The filesystem provider every media read goes through (zero-IO T13a).
    fs: Arc<dyn DynFileSystem>,
}

impl ReadMediaFileTool {
    /// Create a read-media tool confined to the `workspace_paths` sandbox,
    /// or `None` to disable workspace path validation. `fs` is the injected
    /// filesystem provider (the engine default is the local one).
    pub fn new(workspace_paths: Option<Arc<WorkspacePaths>>, fs: Arc<dyn DynFileSystem>) -> Self {
        Self {
            workspace_paths,
            fs,
        }
    }
}

impl Tool for ReadMediaFileTool {
    fn name(&self) -> &str {
        "ReadMediaFile"
    }

    fn description(&self) -> &str {
        "Read an image file from the local filesystem and return it as an inline image. \
         Supported formats: PNG, JPEG, GIF, WebP. Images are compressed (downscaled to \
         2048px, JPEG quality reduced) before delivery unless 'full_resolution' is true; \
         'region' crops to {x, y, width, height} first. The pristine original is kept in \
         the temp dir and its path is reported in the note. Maximum file size: 100 MB."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the image file. Relative paths resolve against the working directory; paths outside must be absolute. Paths must be inside the workspace sandbox (the working directory or an authorized additional directory); paths outside it require user authorization."
                    },
                    "region": {
                        "type": "object",
                        "properties": {
                            "x": { "type": "integer", "minimum": 0, "description": "Left edge of the crop, in pixels." },
                            "y": { "type": "integer", "minimum": 0, "description": "Top edge of the crop, in pixels." },
                            "width": { "type": "integer", "minimum": 1, "description": "Crop width in pixels." },
                            "height": { "type": "integer", "minimum": 1, "description": "Crop height in pixels." }
                        },
                        "required": ["x", "y", "width", "height"],
                        "description": "Crop the image to this region before encoding. The region is clamped to the image bounds."
                    },
                    "full_resolution": {
                        "type": "boolean",
                        "description": "When true, deliver the original bytes untouched (no compression, no resizing). Defaults to false."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            })
        })
    }

    fn capabilities(&self) -> &[String] {
        static CAPABILITIES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
        CAPABILITIES.get_or_init(|| vec!["code.read".to_string()])
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let Some(path) = input.get("path").and_then(serde_json::Value::as_str) else {
            return Ok(tool_execution_error(
                "ReadMediaFile requires a string 'path' argument",
            ));
        };
        if path.is_empty() {
            return Ok(tool_execution_error(
                "ReadMediaFile requires a non-empty 'path' argument",
            ));
        }
        let path = path.to_string();
        // Classify against the workspace sandbox before any I/O (issue 15D,
        // plan 2026-10-10-sandbox-path-permission D1). Inside and Outside
        // both build the plan over the canonical path (Outside enters the
        // permission chain instead of being refused here); only escape
        // shapes are refused.
        let file_path = match crate::tools::classify_tool_path(
            self.workspace_paths.as_deref(),
            &self.fs,
            &path,
        ) {
            PathClassification::Inside(canonical) | PathClassification::Outside(canonical) => {
                canonical
            }
            PathClassification::Escape { reason } => {
                return Ok(tool_execution_error(crate::tools::escape_tool_error(
                    "ReadMediaFile",
                    &path,
                    &reason,
                )));
            }
        };
        let path = file_path.to_string_lossy().into_owned();
        let region = input.get("region").and_then(Region::from_json);
        // A crop with zero width/height is meaningless — refuse up front.
        if let Some(region) = region
            && (region.width == 0 || region.height == 0)
        {
            return Ok(tool_execution_error(
                "ReadMediaFile 'region' must have positive width and height",
            ));
        }
        let full_resolution = input
            .get("full_resolution")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let plan = ExecutionPlan {
            accesses: vec![ToolAccess {
                kind: AccessKind::File,
                path: Some(path.clone()),
                operation: AccessOperation::Read,
            }],
            display: ToolInputDisplay::FileIo {
                path: path.clone(),
                // The protocol `file_io.operation` enum only accepts
                // `read|write|edit|glob|grep`; the path already identifies
                // the media file, so the semantic operation is `read`.
                operation: "read".to_string(),
            },
            approval_rule: "ReadMediaFile".to_string(),
            stop_batch_after_this: false,
        };

        let fs = Arc::clone(&self.fs);

        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |_ctx| {
                Box::pin(read_media_file(fs, path, region, full_resolution))
            }),
        })
    }
}

/// Image dimensions (width × height in pixels).
struct ImageDimensions {
    width: u32,
    height: u32,
}

/// Sniff image dimensions from the file header bytes.
///
/// Parses the binary header for PNG, GIF, WebP, and BMP to extract
/// width and height. JPEG dimensions require scanning SOF markers
/// and may fail on some files — returns `None` in that case.
fn sniff_dimensions(bytes: &[u8], format: &ImageFormat) -> Option<ImageDimensions> {
    match format {
        ImageFormat::Png => {
            // PNG IHDR chunk: width (4 bytes big-endian) at offset 16,
            // height (4 bytes big-endian) at offset 20.
            if bytes.len() >= 24 {
                let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
                let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
                return Some(ImageDimensions { width, height });
            }
            None
        }
        ImageFormat::Gif => {
            // GIF logical screen descriptor: width (2 bytes little-endian)
            // at offset 6, height (2 bytes little-endian) at offset 8.
            if bytes.len() >= 10 {
                let width = u16::from_le_bytes([bytes[6], bytes[7]]) as u32;
                let height = u16::from_le_bytes([bytes[8], bytes[9]]) as u32;
                return Some(ImageDimensions { width, height });
            }
            None
        }
        ImageFormat::Webp => {
            // WebP VP8/VP8L/VP8X: dimensions are at different offsets
            // depending on the sub-format. VP8X (extended) at offset 24-27,
            // VP8 (lossy) at offset 26-29, VP8L (lossless) at offset 21-24.
            if bytes.len() >= 30 {
                let chunk_type = &bytes[12..16];
                match chunk_type {
                    b"VP8X" => {
                        // VP8X: width-1 at offset 24 (3 bytes LE), height-1 at 27 (3 bytes LE)
                        let width = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], 0]) + 1;
                        let height = u32::from_le_bytes([bytes[27], bytes[28], bytes[29], 0]) + 1;
                        Some(ImageDimensions { width, height })
                    }
                    b"VP8 " => {
                        // VP8 (lossy): width at offset 26 (2 bytes LE), height at 28
                        let width = u16::from_le_bytes([bytes[26], bytes[27]]) as u32;
                        let height = u16::from_le_bytes([bytes[28], bytes[29]]) as u32;
                        Some(ImageDimensions { width, height })
                    }
                    b"VP8L" => {
                        // VP8L (lossless): width-1 and height-1 packed in
                        // a 14-bit pair at offset 21. This is trickier to
                        // parse; skip for now.
                        None
                    }
                    _ => None,
                }
            } else {
                None
            }
        }
        ImageFormat::Bmp => {
            // BMP DIB header: width (4 bytes LE) at offset 18,
            // height (4 bytes LE) at offset 22.
            if bytes.len() >= 26 {
                let width = u32::from_le_bytes([bytes[18], bytes[19], bytes[20], bytes[21]]);
                let height = u32::from_le_bytes([bytes[22], bytes[23], bytes[24], bytes[25]]);
                return Some(ImageDimensions {
                    width,
                    height: if height > i32::MAX as u32 {
                        0
                    } else {
                        height as i32
                    }
                    .unsigned_abs(),
                });
            }
            None
        }
        ImageFormat::Jpeg => {
            // JPEG: scan for SOF0 (0xFFC0) or SOF2 (0xFFC2) marker to find
            // dimensions. The marker segment has: 2-byte length, 1-byte
            // precision, 2-byte height (big-endian), 2-byte width.
            let mut i = 2; // skip SOI marker (0xFFD8)
            while i + 9 < bytes.len() {
                if bytes[i] != 0xFF {
                    i += 1;
                    continue;
                }
                let marker = bytes[i + 1];
                i += 2;

                // SOF markers (0xC0–0xCF, excluding 0xC4 DHT and 0xC8 JIT and 0xCC DAC)
                if (0xC0..=0xCF).contains(&marker)
                    && marker != 0xC4
                    && marker != 0xC8
                    && marker != 0xCC
                {
                    if i + 7 < bytes.len() {
                        let height = u16::from_be_bytes([bytes[i + 3], bytes[i + 4]]) as u32;
                        let width = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
                        return Some(ImageDimensions { width, height });
                    }
                    break;
                }

                // Skip this marker segment.
                if i + 1 < bytes.len() {
                    let seg_len = u16::from_be_bytes([bytes[i], bytes[i + 1]]) as usize;
                    i += seg_len;
                } else {
                    break;
                }
            }
            None
        }
    }
}

/// Read an image file and return it as inline media content parts.
///
/// Delivery modes (G-19):
/// - `region` set → crop to the region before encoding.
/// - `full_resolution` true → send the pristine bytes untouched.
/// - default → compress when beneficial (downscale to 2048px, JPEG quality
///   reduction, large PNG → JPEG), preserving the original in the temp dir.
async fn read_media_file(
    fs: Arc<dyn DynFileSystem>,
    path: String,
    region: Option<Region>,
    full_resolution: bool,
) -> ToolResult {
    let file_path = Path::new(&path);

    // Validate file metadata before reading: refuse directories and enforce
    // the size limit upfront to avoid loading oversized files into memory.
    match fs.stat(file_path).await {
        Ok(stat) if stat.is_dir => {
            return ToolResult::error(format!("ReadMediaFile failed: '{path}' is a directory"));
        }
        Ok(stat) if stat.size as usize > MAX_MEDIA_BYTES => {
            return ToolResult::error(format!(
                "ReadMediaFile failed: '{path}' is {} bytes, which exceeds the maximum \
                 supported size of {} bytes (100 MB)",
                stat.size, MAX_MEDIA_BYTES
            ));
        }
        Err(error) => {
            return ToolResult::error(format!("ReadMediaFile failed to stat '{path}': {error}"));
        }
        _ => {}
    }

    // Read the file bytes.
    let bytes = match fs.read_bytes(file_path).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return ToolResult::error(format!("ReadMediaFile failed to read '{path}': {error}"));
        }
    };

    // Detect the image format from magic bytes.
    let format = match detect_image_format(&bytes) {
        Some(f) => f,
        None => {
            return ToolResult::error(
                "This file does not appear to be an image. Use the Read tool for text \
                 files or Bash for other file types."
                    .to_string(),
            );
        }
    };

    let mime_type = format.mime_type();

    // BMP is not supported by most providers — guide the user to convert.
    if matches!(format, ImageFormat::Bmp) {
        return ToolResult::error(format!(
            "BMP format is not supported by most providers. Convert to PNG using: \
             `sips -s format png {path} <output.png>` (macOS) or \
             `convert {path} <output.png>` (Linux/ImageMagick), then read the \
             converted file."
        ));
    }

    // Verify the format is in the supported list (defensive — all remaining
    // formats should be supported, but this guards against future additions).
    if !SUPPORTED_IMAGE_MIMES.contains(&mime_type) {
        return ToolResult::error(format!(
            "ReadMediaFile does not support MIME type '{mime_type}'. Supported \
             formats: PNG, JPEG, GIF, WebP."
        ));
    }

    // Determine the payload according to the delivery mode. The pristine
    // original is preserved in the OS temp directory before any compressed
    // copy is sent (G-19).
    let payload: Vec<u8>;
    let payload_mime: &'static str;
    let delivery: &'static str;
    let mut original_line: Option<String> = None;

    if let Some(region) = region {
        let cropped = match image_pipeline::crop_region(&bytes, format, region) {
            Ok(cropped) => cropped,
            Err(error) => {
                return ToolResult::error(format!(
                    "ReadMediaFile failed to crop '{path}': {error}"
                ));
            }
        };
        // Crop output may have been re-encoded (GIF/WebP/BMP → PNG).
        let out_format = detect_image_format(&cropped).unwrap_or(ImageFormat::Png);
        payload = cropped;
        payload_mime = out_format.mime_type();
        delivery = "cropped";
    } else if full_resolution {
        payload = bytes.clone();
        payload_mime = mime_type;
        delivery = "untouched";
    } else {
        match image_pipeline::compress_media(
            &bytes,
            format,
            image_pipeline::DEFAULT_MAX_DIMENSION,
            image_pipeline::DEFAULT_JPEG_QUALITY,
        ) {
            Ok(compressed) if compressed.compressed && compressed.data.len() < bytes.len() => {
                match image_pipeline::store_original(&bytes, format.extension()) {
                    Ok(original_path) => {
                        original_line = Some(format!("original: {}", original_path.display()));
                    }
                    Err(error) => {
                        original_line = Some(format!("original: <could not store: {error}>"));
                    }
                }
                payload = compressed.data;
                payload_mime = compressed.format.mime_type();
                delivery = "compressed";
            }
            // Compression not possible or not beneficial — deliver as-is.
            _ => {
                payload = bytes.clone();
                payload_mime = mime_type;
                delivery = "untouched";
            }
        }
    }

    // Encode the payload as a base64 data URL.
    let b64 = base64::engine::general_purpose::STANDARD.encode(&payload);
    let data_url = format!("data:{payload_mime};base64,{b64}");

    // Sniff image dimensions from the final payload.
    let payload_format = detect_image_format(&payload).unwrap_or(format);
    let dims = sniff_dimensions(&payload, &payload_format);
    let mut note_lines = vec![
        format!("mime: {payload_mime}"),
        format!("bytes: {}", payload.len()),
        format!("delivery: {delivery}"),
    ];
    if let Some(d) = dims {
        note_lines.push(format!("dimensions: {}x{}", d.width, d.height));
    }
    if let Some(line) = original_line {
        note_lines.push(line);
    }
    let note = format!("<system>\n{}\n</system>", note_lines.join("\n"));

    ToolResult {
        output: ToolOutput::Parts(vec![
            ContentPart::Text {
                text: format!("<image path=\"{path}\">"),
            },
            ContentPart::ImageUrl {
                image_url: ImageUrlData {
                    url: data_url,
                    detail: None,
                    id: None,
                },
                detail: None,
            },
            ContentPart::Text {
                text: "</image>".to_string(),
            },
        ]),
        is_error: false,
        note: Some(note),
        stop_turn: false,
        truncated: false,
        injection: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ExecutableToolContext, ToolExecution};
    use image::ImageEncoder;

    /// A minimal 1×1 transparent PNG (67 bytes).
    const MINIMAL_PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
        // IHDR chunk
        0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, // CRC
        // IDAT chunk (minimal)
        0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x62, 0x00, 0x01, 0x00, 0x00,
        0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, // CRC
        // IEND chunk
        0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82, // CRC
    ];

    /// A minimal JPEG (23 bytes) — JFIF header + EOI.
    const MINIMAL_JPEG: &[u8] = &[
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
    ];

    /// A minimal GIF89a header (13 bytes).
    const MINIMAL_GIF: &[u8] = &[
        b'G', b'I', b'F', b'8', b'9', b'a', 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
    ];

    /// A minimal WebP header (12 bytes — RIFF + size + WEBP).
    const MINIMAL_WEBP: &[u8] = &[
        b'R', b'I', b'F', b'F', 0x04, 0x00, 0x00, 0x00, b'W', b'E', b'B', b'P',
    ];

    /// A minimal BMP header (2 bytes — "BM").
    const MINIMAL_BMP: &[u8] = &[
        0x42, 0x4D, // "BM" signature
        0x00, 0x00, 0x00, 0x00, // file size (unused)
        0x00, 0x00, // reserved1 (must be zero)
        0x00, 0x00, // reserved2 (must be zero)
        0x36, 0x00, 0x00, 0x00, // pixel data offset
        0x28, 0x00, 0x00, 0x00, // DIB header size (40 = BITMAPINFOHEADER)
        0x01, 0x00, 0x00, 0x00, // width
        0x01, 0x00, 0x00, 0x00, // height
        0x01, 0x00, // color planes
        0x18, 0x00, // bits per pixel
        0x00, 0x00, 0x00, 0x00, // compression
        0x00, 0x00, 0x00, 0x00, // image size
        0x00, 0x00, 0x00, 0x00, // x pixels per meter
        0x00, 0x00, 0x00, 0x00, // y pixels per meter
        0x00, 0x00, 0x00, 0x00, // colors in table
        0x00, 0x00, 0x00, 0x00, // important colors
        0x00, 0x00, 0x00, // pixel data (1 pixel, 24-bit, padded to 4 bytes)
    ];

    /// The engine-default local filesystem, as the registered tool receives
    /// it.
    fn local_fs() -> Arc<dyn DynFileSystem> {
        Arc::new(mirri_os::LocalFs::new())
    }

    fn write_fixture(dir: &std::path::Path, name: &str, content: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    /// Execute the tool end-to-end and return the result.
    async fn execute_tool(tool: &ReadMediaFileTool, path: &str) -> ToolResult {
        let execution = tool
            .resolve_execution(serde_json::json!({ "path": path }))
            .expect("resolve");
        match execution {
            ToolExecution::Runnable { execute, .. } => {
                execute(ExecutableToolContext {
                    wait_registry: None,
                    turn_id: "test".to_string(),
                    tool_call_id: "call-1".to_string(),
                    cancel: Default::default(),
                    progress: crate::tool::ToolProgressHandle::default(),
                })
                .await
            }
            ToolExecution::Error(err) => {
                let mut result = ToolResult::error(err.message);
                result.is_error = true;
                result
            }
        }
    }

    #[tokio::test]
    async fn should_return_error_when_path_is_empty() {
        let tool = ReadMediaFileTool::new(None, local_fs());
        let execution = tool
            .resolve_execution(serde_json::json!({ "path": "" }))
            .expect("resolve");
        let ToolExecution::Error(result) = execution else {
            panic!("expected an error execution");
        };
        assert!(
            result.message.contains("non-empty"),
            "message was: {}",
            result.message
        );
    }

    #[tokio::test]
    async fn should_return_error_when_path_argument_is_missing() {
        let tool = ReadMediaFileTool::new(None, local_fs());
        let execution = tool
            .resolve_execution(serde_json::json!({}))
            .expect("resolve");
        let ToolExecution::Error(result) = execution else {
            panic!("expected an error execution");
        };
        assert!(
            result.message.contains("requires a string 'path'"),
            "message was: {}",
            result.message
        );
    }

    #[tokio::test]
    async fn should_return_error_when_file_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let missing_path = tmp.path().join("does_not_exist.png");

        let tool = ReadMediaFileTool::new(None, local_fs());
        let result = execute_tool(&tool, &missing_path.to_string_lossy()).await;
        assert!(result.is_error);
        let ToolOutput::Text(msg) = &result.output else {
            panic!("expected text output");
        };
        assert!(
            msg.contains("failed to stat") || msg.contains("failed to read"),
            "message was: {msg}"
        );
    }

    #[tokio::test]
    async fn should_return_error_when_file_is_text() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "notes.txt", b"hello world");

        let tool = ReadMediaFileTool::new(None, local_fs());
        let result = execute_tool(&tool, &path.to_string_lossy()).await;
        assert!(result.is_error);
        let ToolOutput::Text(msg) = &result.output else {
            panic!("expected text output");
        };
        assert!(
            msg.contains("does not appear to be an image"),
            "message was: {msg}"
        );
    }

    #[tokio::test]
    async fn should_return_error_when_reading_bmp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "pixel.bmp", MINIMAL_BMP);

        let tool = ReadMediaFileTool::new(None, local_fs());
        let result = execute_tool(&tool, &path.to_string_lossy()).await;
        assert!(result.is_error);
        let ToolOutput::Text(msg) = &result.output else {
            panic!("expected text output");
        };
        assert!(
            msg.contains("BMP format is not supported"),
            "message was: {msg}"
        );
        assert!(
            msg.contains("sips") || msg.contains("convert"),
            "should contain conversion guidance: {msg}"
        );
    }

    #[tokio::test]
    async fn should_return_error_when_reading_unsupported_format() {
        let tmp = tempfile::tempdir().unwrap();
        // Bytes that don't match any known image signature.
        let path = write_fixture(tmp.path(), "mystery.bin", &[0x00, 0x01, 0x02, 0x03, 0x04]);

        let tool = ReadMediaFileTool::new(None, local_fs());
        let result = execute_tool(&tool, &path.to_string_lossy()).await;
        assert!(result.is_error);
        let ToolOutput::Text(msg) = &result.output else {
            panic!("expected text output");
        };
        assert!(
            msg.contains("does not appear to be an image"),
            "message was: {msg}"
        );
    }

    #[tokio::test]
    async fn should_return_image_parts_when_reading_png_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "pixel.png", MINIMAL_PNG);

        let tool = ReadMediaFileTool::new(None, local_fs());
        let result = execute_tool(&tool, &path.to_string_lossy()).await;
        assert!(!result.is_error, "should not be an error");

        let ToolOutput::Parts(parts) = &result.output else {
            panic!("expected Parts output, got: {:?}", result.output);
        };
        assert_eq!(parts.len(), 3, "should have 3 parts");

        // First part: opening <image> tag.
        assert!(
            matches!(&parts[0], ContentPart::Text { text } if text.starts_with("<image path="))
        );

        // Second part: image_url with data URL.
        match &parts[1] {
            ContentPart::ImageUrl { image_url, .. } => {
                assert!(
                    image_url.url.starts_with("data:image/png;base64,"),
                    "url should start with data:image/png;base64, got: {}",
                    &image_url.url[..30.min(image_url.url.len())]
                );
            }
            _ => panic!("expected ImageUrl part, got: {:?}", parts[1]),
        }

        // Third part: closing </image> tag.
        assert!(matches!(&parts[2], ContentPart::Text { text } if text == "</image>"));

        // Verify the note contains system metadata.
        let note = result.note.expect("should have a note");
        assert!(note.contains("image/png"), "note should contain mime type");
        assert!(note.contains("bytes:"), "note should contain byte size");
        assert!(
            note.contains("delivery: untouched"),
            "note should contain delivery mode"
        );
    }

    #[tokio::test]
    async fn should_return_image_parts_when_reading_jpeg_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "pixel.jpg", MINIMAL_JPEG);

        let tool = ReadMediaFileTool::new(None, local_fs());
        let result = execute_tool(&tool, &path.to_string_lossy()).await;
        assert!(!result.is_error, "should not be an error");

        let ToolOutput::Parts(parts) = &result.output else {
            panic!("expected Parts output, got: {:?}", result.output);
        };
        assert_eq!(parts.len(), 3, "should have 3 parts");

        match &parts[1] {
            ContentPart::ImageUrl { image_url, .. } => {
                assert!(
                    image_url.url.starts_with("data:image/jpeg;base64,"),
                    "url should start with data:image/jpeg;base64,"
                );
            }
            _ => panic!("expected ImageUrl part"),
        }
    }

    #[tokio::test]
    async fn should_return_image_parts_when_reading_gif_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "pixel.gif", MINIMAL_GIF);

        let tool = ReadMediaFileTool::new(None, local_fs());
        let result = execute_tool(&tool, &path.to_string_lossy()).await;
        assert!(!result.is_error, "should not be an error");

        let ToolOutput::Parts(parts) = &result.output else {
            panic!("expected Parts output, got: {:?}", result.output);
        };
        assert_eq!(parts.len(), 3, "should have 3 parts");

        match &parts[1] {
            ContentPart::ImageUrl { image_url, .. } => {
                assert!(
                    image_url.url.starts_with("data:image/gif;base64,"),
                    "url should start with data:image/gif;base64,"
                );
            }
            _ => panic!("expected ImageUrl part"),
        }
    }

    #[tokio::test]
    async fn should_return_image_parts_when_reading_webp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "pixel.webp", MINIMAL_WEBP);

        let tool = ReadMediaFileTool::new(None, local_fs());
        let result = execute_tool(&tool, &path.to_string_lossy()).await;
        assert!(!result.is_error, "should not be an error");

        let ToolOutput::Parts(parts) = &result.output else {
            panic!("expected Parts output, got: {:?}", result.output);
        };
        assert_eq!(parts.len(), 3, "should have 3 parts");

        match &parts[1] {
            ContentPart::ImageUrl { image_url, .. } => {
                assert!(
                    image_url.url.starts_with("data:image/webp;base64,"),
                    "url should start with data:image/webp;base64,"
                );
            }
            _ => panic!("expected ImageUrl part"),
        }
    }

    #[tokio::test]
    async fn should_return_error_when_reading_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = ReadMediaFileTool::new(None, local_fs());
        let result = execute_tool(&tool, &tmp.path().to_string_lossy()).await;
        assert!(result.is_error);
        let ToolOutput::Text(msg) = &result.output else {
            panic!("expected text output");
        };
        assert!(msg.contains("is a directory"), "message was: {msg}");
    }

    #[tokio::test]
    async fn should_read_media_from_an_outside_file_when_reading_media_with_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("secret.png");
        std::fs::write(&outside_file, MINIMAL_PNG).unwrap();

        let tool = ReadMediaFileTool::new(
            Some(Arc::new(WorkspacePaths::new(tmp.path().to_path_buf()))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({
                "path": outside_file.to_string_lossy()
            }))
            .expect("resolve");

        // A clean absolute path outside the sandbox flows into the plan; the
        // permission chain (not the tool) decides whether it may run.
        let ToolExecution::Runnable { plan, execute } = execution else {
            panic!("expected a runnable execution for an outside absolute path");
        };
        let access = plan.accesses.first().expect("one file access");
        assert_eq!(
            access.path.as_deref(),
            Some(outside_file.canonicalize().unwrap().to_str().unwrap()),
            "the canonical outside path must enter the accesses"
        );
        let result = execute(crate::tool::ExecutableToolContext {
            wait_registry: None,
            turn_id: "test".to_string(),
            tool_call_id: "call-1".to_string(),
            cancel: Default::default(),
            progress: crate::tool::ToolProgressHandle::default(),
        })
        .await;
        assert!(!result.is_error, "media read failed: {result:?}");
        assert!(matches!(result.output, ToolOutput::Parts(_)));
    }

    #[tokio::test]
    async fn should_refuse_relative_dotdot_escape_when_reading_media_with_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();

        let tool = ReadMediaFileTool::new(
            Some(Arc::new(WorkspacePaths::new(tmp.path().to_path_buf()))),
            local_fs(),
        );
        let execution = tool
            .resolve_execution(serde_json::json!({ "path": "../secret.png" }))
            .expect("resolve");

        let ToolExecution::Error(result) = execution else {
            panic!("expected an escape refusal, got a runnable execution");
        };
        assert!(
            result.message.contains("escapes the workspace sandbox"),
            "refusal uses the new escape wording: {}",
            result.message
        );
    }

    #[tokio::test]
    async fn should_read_media_inside_workspace_when_workspace_root_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "in.png", MINIMAL_PNG);

        let tool = ReadMediaFileTool::new(
            Some(Arc::new(WorkspacePaths::new(tmp.path().to_path_buf()))),
            local_fs(),
        );
        let result = execute_tool(&tool, &path.to_string_lossy()).await;
        assert!(!result.is_error, "should succeed inside workspace");
        assert!(matches!(result.output, ToolOutput::Parts(_)));
    }

    #[tokio::test]
    async fn should_reject_oversized_file_before_reading_contents() {
        // Create a real file whose stat-reported size exceeds 100 MB to verify
        // the stat-based pre-check rejects it without loading full contents.
        // We use a sparse file: stat reports a large size but minimal disk blocks.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("large.png");
        use std::io::{Seek, SeekFrom, Write};
        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .unwrap();
            // Write PNG header at start so format detection would pass
            // if the size pre-check didn't catch it first.
            file.write_all(MINIMAL_PNG).unwrap();
            // Seek far past 100 MB and write a byte to make stat report large size.
            file.seek(SeekFrom::Start(101 * 1024 * 1024)).unwrap();
            file.write_all(&[0x00]).unwrap();
            file.flush().unwrap();
        }

        let tool = ReadMediaFileTool::new(None, local_fs());
        let result = execute_tool(&tool, &path.to_string_lossy()).await;
        assert!(result.is_error, "should reject oversized file");
        let ToolOutput::Text(msg) = &result.output else {
            panic!("expected text output");
        };
        assert!(
            msg.contains("exceeds the maximum"),
            "should mention size limit: {msg}"
        );
    }

    /// should report image dimensions for PNG files
    #[tokio::test]
    async fn should_report_image_dimensions_for_png_files() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "pixel.png", MINIMAL_PNG);

        let tool = ReadMediaFileTool::new(None, local_fs());
        let result = execute_tool(&tool, &path.to_string_lossy()).await;
        assert!(!result.is_error);
        let note = result.note.expect("should have a note");
        assert!(
            note.contains("dimensions: 1x1"),
            "note should contain 1x1 dimensions: {note}"
        );
    }

    /// should report image dimensions for GIF files
    #[tokio::test]
    async fn should_report_image_dimensions_for_gif_files() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), "pixel.gif", MINIMAL_GIF);

        let tool = ReadMediaFileTool::new(None, local_fs());
        let result = execute_tool(&tool, &path.to_string_lossy()).await;
        assert!(!result.is_error);
        let note = result.note.expect("should have a note");
        assert!(
            note.contains("dimensions: 1x1"),
            "note should contain 1x1 dimensions: {note}"
        );
    }

    /// should detect PNG dimensions from header bytes
    #[test]
    fn should_detect_png_dimensions_from_header() {
        let format = detect_image_format(MINIMAL_PNG).unwrap();
        assert!(matches!(format, ImageFormat::Png));
        let dims = sniff_dimensions(MINIMAL_PNG, &format).unwrap();
        assert_eq!(dims.width, 1);
        assert_eq!(dims.height, 1);
    }

    /// should detect GIF dimensions from header bytes
    #[test]
    fn should_detect_gif_dimensions_from_header() {
        let format = detect_image_format(MINIMAL_GIF).unwrap();
        assert!(matches!(format, ImageFormat::Gif));
        let dims = sniff_dimensions(MINIMAL_GIF, &format).unwrap();
        assert_eq!(dims.width, 1);
        assert_eq!(dims.height, 1);
    }

    /// should return None for dimensions when header is too short
    #[test]
    fn should_return_none_when_header_too_short() {
        let format = ImageFormat::Png;
        let dims = sniff_dimensions(&[0x89, 0x50], &format);
        assert!(dims.is_none());
    }

    // ------------------------------------------------------------------ //
    //  Magic-byte format detection (GAP-16)                              //
    // ------------------------------------------------------------------ //

    /// should detect PNG and its MIME when bytes have the PNG magic
    #[test]
    fn should_detect_png_format_and_mime_when_bytes_have_png_magic_bytes() {
        let format = detect_image_format(MINIMAL_PNG).expect("PNG magic bytes");

        assert!(matches!(format, ImageFormat::Png));
        assert_eq!(format.mime_type(), "image/png");
        assert_eq!(format.extension(), "png");
    }

    /// should detect JPEG and its MIME when bytes have the JPEG magic
    #[test]
    fn should_detect_jpeg_format_and_mime_when_bytes_have_jpeg_magic_bytes() {
        let format = detect_image_format(MINIMAL_JPEG).expect("JPEG magic bytes");

        assert!(matches!(format, ImageFormat::Jpeg));
        assert_eq!(format.mime_type(), "image/jpeg");
        assert_eq!(format.extension(), "jpg");
    }

    /// should detect GIF and its MIME when bytes have the GIF magic
    #[test]
    fn should_detect_gif_format_and_mime_when_bytes_have_gif_magic_bytes() {
        let format = detect_image_format(MINIMAL_GIF).expect("GIF magic bytes");

        assert!(matches!(format, ImageFormat::Gif));
        assert_eq!(format.mime_type(), "image/gif");
        assert_eq!(format.extension(), "gif");
    }

    /// should detect WebP and its MIME when bytes have the WebP magic
    #[test]
    fn should_detect_webp_format_and_mime_when_bytes_have_webp_magic_bytes() {
        let format = detect_image_format(MINIMAL_WEBP).expect("WebP magic bytes");

        assert!(matches!(format, ImageFormat::Webp));
        assert_eq!(format.mime_type(), "image/webp");
        assert_eq!(format.extension(), "webp");
    }

    /// should detect BMP and its MIME when bytes have the BMP magic
    #[test]
    fn should_detect_bmp_format_and_mime_when_bytes_have_bmp_magic_bytes() {
        let format = detect_image_format(MINIMAL_BMP).expect("BMP magic bytes");

        assert!(matches!(format, ImageFormat::Bmp));
        assert_eq!(format.mime_type(), "image/bmp");
        assert_eq!(format.extension(), "bmp");
    }

    /// should return None when bytes match no known image magic
    #[test]
    fn should_return_none_when_bytes_match_no_known_image_magic() {
        assert!(detect_image_format(b"plain text file").is_none());
        // A truncated PNG signature is not a valid magic sequence.
        assert!(detect_image_format(&MINIMAL_PNG[..4]).is_none());
    }

    // ------------------------------------------------------------------ //
    //  G-19 media-pipeline tests                                         //
    // ------------------------------------------------------------------ //

    /// Encode a seeded-noise RGBA image — PNG cannot compress noise, so a
    /// JPEG copy is deterministically smaller (lets tests assert compression
    /// without depending on encoder heuristics).
    fn noisy_png(width: u32, height: u32) -> Vec<u8> {
        let mut img = image::RgbaImage::new(width, height);
        let mut seed = 0x9E37_79B9u32;
        for pixel in img.pixels_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            *pixel = image::Rgba([seed as u8, (seed >> 8) as u8, (seed >> 16) as u8, 255]);
        }
        let mut out = Vec::new();
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(
                img.as_raw(),
                img.width(),
                img.height(),
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        out
    }

    async fn execute_tool_with_input(
        tool: &ReadMediaFileTool,
        input: serde_json::Value,
    ) -> ToolResult {
        let execution = tool.resolve_execution(input).expect("resolve");
        match execution {
            ToolExecution::Runnable { execute, .. } => {
                execute(ExecutableToolContext {
                    wait_registry: None,
                    turn_id: "test".to_string(),
                    tool_call_id: "call-1".to_string(),
                    cancel: Default::default(),
                    progress: crate::tool::ToolProgressHandle::default(),
                })
                .await
            }
            ToolExecution::Error(err) => {
                let mut result = ToolResult::error(err.message);
                result.is_error = true;
                result
            }
        }
    }

    /// Extract the base64 payload bytes from an image parts result.
    fn payload_bytes(result: &ToolResult) -> Vec<u8> {
        let ToolOutput::Parts(parts) = &result.output else {
            panic!("expected Parts output");
        };
        let ContentPart::ImageUrl { image_url, .. } = &parts[1] else {
            panic!("expected ImageUrl part");
        };
        let encoded = image_url.url.split_once(";base64,").expect("data URL").1;
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("valid base64")
    }

    #[tokio::test]
    async fn should_crop_before_encoding_when_region_is_specified() {
        let tmp = tempfile::tempdir().unwrap();
        let bytes = noisy_png(12, 12);
        let path = write_fixture(tmp.path(), "grid.png", &bytes);

        let tool = ReadMediaFileTool::new(None, local_fs());
        let result = execute_tool_with_input(
            &tool,
            serde_json::json!({
                "path": path.to_string_lossy(),
                "region": { "x": 2, "y": 3, "width": 6, "height": 6 }
            }),
        )
        .await;
        assert!(!result.is_error, "{:?}", result.output);
        let note = result.note.as_deref().expect("note");
        assert!(note.contains("delivery: cropped"), "note was: {note}");

        let decoded = image::load_from_memory(&payload_bytes(&result)).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (6, 6));
    }

    #[tokio::test]
    async fn should_compress_and_store_original_when_reading_large_image() {
        let tmp = tempfile::tempdir().unwrap();
        // 2500×1500 exceeds the 2048px default max dimension.
        let bytes = noisy_png(2500, 1500);
        let path = write_fixture(tmp.path(), "large.png", &bytes);

        let tool = ReadMediaFileTool::new(None, local_fs());
        let result = execute_tool(&tool, &path.to_string_lossy()).await;
        assert!(!result.is_error, "{:?}", result.output);
        let note = result.note.as_deref().expect("note");
        assert!(
            note.contains("delivery: compressed"),
            "large image should be compressed: {note}"
        );

        // The pristine original must be preserved and referenced.
        let original_line = note
            .lines()
            .find(|l| l.starts_with("original: "))
            .expect("original line in note")
            .to_string();
        let original_path =
            std::path::PathBuf::from(original_line.trim_start_matches("original: ").trim());
        assert!(
            original_path.exists(),
            "original should exist at {original_path:?}"
        );
        assert_eq!(
            std::fs::read(&original_path).unwrap(),
            bytes,
            "original file preserves the pristine bytes"
        );
        let _ = std::fs::remove_file(&original_path);

        // The delivered payload is downscaled.
        let decoded = image::load_from_memory(&payload_bytes(&result)).unwrap();
        assert!(decoded.width() <= 2048 && decoded.height() <= 2048);
    }

    #[tokio::test]
    async fn should_skip_compression_when_full_resolution_is_true() {
        let tmp = tempfile::tempdir().unwrap();
        let bytes = noisy_png(2500, 1500);
        let path = write_fixture(tmp.path(), "full.png", &bytes);

        let tool = ReadMediaFileTool::new(None, local_fs());
        let result = execute_tool_with_input(
            &tool,
            serde_json::json!({
                "path": path.to_string_lossy(),
                "full_resolution": true
            }),
        )
        .await;
        assert!(!result.is_error, "{:?}", result.output);
        let note = result.note.as_deref().expect("note");
        assert!(
            note.contains("delivery: untouched"),
            "full_resolution must skip compression: {note}"
        );
        assert!(
            !note.contains("original:"),
            "no original copy is stored for untouched delivery: {note}"
        );
        assert_eq!(
            payload_bytes(&result),
            bytes,
            "full-resolution delivery returns the exact file bytes"
        );
    }

    #[tokio::test]
    async fn should_refuse_zero_sized_region_when_region_is_invalid() {
        let tool = ReadMediaFileTool::new(None, local_fs());
        let execution = tool
            .resolve_execution(serde_json::json!({
                "path": "/tmp/whatever.png",
                "region": { "x": 0, "y": 0, "width": 0, "height": 10 }
            }))
            .expect("resolve");
        let ToolExecution::Error(result) = execution else {
            panic!("expected an error execution for a zero-sized region");
        };
        assert!(result.message.contains("positive width and height"));
    }

    /// Given the ReadMediaFile tool's parameter schema (Task 5, plan
    /// 2026-10-10-sandbox-path-permission D5), When the `path` description
    /// is inspected, Then it states the workspace-sandbox constraint.
    #[test]
    fn should_state_the_sandbox_constraint_in_the_path_parameter_description() {
        let tool = ReadMediaFileTool::new(None, local_fs());
        let description = tool.parameters()["properties"]["path"]["description"]
            .as_str()
            .expect("path description present");
        assert!(description.contains("workspace sandbox"), "{description}");
        assert!(description.contains("user authorization"), "{description}");
    }
}
