//! Image/media processing pipeline (G-19).
//!
//! Shared media-processing helpers for tools that deliver images to the model:
//!
//! - [`compress_image`] / [`compress_media`] — resize + re-encode images
//!   before they are sent to the LLM (JPEG quality reduction, large-PNG →
//!   JPEG conversion, downscale beyond [`DEFAULT_MAX_DIMENSION`]).
//! - [`store_original`] — stash the pristine image bytes in the OS temp
//!   directory so the original is never lost when a compressed copy is sent.
//! - [`crop_region`] — crop an image to a [`Region`] before encoding.
//! - [`model_supports_vision`] / [`entry_supports_vision`] /
//!   [`catalog_supports_vision`] — model-capability gating so text-only
//!   models get guidance instead of media they cannot see.
//!
//! Decoding/encoding is delegated to the `image` crate; when an input cannot
//! be decoded (truncated or exotic files) the compressors pass the bytes
//! through unchanged instead of erroring — media delivery is best-effort.

use std::path::{Path, PathBuf};

use image::ImageEncoder;
use uuid::Uuid;

use crate::model_catalog::{ModelCatalog, ModelCatalogEntry};

/// Default maximum image dimension (width or height) in pixels.
pub const DEFAULT_MAX_DIMENSION: u32 = 2048;

/// Default JPEG quality used when re-encoding compressed images.
pub const DEFAULT_JPEG_QUALITY: u8 = 85;

/// PNGs larger than this (in bytes) are converted to JPEG.
const PNG_TO_JPEG_BYTE_THRESHOLD: usize = 512 * 1024;

/// Image formats the media pipeline can process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
    Bmp,
}

impl ImageFormat {
    /// Detect the format from a file path extension, if it is one of the
    /// supported image formats.
    pub fn from_extension(path: &Path) -> Option<ImageFormat> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "png" => Some(ImageFormat::Png),
            "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
            "gif" => Some(ImageFormat::Gif),
            "webp" => Some(ImageFormat::Webp),
            "bmp" => Some(ImageFormat::Bmp),
            _ => None,
        }
    }

    /// Canonical file extension for this format (without the dot).
    pub fn extension(&self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Gif => "gif",
            ImageFormat::Webp => "webp",
            ImageFormat::Bmp => "bmp",
        }
    }

    /// MIME type for this format.
    pub fn mime_type(&self) -> &'static str {
        match self {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Gif => "image/gif",
            ImageFormat::Webp => "image/webp",
            ImageFormat::Bmp => "image/bmp",
        }
    }
}

/// Detect the image format from magic bytes.
///
/// Returns `Some(format)` if the bytes match a known image signature
/// (PNG, JPEG, GIF, WebP, BMP), `None` otherwise. Shared by the media
/// tools and the entry ingest pipeline so both classify the same bytes
/// the same way.
pub fn detect_image_format(bytes: &[u8]) -> Option<ImageFormat> {
    // PNG: \x89PNG\r\n\x1a\n (8 bytes)
    if bytes.len() >= 8
        && bytes[0] == 0x89
        && bytes[1] == 0x50
        && bytes[2] == 0x4E
        && bytes[3] == 0x47
        && bytes[4] == 0x0D
        && bytes[5] == 0x0A
        && bytes[6] == 0x1A
        && bytes[7] == 0x0A
    {
        return Some(ImageFormat::Png);
    }

    // JPEG: \xFF\xD8\xFF (3 bytes)
    if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return Some(ImageFormat::Jpeg);
    }

    // GIF: "GIF87a" or "GIF89a" (6 bytes)
    if bytes.len() >= 6 {
        let header = &bytes[0..6];
        if header == b"GIF87a" || header == b"GIF89a" {
            return Some(ImageFormat::Gif);
        }
    }

    // WebP: "RIFF" (bytes 0-3) + .... + "WEBP" (bytes 8-11)
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some(ImageFormat::Webp);
    }

    // BMP: "BM" (bytes 0-1) + reserved bytes at offset 6-7 must be zero.
    // The extra check reduces false-positives on arbitrary files starting with "BM".
    if bytes.len() >= 8
        && bytes[0] == 0x42
        && bytes[1] == 0x4D
        && bytes[6] == 0x00
        && bytes[7] == 0x00
    {
        return Some(ImageFormat::Bmp);
    }

    None
}

/// A rectangular region of an image, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    /// Left edge (inclusive), relative to the image origin.
    pub x: u32,
    /// Top edge (inclusive), relative to the image origin.
    pub y: u32,
    /// Crop width in pixels.
    pub width: u32,
    /// Crop height in pixels.
    pub height: u32,
}

impl Region {
    /// Parse a region from a tool-input JSON value shaped
    /// `{"x": u32, "y": u32, "width": u32, "height": u32}`.
    pub fn from_json(value: &serde_json::Value) -> Option<Region> {
        Some(Region {
            x: value.get("x")?.as_u64()? as u32,
            y: value.get("y")?.as_u64()? as u32,
            width: value.get("width")?.as_u64()? as u32,
            height: value.get("height")?.as_u64()? as u32,
        })
    }
}

/// Result of the media pipeline for one image delivery.
#[derive(Debug, Clone)]
pub struct CompressedImage {
    /// Bytes to send to the model (compressed, or the original when
    /// compression was not possible / not beneficial).
    pub data: Vec<u8>,
    /// Effective format of `data`.
    pub format: ImageFormat,
    /// `true` when `data` differs from the input (resized and/or re-encoded).
    pub compressed: bool,
    /// Dimensions of `data` (0 when they could not be probed).
    pub width: u32,
    pub height: u32,
}

/// Errors produced by the media pipeline.
#[derive(Debug, thiserror::Error)]
pub enum ImagePipelineError {
    /// The input bytes could not be decoded as an image.
    #[error("failed to decode image: {0}")]
    Decode(String),
    /// The processed image could not be encoded.
    #[error("failed to encode image: {0}")]
    Encode(String),
    /// Filesystem error (e.g. storing the original).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The requested crop region is invalid.
    #[error("invalid crop region: {0}")]
    InvalidRegion(String),
}

/// Result alias for the media pipeline.
pub type ImagePipelineResult<T> = Result<T, ImagePipelineError>;

/// Compress `input` for delivery to an LLM.
///
/// - **JPEG**: re-encoded at `quality`, downscaled to `max_dimension`.
/// - **PNG**: converted to JPEG when large (dimension or byte size), otherwise
///   re-encoded as PNG (which typically shrinks it).
/// - **GIF / WebP / BMP**: passed through unchanged — re-encoding would lose
///   animation or is unsupported by the encoder set.
///
/// When the input cannot be decoded, the original bytes are returned
/// unchanged rather than an error (best-effort delivery).
pub fn compress_image(
    input: &[u8],
    format: ImageFormat,
    max_dimension: u32,
    quality: u8,
) -> ImagePipelineResult<Vec<u8>> {
    match format {
        ImageFormat::Gif | ImageFormat::Webp | ImageFormat::Bmp => Ok(input.to_vec()),
        ImageFormat::Jpeg => {
            let img = decode(input)?;
            let img = resize_to_max(&img, max_dimension);
            encode_jpeg(&img, quality)
        }
        ImageFormat::Png => {
            let img = decode(input)?;
            if input.len() > PNG_TO_JPEG_BYTE_THRESHOLD
                || img.width() > max_dimension
                || img.height() > max_dimension
            {
                // Large PNG → JPEG.
                let img = resize_to_max(&img, max_dimension);
                encode_jpeg(&img, quality)
            } else {
                // Small PNG → keep PNG, re-encode.
                encode_png(&img)
            }
        }
    }
}

/// [`compress_image`] plus delivery metadata.
///
/// This is the entry point tools use: it reports the effective output format,
/// dimensions, and whether the payload actually changed. Formats that are
/// passed through (GIF/WebP/BMP) always report `compressed: false`.
pub fn compress_media(
    input: &[u8],
    format: ImageFormat,
    max_dimension: u32,
    quality: u8,
) -> ImagePipelineResult<CompressedImage> {
    match format {
        ImageFormat::Gif | ImageFormat::Webp | ImageFormat::Bmp => {
            let (width, height) = probe_dimensions(input).unwrap_or((0, 0));
            Ok(CompressedImage {
                data: input.to_vec(),
                format,
                compressed: false,
                width,
                height,
            })
        }
        ImageFormat::Jpeg => {
            let data = compress_image(input, format, max_dimension, quality)?;
            let (width, height) = probe_dimensions(&data).unwrap_or((0, 0));
            let compressed = data != input;
            Ok(CompressedImage {
                data,
                format,
                compressed,
                width,
                height,
            })
        }
        ImageFormat::Png => {
            // The PNG branch converts to JPEG when the input is large; mirror
            // that decision here so the reported format matches the payload.
            let converts_to_jpeg = image::load_from_memory(input)
                .ok()
                .map(|img| {
                    input.len() > PNG_TO_JPEG_BYTE_THRESHOLD
                        || img.width() > max_dimension
                        || img.height() > max_dimension
                })
                .unwrap_or(false);
            let data = compress_image(input, format, max_dimension, quality)?;
            let out_format = if converts_to_jpeg {
                ImageFormat::Jpeg
            } else {
                ImageFormat::Png
            };
            let (width, height) = probe_dimensions(&data).unwrap_or((0, 0));
            let compressed = data != input;
            Ok(CompressedImage {
                data,
                format: out_format,
                compressed,
                width,
                height,
            })
        }
    }
}

/// Save `input` to a uniquely named file in the OS temp directory and return
/// its path, so the original image survives compression.
///
/// The write goes through the host-local `LocalSyncFs`; inject a double and
/// an explicit temp root with [`store_original_with_fs`] instead.
pub fn store_original(input: &[u8], ext: &str) -> ImagePipelineResult<PathBuf> {
    store_original_with_fs(
        &mirri_os::LocalSyncFs::new(),
        &std::env::temp_dir(),
        input,
        ext,
    )
}

/// [`store_original`] with an explicit [`SyncFs`] provider and temp root —
/// the injection seam (zero-IO T13c). The temp root is provided by the
/// caller (the engine default resolves it from the OS environment, which
/// stays outside the fs gate).
pub fn store_original_with_fs(
    fs: &dyn mirri_os::SyncFs,
    temp_root: &Path,
    input: &[u8],
    ext: &str,
) -> ImagePipelineResult<PathBuf> {
    let ext = if ext.is_empty() { "bin" } else { ext };
    let path = temp_root.join(format!("mirri-original-{}.{ext}", Uuid::new_v4()));
    fs.write_bytes(&path, input)
        .map_err(|e| ImagePipelineError::Io(std::io::Error::other(format!("{e:#}"))))?;
    Ok(path)
}

/// Crop `input` to `region` and re-encode it.
///
/// The region is clamped to the image bounds: a crop that extends past the
/// edge yields the remaining in-bounds portion. Output format is preserved
/// for PNG and JPEG; GIF / WebP / BMP inputs are re-encoded as PNG (their
/// encoders are not available in the pipeline feature set).
pub fn crop_region(
    input: &[u8],
    format: ImageFormat,
    region: Region,
) -> ImagePipelineResult<Vec<u8>> {
    if region.width == 0 || region.height == 0 {
        return Err(ImagePipelineError::InvalidRegion(format!(
            "crop width/height must be > 0, got {}x{}",
            region.width, region.height
        )));
    }
    let img = decode(input)?;
    let x = region.x.min(img.width());
    let y = region.y.min(img.height());
    let width = region.width.min(img.width() - x);
    let height = region.height.min(img.height() - y);
    if width == 0 || height == 0 {
        return Err(ImagePipelineError::InvalidRegion(
            "crop region lies entirely outside the image".to_string(),
        ));
    }
    let cropped = image::imageops::crop_imm(&img, x, y, width, height).to_image();
    let out_format = match format {
        ImageFormat::Gif | ImageFormat::Webp | ImageFormat::Bmp => ImageFormat::Png,
        other => other,
    };
    encode_buffer(&cropped, out_format, DEFAULT_JPEG_QUALITY)
}

/// Tri-state name inference for vision support: `Some(true)` /
/// `Some(false)` when a marker matches, `None` when the name carries no
/// signal (the caller then resolves the modality as unsupported — ADR-003
/// revision: undefined is not "unknown").
pub fn name_vision_support(model: &str) -> Option<bool> {
    let lower = model.to_ascii_lowercase();
    if VISION_MARKERS.iter().any(|m| lower.contains(m)) {
        return Some(true);
    }
    if TEXT_ONLY_MARKERS.iter().any(|m| lower.contains(m)) {
        return Some(false);
    }
    None
}

/// Whether the current model supports vision.
///
/// Uses explicit markers when the model id is known; unknown models default
/// to `true` (permissive — the LLM itself can still be steered by its own
/// provider prompt). Combine with [`entry_supports_vision`] /
/// [`catalog_supports_vision`] when catalog capability data is available.
pub fn model_supports_vision(model: Option<&str>) -> bool {
    model.and_then(name_vision_support).unwrap_or(true)
}

/// Whether a catalog entry supports vision, using its declared capabilities
/// when present and falling back to the name heuristic when unknown.
pub fn entry_supports_vision(entry: &ModelCatalogEntry) -> bool {
    if entry
        .capabilities
        .iter()
        .any(|c| c.eq_ignore_ascii_case("vision"))
    {
        return true;
    }
    if !entry.capabilities.is_empty() {
        return false;
    }
    model_supports_vision(Some(&entry.model))
}

/// Whether the model identified by `alias` supports vision, consulting the
/// catalog first and falling back to the name heuristic on misses.
pub fn catalog_supports_vision(catalog: &dyn ModelCatalog, alias: &str) -> bool {
    if let Some(entry) = catalog.get_by_id(alias) {
        return entry_supports_vision(&entry);
    }
    model_supports_vision(Some(alias))
}

/// Model-id markers that indicate vision support.
const VISION_MARKERS: &[&str] = &[
    "vision",
    "multimodal",
    "omni",
    "gpt-4o",
    "gpt-4.1",
    "gpt-5",
    "claude-3",
    "claude-sonnet-4",
    "claude-opus-4",
    "gemini",
    "qwen-vl",
    "qwen2-vl",
    "qwen2.5-vl",
    "llava",
    "pixtral",
    "gemma-3",
    "glm-4v",
    "internvl",
    "minicpm-v",
    "phi-3-vision",
    "moondream",
    "molmo",
    "llama-3.2",
];

/// Model-id markers that indicate text-only models. Checked only after the
/// vision markers, so `gpt-4o` never trips the `gpt-4` text-only marker.
const TEXT_ONLY_MARKERS: &[&str] = &[
    "gpt-3.5",
    "gpt-4-turbo-preview",
    "gpt-4-base",
    "text-davinci",
    "text-bison",
    "deepseek",
    "mistral",
    "mixtral",
    "llama-3.1",
    "llama-3.3",
    "claude-2",
    "claude-1",
    "command-r",
    "command-r-plus",
];

/// Decode image bytes with the `image` crate.
fn decode(input: &[u8]) -> ImagePipelineResult<image::DynamicImage> {
    image::load_from_memory(input).map_err(|e| ImagePipelineError::Decode(e.to_string()))
}

/// Downscale `img` so its longer side is at most `max_dimension`
/// (`0` disables the cap). Aspect ratio is preserved.
fn resize_to_max(img: &image::DynamicImage, max_dimension: u32) -> image::DynamicImage {
    if max_dimension == 0 {
        return img.clone();
    }
    let (width, height) = (img.width(), img.height());
    let longest = width.max(height);
    if longest <= max_dimension {
        return img.clone();
    }
    let scale = max_dimension as f64 / longest as f64;
    let new_width = ((width as f64 * scale).round() as u32).max(1);
    let new_height = ((height as f64 * scale).round() as u32).max(1);
    img.resize(new_width, new_height, image::imageops::FilterType::Lanczos3)
}

/// Re-encode `img` as a JPEG at `quality` (clamped to 1..=100).
fn encode_jpeg(img: &image::DynamicImage, quality: u8) -> ImagePipelineResult<Vec<u8>> {
    let rgb = img.to_rgb8();
    let mut out = Vec::new();
    let mut encoder =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality.clamp(1, 100));
    encoder
        .encode(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| ImagePipelineError::Encode(e.to_string()))?;
    Ok(out)
}

/// Re-encode `img` as a PNG (RGBA, so alpha is preserved).
fn encode_png(img: &image::DynamicImage) -> ImagePipelineResult<Vec<u8>> {
    let rgba = img.to_rgba8();
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|e| ImagePipelineError::Encode(e.to_string()))?;
    Ok(out)
}

/// Encode an `ImageBuffer` in the given output format.
fn encode_buffer(
    img: &image::RgbaImage,
    format: ImageFormat,
    quality: u8,
) -> ImagePipelineResult<Vec<u8>> {
    let dynamic = image::DynamicImage::ImageRgba8(img.clone());
    match format {
        ImageFormat::Jpeg => encode_jpeg(&dynamic, quality),
        ImageFormat::Png => encode_png(&dynamic),
        ImageFormat::Gif | ImageFormat::Webp | ImageFormat::Bmp => {
            // Fall back to PNG for formats without an encoder in the pipeline
            // feature set.
            encode_png(&dynamic)
        }
    }
}

/// Best-effort dimension probe via full decode; `None` when the bytes are not
/// a decodable image. Shared with the entry ingest pipeline's budget check.
pub fn probe_dimensions(input: &[u8]) -> Option<(u32, u32)> {
    let img = image::load_from_memory(input).ok()?;
    Some((img.width(), img.height()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a smooth RGBA gradient, which compresses well and lets tests
    /// assert exact post-resize dimensions.
    fn gradient_rgba(width: u32, height: u32) -> image::RgbaImage {
        let mut img = image::RgbaImage::new(width, height);
        for (x, y, pixel) in img.enumerate_pixels_mut() {
            *pixel = image::Rgba([
                (x * 255 / width.max(1)) as u8,
                (y * 255 / height.max(1)) as u8,
                128,
                255,
            ]);
        }
        img
    }

    /// Encode a seeded-noise RGBA image — noise compresses poorly at high
    /// JPEG quality, so quality reduction deterministically shrinks it.
    fn noise_rgba(width: u32, height: u32) -> image::RgbaImage {
        let mut img = image::RgbaImage::new(width, height);
        let mut seed = 0x9E37_79B9u32;
        for pixel in img.pixels_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            *pixel = image::Rgba([seed as u8, (seed >> 8) as u8, (seed >> 16) as u8, 255]);
        }
        img
    }

    fn encode_png_bytes(img: &image::RgbaImage) -> Vec<u8> {
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

    fn encode_jpeg_bytes(img: &image::RgbaImage, quality: u8) -> Vec<u8> {
        let rgb = image::DynamicImage::ImageRgba8(img.clone()).to_rgb8();
        let mut out = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
        encoder
            .encode(
                rgb.as_raw(),
                rgb.width(),
                rgb.height(),
                image::ExtendedColorType::Rgb8,
            )
            .unwrap();
        out
    }

    #[test]
    fn should_compress_large_image_before_sending_to_llm() {
        // A 3072×2048 PNG exceeds the default max dimension of 2048.
        let png = encode_png_bytes(&gradient_rgba(3072, 2048));

        let out = compress_image(&png, ImageFormat::Png, DEFAULT_MAX_DIMENSION, 85).unwrap();

        // Large PNGs are converted to JPEG and downscaled to the max
        // dimension, preserving the 3:2 aspect ratio.
        assert!(out.starts_with(&[0xFF, 0xD8, 0xFF]), "expected JPEG output");
        let decoded = image::load_from_memory(&out).unwrap();
        assert_eq!(decoded.width(), 2048);
        assert_eq!(decoded.height(), 1365);
    }

    #[test]
    fn should_reduce_jpeg_quality_when_compressing_a_large_jpeg() {
        // Noise compresses poorly, so the quality reduction from 95 to 30
        // deterministically shrinks the payload (a smooth gradient would fit
        // in a few DCT coefficients at either quality).
        let noisy = encode_jpeg_bytes(&noise_rgba(1500, 1000), 95);

        let out = compress_image(&noisy, ImageFormat::Jpeg, DEFAULT_MAX_DIMENSION, 30).unwrap();

        assert!(out.starts_with(&[0xFF, 0xD8, 0xFF]), "expected JPEG output");
        assert!(
            out.len() < noisy.len(),
            "q30 ({}) should be smaller than q95 ({})",
            out.len(),
            noisy.len()
        );
    }

    #[test]
    fn should_keep_small_png_as_png_when_compressing() {
        let png = encode_png_bytes(&gradient_rgba(64, 64));

        let out = compress_image(&png, ImageFormat::Png, DEFAULT_MAX_DIMENSION, 85).unwrap();

        assert!(
            out.starts_with(&[0x89, b'P', b'N', b'G']),
            "expected PNG output"
        );
    }

    #[test]
    fn should_store_original_image_and_return_reference_path() {
        let data: &[u8] = b"RAW-IMAGE-BYTES";

        let path = store_original(data, "png").unwrap();

        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("mirri-original-"), "name was: {name}");
        assert!(name.ends_with(".png"), "name was: {name}");
        assert_eq!(std::fs::read(&path).unwrap(), data, "stored bytes match");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn should_crop_image_when_region_is_specified() {
        let png = encode_png_bytes(&gradient_rgba(10, 10));

        let out = crop_region(
            &png,
            ImageFormat::Png,
            Region {
                x: 2,
                y: 3,
                width: 4,
                height: 5,
            },
        )
        .unwrap();

        let img = image::load_from_memory(&out).unwrap();
        assert_eq!((img.width(), img.height()), (4, 5));
    }

    #[test]
    fn should_clamp_region_to_image_bounds_when_region_overflows() {
        let png = encode_png_bytes(&gradient_rgba(8, 8));

        // Region starts at 6,6 but wants 4×4 — only 2×2 remains in bounds.
        let out = crop_region(
            &png,
            ImageFormat::Png,
            Region {
                x: 6,
                y: 6,
                width: 4,
                height: 4,
            },
        )
        .unwrap();

        let img = image::load_from_memory(&out).unwrap();
        assert_eq!((img.width(), img.height()), (2, 2));
    }

    #[test]
    fn should_return_error_when_crop_region_is_fully_outside_image() {
        let png = encode_png_bytes(&gradient_rgba(8, 8));

        let result = crop_region(
            &png,
            ImageFormat::Png,
            Region {
                x: 20,
                y: 20,
                width: 4,
                height: 4,
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn should_return_tri_state_vision_support_by_name() {
        // Given 名字启发式的三类模型名
        // When 三态判定
        // Then 命中 VISION→Some(true)、TEXT_ONLY→Some(false)、无信号→None
        assert_eq!(name_vision_support("gpt-4o-mini"), Some(true));
        assert_eq!(name_vision_support("claude-3-5-sonnet"), Some(true));
        assert_eq!(name_vision_support("deepseek-chat"), Some(false));
        assert_eq!(name_vision_support("some-private-model"), None);
    }

    #[test]
    fn should_report_vision_supported_when_model_name_indicates_vision() {
        for model in [
            Some("gpt-4o"),
            Some("gpt-4o-mini"),
            Some("claude-sonnet-4-20250514"),
            Some("gemini-2.0-flash"),
            Some("qwen2.5-vl-7b"),
        ] {
            assert!(
                model_supports_vision(model),
                "{model:?} should support vision"
            );
        }
    }

    #[test]
    fn should_report_vision_unsupported_when_model_name_is_text_only() {
        for model in [
            Some("gpt-3.5-turbo"),
            Some("deepseek-chat"),
            Some("llama-3.1-8b"),
            Some("mistral-large"),
            Some("text-davinci-003"),
        ] {
            assert!(
                !model_supports_vision(model),
                "{model:?} should not support vision"
            );
        }
    }

    #[test]
    fn should_default_to_vision_supported_when_model_is_unknown() {
        assert!(model_supports_vision(None));
        assert!(model_supports_vision(Some("custom-model-xyz")));
    }

    #[test]
    fn should_report_vision_supported_when_catalog_entry_lists_vision_capability() {
        let entry = ModelCatalogEntry {
            id: "sonnet".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            provider: "anthropic".to_string(),
            description: None,
            max_context_size: None,
            capabilities: vec!["vision".to_string(), "tool-use".to_string()],
            default_effort: None,
        };
        assert!(entry_supports_vision(&entry));
    }

    #[test]
    fn should_report_vision_unsupported_when_catalog_entry_lacks_vision_capability() {
        let entry = ModelCatalogEntry {
            id: "deepseek".to_string(),
            model: "deepseek-chat".to_string(),
            provider: "custom".to_string(),
            description: None,
            max_context_size: None,
            capabilities: vec!["tool-use".to_string()],
            default_effort: None,
        };
        assert!(!entry_supports_vision(&entry));
    }

    #[test]
    fn should_report_correct_mime_and_extension_when_format_is_known() {
        for (format, mime, ext) in [
            (ImageFormat::Png, "image/png", "png"),
            (ImageFormat::Jpeg, "image/jpeg", "jpg"),
            (ImageFormat::Gif, "image/gif", "gif"),
            (ImageFormat::Webp, "image/webp", "webp"),
            (ImageFormat::Bmp, "image/bmp", "bmp"),
        ] {
            assert_eq!(format.mime_type(), mime, "{format:?}");
            assert_eq!(format.extension(), ext, "{format:?}");
        }
    }

    #[test]
    fn should_detect_format_from_extension_when_path_has_known_extension() {
        assert_eq!(
            ImageFormat::from_extension(Path::new("a.png")),
            Some(ImageFormat::Png)
        );
        assert_eq!(
            ImageFormat::from_extension(Path::new("a.JPG")),
            Some(ImageFormat::Jpeg),
            "extensions are matched case-insensitively"
        );
        assert_eq!(
            ImageFormat::from_extension(Path::new("a.jpeg")),
            Some(ImageFormat::Jpeg)
        );
        assert_eq!(
            ImageFormat::from_extension(Path::new("a.gif")),
            Some(ImageFormat::Gif)
        );
        assert_eq!(
            ImageFormat::from_extension(Path::new("a.webp")),
            Some(ImageFormat::Webp)
        );
        assert_eq!(
            ImageFormat::from_extension(Path::new("a.bmp")),
            Some(ImageFormat::Bmp)
        );
        assert_eq!(ImageFormat::from_extension(Path::new("a.txt")), None);
        assert_eq!(ImageFormat::from_extension(Path::new("no_extension")), None);
    }

    #[test]
    fn should_detect_image_format_when_bytes_match_a_known_magic() {
        // PNG: \x89PNG\r\n\x1a\n
        assert_eq!(
            detect_image_format(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]),
            Some(ImageFormat::Png)
        );
        // JPEG: \xFF\xD8\xFF
        assert_eq!(
            detect_image_format(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some(ImageFormat::Jpeg)
        );
        // GIF: "GIF87a" / "GIF89a"
        assert_eq!(detect_image_format(b"GIF87a"), Some(ImageFormat::Gif));
        assert_eq!(detect_image_format(b"GIF89a"), Some(ImageFormat::Gif));
        // WebP: "RIFF" + size + "WEBP"
        assert_eq!(
            detect_image_format(&[b'R', b'I', b'F', b'F', 0, 0, 0, 0, b'W', b'E', b'B', b'P']),
            Some(ImageFormat::Webp)
        );
        // BMP: "BM" + reserved zero bytes at offset 6-7
        assert_eq!(
            detect_image_format(&[0x42, 0x4D, 0, 0, 0, 0, 0, 0]),
            Some(ImageFormat::Bmp)
        );
    }

    #[test]
    fn should_return_none_when_bytes_match_no_known_image_magic() {
        assert!(detect_image_format(b"plain text file").is_none());
        // A truncated PNG signature is not a valid magic sequence.
        assert!(detect_image_format(&[0x89, 0x50, 0x4E, 0x47]).is_none());
        // "BM" alone with non-zero reserved bytes is not a BMP.
        assert!(detect_image_format(&[0x42, 0x4D, 0, 0, 0, 0, 0x01, 0x02]).is_none());
    }
}
