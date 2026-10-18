//! Multimodal entry pipeline (spec architecture §3.3–§3.4): archive the
//! originals and stamp the recovery handle, then keep the in-message
//! view within the entry budget. The blob store is the session's
//! content-addressed store (same root the wire-log offload uses).

use base64::Engine;

use crate::blob::{AgentBlobService, format_data_uri, parse_data_uri};
use crate::image_pipeline::{self, compress_media, detect_image_format};
use crate::message::ContentPart;

/// Entry budget (multimodal spec architecture §3.4): the byte and edge
/// ceilings an image must fit to enter session history as-is.
const ENTRY_MAX_EDGE_PX: u32 = 2000;
const ENTRY_BYTE_BUDGET: usize = 3_932_160; // 3.75 MB
const ENTRY_JPEG_QUALITY: u8 = 85;

/// Ingest prompt media: archive the originals first (E2 — the original
/// is the fidelity-upgrade source), then compress the in-message view
/// within the entry budget (§3.4 — the byte pressure bound every later
/// request inherits). Fail-open: a part that cannot be decoded or
/// re-encoded keeps its original view.
pub fn ingest_prompt_media(blob_service: &AgentBlobService, parts: &mut [ContentPart]) {
    blob_service.persist_image_originals(parts);
    compress_entry_images(parts);
}

/// Re-encode image views that exceed the entry budget. The archived
/// original (the part's `id` handle, already stamped by
/// [`AgentBlobService::persist_image_originals`]) is never touched —
/// compression is a view concern and the archive keeps the pristine
/// bytes (ADR-004).
fn compress_entry_images(parts: &mut [ContentPart]) {
    for part in parts.iter_mut() {
        let ContentPart::ImageUrl { image_url, .. } = part else {
            continue;
        };
        let Some((_mime, base64_payload)) = parse_data_uri(&image_url.url) else {
            continue;
        };
        let Ok(data) = base64::engine::general_purpose::STANDARD.decode(base64_payload) else {
            continue;
        };
        let Some(format) = detect_image_format(&data) else {
            continue;
        };
        let (width, height) = image_pipeline::probe_dimensions(&data).unwrap_or((0, 0));
        let oversized = data.len() > ENTRY_BYTE_BUDGET || width.max(height) > ENTRY_MAX_EDGE_PX;
        if !oversized {
            continue;
        }
        if let Ok(compressed) = compress_media(&data, format, ENTRY_MAX_EDGE_PX, ENTRY_JPEG_QUALITY)
            && compressed.compressed
            && compressed.data.len() < data.len()
        {
            image_url.url = format_data_uri(compressed.format.mime_type(), &compressed.data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ImageUrlData;
    use image::ImageEncoder;

    /// Encode a seeded-noise RGBA image as PNG — noise is incompressible,
    /// so the fixture predictably exceeds the entry byte budget while its
    /// edges stay under the pixel ceiling.
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

    /// Encode a smooth RGBA gradient as PNG — compresses to a few hundred
    /// bytes, comfortably inside the entry budget.
    fn gradient_png(width: u32, height: u32) -> Vec<u8> {
        let mut img = image::RgbaImage::new(width, height);
        for (x, y, pixel) in img.enumerate_pixels_mut() {
            *pixel = image::Rgba([
                (x * 255 / width.max(1)) as u8,
                (y * 255 / height.max(1)) as u8,
                128,
                255,
            ]);
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

    /// An inline image part carrying `data` as a base64 data URI.
    fn image_part(data: &[u8]) -> ContentPart {
        ContentPart::ImageUrl {
            image_url: ImageUrlData {
                url: format_data_uri("image/png", data),
                detail: None,
                id: None,
            },
            detail: None,
        }
    }

    /// Decode the part's data-URI payload back to raw bytes.
    fn view_bytes(part: &ContentPart) -> Vec<u8> {
        let ContentPart::ImageUrl { image_url, .. } = part else {
            panic!("expected an image part");
        };
        let (_, b64) = parse_data_uri(&image_url.url).expect("data URI");
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .expect("valid base64")
    }

    #[test]
    fn should_compress_oversized_image_view_but_keep_original_in_id() {
        // Given 一张超过入口字节预算的 PNG 图片部件
        let dir = tempfile::tempdir().unwrap();
        let service = AgentBlobService::new(dir.path().to_path_buf());
        let original = noisy_png(1120, 1000);
        assert!(
            original.len() > ENTRY_BYTE_BUDGET,
            "fixture must exceed the byte budget: {} bytes",
            original.len()
        );
        let original_url = format_data_uri("image/png", &original);
        let mut parts = vec![image_part(&original)];

        // When 入口 ingest（留档+压缩）
        ingest_prompt_media(&service, &mut parts);

        // Then 消息内 url 换为压缩视图、id 指向的原件字节不变
        // （§3.4 行为契约：压缩与留档正交）
        let ContentPart::ImageUrl { image_url, .. } = &parts[0] else {
            panic!("expected an image part");
        };
        assert_ne!(image_url.url, original_url, "the view should be re-encoded");
        let view = view_bytes(&parts[0]);
        assert!(
            view.len() < original.len(),
            "view ({}) should be smaller than the original ({})",
            view.len(),
            original.len()
        );
        assert!(
            view.len() < ENTRY_BYTE_BUDGET,
            "view ({}) should fit the entry budget",
            view.len()
        );
        assert!(
            view.starts_with(&[0xFF, 0xD8, 0xFF]),
            "a large PNG is re-encoded as JPEG"
        );
        let decoded = image::load_from_memory(&view).expect("decodable view");
        assert!(decoded.width().max(decoded.height()) <= ENTRY_MAX_EDGE_PX);
        let id = image_url.id.as_deref().expect("archived original path");
        assert_eq!(
            std::fs::read(id).unwrap(),
            original,
            "the original bytes are archived untouched"
        );
    }

    #[test]
    fn should_not_touch_images_within_entry_budget() {
        // Given 尺寸与字节均在预算内的小图
        let png = gradient_png(64, 64);
        let url = format_data_uri("image/png", &png);
        let mut parts = vec![image_part(&png)];

        // When 入口压缩（无适配时直通，US7.4）
        compress_entry_images(&mut parts);

        // Then url 原样保留
        let ContentPart::ImageUrl { image_url, .. } = &parts[0] else {
            panic!("expected an image part");
        };
        assert_eq!(
            image_url.url, url,
            "in-budget images pass through untouched"
        );
    }

    #[test]
    fn should_fail_open_when_compression_fails() {
        // Given 损坏的字节声称是 data:image/png（PNG 魔数 + 超预算的垃圾，
        // 无法解码）
        let mut data = vec![0u8; ENTRY_BYTE_BUDGET + 64];
        data[..8].copy_from_slice(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
        let url = format_data_uri("image/png", &data);
        let mut parts = vec![image_part(&data)];

        // When 入口压缩
        compress_entry_images(&mut parts);

        // Then url 原样保留（压缩失败不吞图片）
        let ContentPart::ImageUrl { image_url, .. } = &parts[0] else {
            panic!("expected an image part");
        };
        assert_eq!(
            image_url.url, url,
            "a part that cannot be re-encoded keeps its view"
        );
    }
}
