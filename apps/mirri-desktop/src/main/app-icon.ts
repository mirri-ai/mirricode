// apps/mirri-desktop/src/main/app-icon.ts
//
// Real macOS bundle-icon extraction for the "Open In" dropdown. Electron's
// app.getFileIcon() returns a generic placeholder for .app directories (it
// cannot read the bundle's embedded .icns), so this module reads the icon
// directly:
//
//   Contents/Info.plist → CFBundleIconFile → Contents/Resources/<icon>.icns
//   → icns chunks (ic07..ic14) → embedded PNG → data URL
//
// MAIN-PROCESS ONLY. This module must never be imported by src/main/preload.ts
// or src/main/ipc.ts (the preload bundle must stay fully self-contained);
// register-ipc.ts is its only importer. Everything here is best-effort: any
// error (missing file, malformed plist/icns, plutil failing) yields undefined
// and the caller shows the app without an icon.

import { execFile } from 'node:child_process';
import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { promisify } from 'node:util';

const execFileAsync = promisify(execFile);

/** icns chunk type → its physical pixel size. ic12/ic13/ic14 are the @2x
 *  variants of the logical 256/128/512 sizes; the physical (retina) pixels
 *  are what matters for payload selection. */
const ICNS_CHUNK_PIXELS: Readonly<Record<string, number>> = {
  ic07: 128,
  ic08: 256,
  ic09: 512,
  ic10: 1024,
  ic11: 64, // 32pt @2x
  ic12: 512, // 256pt @2x → physical 512
  ic13: 256, // 128pt @2x → physical 256
  ic14: 1024, // 512pt @2x → physical 1024
};

/** The "Open In" slot renders at ~18 CSS px; any chunk ≥256px is already
 *  sharper than the slot needs, so pick the SMALLEST such chunk to keep the
 *  data-URL payload sane (a 1024px PNG can be ~200KB+). */
const TARGET_PIXELS = 256;

const PNG_MAGIC = Buffer.from([0x89, 0x50, 0x4e, 0x47]);
const ICNS_MAGIC = 'icns';
const BPLIST_MAGIC = 'bplist00';

/** CFBundleIconFile value → icns path under Contents/Resources. Some plists
 *  give the file without an .icns extension; the extensionless raw value is
 *  tried last because a few bundles name the actual file differently. */
function iconFileCandidates(iconFile: string): string[] {
  const resources = (name: string) => join('Contents', 'Resources', name);
  if (iconFile.endsWith('.icns')) {
    return [resources(iconFile)];
  }
  return [resources(`${iconFile}.icns`), resources(iconFile)];
}

/** Read Contents/Info.plist, return its raw text. Binary plists (bplist00
 *  magic) are converted to XML through `plutil` (macOS-only code, so the
 *  binary is guaranteed); returns undefined when unreadable either way. The
 *  conversion failure path is untestable via fixtures — `plutil` is real
 *  here — so any error simply falls through to undefined. */
async function readInfoPlistText(bundlePath: string): Promise<string | undefined> {
  const plistPath = join(bundlePath, 'Contents', 'Info.plist');
  let plistText: string;
  try {
    plistText = await readFile(plistPath, 'utf-8');
  } catch {
    return undefined;
  }
  if (plistText.startsWith(BPLIST_MAGIC)) {
    try {
      const { stdout } = await execFileAsync('plutil', ['-convert', 'xml1', '-o', '-', '--', plistPath]);
      plistText = stdout;
    } catch {
      return undefined;
    }
  }
  return plistText;
}

/** `<key>CFBundleIconFile</key>` followed shortly by `<string>…</string>`.
 *  Plain capture of the string content — no entity decoding (app icon names
 *  are simple filenames; a stray `&` in a name would be decoded wrong but the
 *  subsequent file lookup simply fails and the icon is omitted). */
function extractIconFile(plistText: string): string | undefined {
  const match = plistText.match(/<key>CFBundleIconFile<\/key>\s*<string>([^<]*)<\/string>/);
  if (match === null || match[1] === undefined) return undefined;
  const iconFile = match[1].trim();
  return iconFile.length > 0 ? iconFile : undefined;
}

/** Walk the icns chunk table and pick the PNG payload to embed. Returns
 *  undefined when the file is not an icns, holds no PNG chunk, or the length
 *  fields are corrupt (all chunks must stay within the file). */
function pickPngChunk(icns: Buffer): Buffer | undefined {
  if (icns.length < 8 || icns.toString('ascii', 0, 4) !== ICNS_MAGIC) return undefined;
  const declaredTotal = icns.readUInt32BE(4);
  if (declaredTotal < 8 || declaredTotal > icns.length) return undefined;

  const candidates: Array<{ pixels: number; data: Buffer }> = [];
  let offset = 8;
  while (offset + 8 <= declaredTotal) {
    const type = icns.toString('ascii', offset, offset + 4);
    const chunkLength = icns.readUInt32BE(offset + 4);
    if (chunkLength < 8 || offset + chunkLength > declaredTotal) break;
    const dataStart = offset + 8;
    const dataEnd = offset + chunkLength;
    const data = icns.subarray(dataStart, dataEnd);
    if (data.length > PNG_MAGIC.length && data.subarray(0, PNG_MAGIC.length).equals(PNG_MAGIC)) {
      candidates.push({ pixels: ICNS_CHUNK_PIXELS[type] ?? 0, data });
    }
    offset += chunkLength;
  }
  if (candidates.length === 0) return undefined;

  const atOrAboveTarget = candidates
    .filter((c) => c.pixels >= TARGET_PIXELS)
    .toSorted((a, b) => a.pixels - b.pixels);
  if (atOrAboveTarget.length > 0) return atOrAboveTarget[0]!.data;

  // 没有任何 chunk 达到目标尺寸 —— 退回最大的那个（保持原始像素数据）。
  const largest = candidates.reduce((best, c) => (c.pixels > best.pixels ? c : best), candidates[0]!);
  return largest.data;
}

/** Extract the bundle's embedded icon as a PNG data URL. Any failure returns
 *  undefined — icon extraction is best-effort and must never break detection. */
export async function readAppBundleIcon(bundlePath: string): Promise<string | undefined> {
  try {
    const plistText = await readInfoPlistText(bundlePath);
    if (plistText === undefined) return undefined;
    const iconFile = extractIconFile(plistText);
    if (iconFile === undefined) return undefined;

    for (const candidate of iconFileCandidates(iconFile)) {
      let icns: Buffer;
      try {
        icns = await readFile(join(bundlePath, candidate));
      } catch {
        continue; // 该候选名不存在（如追加 .icns 后仍无文件）→ 试下一个
      }
      const png = pickPngChunk(icns);
      if (png !== undefined) return `data:image/png;base64,${png.toString('base64')}`;
    }
    return undefined;
  } catch {
    // 一切错误都视为"拿不到图标"，不影响 app 列表本身。
    return undefined;
  }
}