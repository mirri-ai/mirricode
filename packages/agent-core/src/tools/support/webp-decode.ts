/**
 * WebP decode helpers for image-compress.ts.
 *
 * The decoder wasm is shipped as an inline base64 string (webp-dec-wasm.ts)
 * so the bundled CLI has no runtime dependency on a .wasm file path. The
 * decode function loads the wasm lazily on first use and caches the module.
 */

import { init, default as decode } from '@jsquash/webp/decode.js';

import { WEBP_DECODER_WASM_BASE64 } from './webp-dec-wasm';

interface DecodedImage {
  readonly width: number;
  readonly height: number;
  readonly data: Uint8ClampedArray;
}

let initialized = false;

/**
 * Decode a WebP image to raw RGBA pixels. Returns null on any failure
 * (corrupt data, unsupported features, wasm load error).
 */
export async function decodeWebp(
  bytes: Uint8Array,
): Promise<DecodedImage | null> {
  try {
    if (!initialized) {
      const wasmBuf = Buffer.from(WEBP_DECODER_WASM_BASE64, 'base64');
      try {
        await init({ wasmBinary: wasmBuf });
      } catch {
        // Already initialized — ignore.
      }
      initialized = true;
    }
    const imageData = await decode(bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer);
    return { width: imageData.width, height: imageData.height, data: imageData.data };
  } catch {
    return null;
  }
}

/**
 * Detect animated WebP by inspecting the ANIM and ANMF chunk markers.
 * Animated WebP must pass through to preserve frame data.
 */
export function isAnimatedWebp(bytes: Uint8Array): boolean {
  // WebP container: RIFF....WEBP followed by chunks.
  // ANIM chunk (0x414E494D) signals animation; ANMF (0x414E4D46) is a frame.
  if (bytes.length < 16) return false;
  // Check RIFF header
  if (bytes[0] !== 0x52 || bytes[1] !== 0x49 || bytes[2] !== 0x46 || bytes[3] !== 0x46) return false;
  if (bytes[8] !== 0x57 || bytes[9] !== 0x45 || bytes[10] !== 0x42 || bytes[11] !== 0x50) return false;
  // Scan for ANIM chunk marker in the first 256 bytes
  for (let i = 12; i < Math.min(bytes.length - 3, 256); i++) {
    if (bytes[i] === 0x41 && bytes[i + 1] === 0x4E && bytes[i + 2] === 0x49 && bytes[i + 3] === 0x4D) {
      return true;
    }
  }
  return false;
}
