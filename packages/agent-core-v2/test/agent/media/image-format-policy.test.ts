/**
 * image-format-policy — provider-accepted image formats, the single source
 * of truth.
 *
 * Tests pin:
 *   - MODEL_ACCEPTED_IMAGE_MIMES is the closed set {png, jpeg, gif, webp}
 *   - normalizeImageMime: case, whitespace, parameters, jpg alias
 *   - isModelAcceptedImageMime: only the four accepted types pass
 *   - parseImageDataUrl: extracts MIME and base64; rejects non-data URLs and
 *     malformed data URLs
 *   - resolveEffectiveImageMime: sniffed bytes override the declared MIME;
 *     declared MIME stands when the sniffer returns null
 *   - unsupportedImageMimeFromUrl: flags known-bad extensions, ignores
 *     query/fragment/case
 *   - buildUnsupportedImageNotice / buildMalformedImageNotice: human-readable
 *     notices naming the format or the URL
 *   - buildImageConversionGuidance: OS-specific conversion commands
 *   - decodeBase64Prefix: returns the first 48 chars' worth of bytes
 */

import { tmpdir } from 'node:os';

import { describe, expect, it } from 'vitest';

import {
  buildImageConversionGuidance,
  buildMalformedImageNotice,
  buildUnsupportedImageNotice,
  decodeBase64Prefix,
  isDataUrl,
  isModelAcceptedImageMime,
  MODEL_ACCEPTED_IMAGE_MIMES,
  normalizeImageMime,
  parseImageDataUrl,
  resolveEffectiveImageMime,
  unsupportedImageMimeFromUrl,
} from '#/agent/media/image-format-policy';

describe('MODEL_ACCEPTED_IMAGE_MIMES', () => {
  it('contains exactly PNG, JPEG, GIF, and WebP', () => {
    expect(MODEL_ACCEPTED_IMAGE_MIMES).toEqual(new Set(['image/png', 'image/jpeg', 'image/gif', 'image/webp']));
  });
});

describe('normalizeImageMime', () => {
  it('passes canonical forms through unchanged', () => {
    expect(normalizeImageMime('image/png')).toBe('image/png');
    expect(normalizeImageMime('image/jpeg')).toBe('image/jpeg');
    expect(normalizeImageMime('image/gif')).toBe('image/gif');
    expect(normalizeImageMime('image/webp')).toBe('image/webp');
  });

  it('lowercases and strips surrounding whitespace', () => {
    expect(normalizeImageMime('Image/JPEG')).toBe('image/jpeg');
    expect(normalizeImageMime(' image/webp ')).toBe('image/webp');
  });

  it('maps image/jpg to image/jpeg', () => {
    expect(normalizeImageMime('image/jpg')).toBe('image/jpeg');
  });

  it('strips MIME parameters after a semicolon', () => {
    expect(normalizeImageMime('image/jpeg; charset=utf-8')).toBe('image/jpeg');
    expect(normalizeImageMime('IMAGE/PNG;foo=bar')).toBe('image/png');
  });
});

describe('isModelAcceptedImageMime', () => {
  it('accepts the four provider-allowed formats', () => {
    expect(isModelAcceptedImageMime('image/png')).toBe(true);
    expect(isModelAcceptedImageMime('image/jpeg')).toBe(true);
    expect(isModelAcceptedImageMime('image/gif')).toBe(true);
    expect(isModelAcceptedImageMime('image/webp')).toBe(true);
  });

  it('accepts case variants and the jpg alias', () => {
    expect(isModelAcceptedImageMime('Image/JPEG')).toBe(true);
    expect(isModelAcceptedImageMime('image/jpg')).toBe(true);
  });

  it('rejects formats outside the accepted set', () => {
    expect(isModelAcceptedImageMime('image/avif')).toBe(false);
    expect(isModelAcceptedImageMime('image/heic')).toBe(false);
    expect(isModelAcceptedImageMime('image/bmp')).toBe(false);
    expect(isModelAcceptedImageMime('image/tiff')).toBe(false);
    expect(isModelAcceptedImageMime('image/svg+xml')).toBe(false);
    expect(isModelAcceptedImageMime('video/mp4')).toBe(false);
  });
});

describe('parseImageDataUrl', () => {
  it('extracts MIME and base64 from a canonical data URL', () => {
    const result = parseImageDataUrl('data:image/png;base64,abc123');
    expect(result).toEqual({ mimeType: 'image/png', base64: 'abc123' });
  });

  it('extracts MIME ignoring intermediate parameters', () => {
    const result = parseImageDataUrl('data:image/jpeg;charset=utf-8;base64,QUJD');
    expect(result).toEqual({ mimeType: 'image/jpeg', base64: 'QUJD' });
  });

  it('returns null for a non-data URL', () => {
    expect(parseImageDataUrl('https://example.com/pic.png')).toBeNull();
  });

  it('returns null for a data URL without the base64 marker', () => {
    expect(parseImageDataUrl('data:image/png,abc123')).toBeNull();
  });

  it('returns null for a data URL with an empty MIME', () => {
    expect(parseImageDataUrl('data:;base64,abc123')).toBeNull();
  });

  it('returns an empty base64 for a data URL with no payload after the comma', () => {
    const result = parseImageDataUrl('data:image/png;base64,');
    expect(result).not.toBeNull();
    expect(result!.mimeType).toBe('image/png');
    expect(result!.base64).toBe('');
  });
});

describe('isDataUrl', () => {
  it('returns true for data: URLs (case-insensitive)', () => {
    expect(isDataUrl('data:image/png;base64,abc')).toBe(true);
    expect(isDataUrl('DATA:image/png;base64,abc')).toBe(true);
  });

  it('returns false for non-data URLs', () => {
    expect(isDataUrl('https://example.com')).toBe(false);
    expect(isDataUrl('file:///tmp/x.png')).toBe(false);
  });
});

describe('resolveEffectiveImageMime', () => {
  it('returns the sniffed MIME when the magic bytes match a known format', () => {
    // PNG magic bytes
    const pngHeader = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13]);
    const result = resolveEffectiveImageMime('image/avif', pngHeader);
    expect(result).toBe('image/png');
  });

  it('falls back to the declared MIME when the sniffer does not recognize the bytes', () => {
    const unknown = Buffer.from([0x01, 0x02, 0x03, 0x04]);
    const result = resolveEffectiveImageMime('image/png', unknown);
    expect(result).toBe('image/png');
  });
});

describe('unsupportedImageMimeFromUrl', () => {
  it('flags known-unsupported extensions', () => {
    expect(unsupportedImageMimeFromUrl('https://example.com/pic.avif')).toBe('image/avif');
    expect(unsupportedImageMimeFromUrl('https://example.com/pic.AVIF')).toBe('image/avif');
    expect(unsupportedImageMimeFromUrl('https://example.com/photo.HEIC?x=1')).toBe('image/heic');
    expect(unsupportedImageMimeFromUrl('https://example.com/scan.tiff')).toBe('image/tiff');
    expect(unsupportedImageMimeFromUrl('https://example.com/icon.ico')).toBe('image/x-icon');
    expect(unsupportedImageMimeFromUrl('https://example.com/logo.svg')).toBe('image/svg+xml');
  });

  it('returns null for accepted, extensionless, or unknown URLs', () => {
    expect(unsupportedImageMimeFromUrl('https://example.com/pic.png')).toBeNull();
    expect(unsupportedImageMimeFromUrl('https://example.com/pic.jpg')).toBeNull();
    expect(unsupportedImageMimeFromUrl('https://example.com/avatar')).toBeNull();
    expect(unsupportedImageMimeFromUrl('https://cdn.example.com/v2/image?id=123')).toBeNull();
  });

  it('strips query strings and fragments before checking the extension', () => {
    expect(unsupportedImageMimeFromUrl('https://x.com/pic.avif?size=full')).toBe('image/avif');
    expect(unsupportedImageMimeFromUrl('https://x.com/pic.heic#frame')).toBe('image/heic');
  });
});

describe('buildUnsupportedImageNotice', () => {
  it('names the MIME when no URL is provided', () => {
    const notice = buildUnsupportedImageNotice('image/avif');
    expect(notice).toContain('image/avif');
    expect(notice).toContain('unsupported image format');
    expect(notice).toContain('PNG');
    expect(notice).toContain('JPEG');
  });

  it('includes the URL name when provided', () => {
    const notice = buildUnsupportedImageNotice('image/avif', 'https://x.com/pic.avif');
    expect(notice).toContain('pic.avif');
    expect(notice).toContain('image/avif');
  });
});

describe('buildMalformedImageNotice', () => {
  it('describes the problem and truncates a long URL', () => {
    const longUrl = `data:image/png${'x'.repeat(500)}`;
    const notice = buildMalformedImageNotice(longUrl);
    expect(notice).toContain('not a valid data URL');
    expect(notice.length).toBeLessThan(250);
  });

  it('shows the full URL when it is short', () => {
    const shortUrl = 'data:image/png';
    const notice = buildMalformedImageNotice(shortUrl);
    expect(notice).toContain(shortUrl);
  });
});

describe('buildImageConversionGuidance', () => {
  it('gives macOS-specific sips command', () => {
    const guidance = buildImageConversionGuidance('photo.heic', 'image/heic', 'macOS');
    expect(guidance).toContain('sips');
    expect(guidance).toContain('photo.heic');
    expect(guidance).toContain('photo.jpg');
  });

  it('gives Linux-specific commands for HEIC with libheif', () => {
    const guidance = buildImageConversionGuidance('photo.heic', 'image/heic', 'Linux');
    expect(guidance).toContain('heif-convert');
    expect(guidance).toContain('libheif-examples');
  });

  it('gives ImageMagick for formats without a Linux decoder', () => {
    const guidance = buildImageConversionGuidance('pic.avif', 'image/avif', 'Linux');
    expect(guidance).toContain('magick');
    expect(guidance).not.toContain('heif-convert');
  });

  it('gives Windows ImageMagick with install hint', () => {
    const guidance = buildImageConversionGuidance('pic.avif', 'image/avif', 'Windows');
    expect(guidance).toContain('magick');
    expect(guidance).toContain('winget');
  });
});

describe('decodeBase64Prefix', () => {
  it('decodes the first 48 base64 characters (36 bytes)', () => {
    // 48 base64 chars → 36 bytes
    const base64 = 'AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcWGRQVFhcWGRQVFhcWGQ==';
    const result = decodeBase64Prefix(base64);
    expect(result).toBeInstanceOf(Buffer);
    expect(result.length).toBe(36);
  });

  it('handles a short base64 string', () => {
    const result = decodeBase64Prefix('AQID');
    expect(result.length).toBeGreaterThan(0);
  });
});
