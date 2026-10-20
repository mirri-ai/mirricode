// apps/mirri-desktop/src/main/app-icon.test.ts
//
// Fixture-based tests for readAppBundleIcon: each test writes a minimal fake
// .app bundle (Contents/Info.plist + Contents/Resources/<Icon>.icns) into a
// temp dir, then asserts the extracted data URL. Real-file fixtures exercise
// the real fs read path; the binary-plist case uses a PRE-CONVERTED fixture
// committed under test/fixtures (generated once with `plutil -convert
// binary1`) so the test itself never needs plutil — on macOS the runtime
// plutil fallback converts it to XML, on Linux it fails to undefined, and
// either way the test suite runs without plutil being installed.

import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { readAppBundleIcon } from './app-icon';

/** A real, decodable 1×1 transparent PNG — every synthesized icns chunk
 *  embeds it (selection is driven by the chunk TYPE, not real pixels). */
const PNG_1X1 = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==',
  'base64',
);

/** Data URL a chunk of the given type would produce. The per-type tag makes
 *  chunk selection observable (all chunks otherwise share the same PNG). */
function expectedDataUrl(type: string): string {
  const taggedPng = Buffer.concat([PNG_1X1, Buffer.from(type, 'ascii')]);
  return `data:image/png;base64,${taggedPng.toString('base64')}`;
}

/** A minimal icns file: 8-byte header + one chunk per entry. Each chunk is
 *  [4-byte type][4-byte BE length incl. its 8-byte header][payload]. A
 *  pngSize of 0 marks a NON-PNG payload (legacy is32/il32-style chunk). */
function icnsBuffer(entries: Array<{ type: string; pngSize: number }>): Buffer {
  const parts: Buffer[] = [];
  for (const { type, pngSize } of entries) {
    const payload = pngSize > 0 ? Buffer.concat([PNG_1X1, Buffer.from(type, 'ascii')]) : Buffer.alloc(8);
    const header = Buffer.alloc(8);
    header.write(type, 0, 'ascii');
    header.writeUInt32BE(8 + payload.length, 4);
    parts.push(header, payload);
  }
  const total = 8 + parts.reduce((sum, p) => sum + p.length, 0);
  const out = Buffer.alloc(total);
  out.write('icns', 0, 'ascii');
  out.writeUInt32BE(total, 4);
  let offset = 8;
  for (const part of parts) {
    part.copy(out, offset);
    offset += part.length;
  }
  return out;
}

interface FakeBundleOptions {
  /** Info.plist content — string (XML) or Buffer (pre-converted binary plist fixture). */
  plist: string | Buffer;
  iconEntries?: Array<{ type: string; pngSize: number }>;
  /** Resources file name; defaults to `AppIcon` (extensionless in Resources
   *  spawns the ".icns appended" resolution path). */
  iconFileName?: string;
}

async function writeAppBundle(rootDir: string, options: FakeBundleOptions): Promise<string> {
  const appDir = join(rootDir, 'Fake.app');
  const contentsDir = join(appDir, 'Contents');
  const resourcesDir = join(contentsDir, 'Resources');
  await mkdir(resourcesDir, { recursive: true });
  await writeFile(join(contentsDir, 'Info.plist'), options.plist);
  if (options.iconEntries !== undefined) {
    await writeFile(join(resourcesDir, options.iconFileName ?? 'AppIcon'), icnsBuffer(options.iconEntries));
  }
  return appDir;
}

const PLIST_WITH_APP_ICON = `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIconFile</key>
  <string>AppIcon</string>
</dict>
</plist>`;

const PLIST_WITHOUT_ICON_FILE = `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>
  <string>Fake</string>
</dict>
</plist>`;

const PLIST_WITH_CODED_ICON_FILE = `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIconFile</key>
  <string>Code.icns</string>
</dict>
</plist>`;

describe('readAppBundleIcon', () => {
  let dir: string;

  beforeEach(async () => {
    dir = await mkdtemp(join(tmpdir(), 'app-icon-test-'));
  });

  afterEach(async () => {
    await rm(dir, { recursive: true, force: true });
  });

  it('should return a png data url when the bundle has an xml plist and a multi-chunk icns', async () => {
    const appDir = await writeAppBundle(dir, {
      plist: PLIST_WITH_APP_ICON,
      iconEntries: [
        { type: 'ic07', pngSize: 128 },
        { type: 'ic12', pngSize: 512 },
      ],
    });
    // 唯一 ≥256 的 chunk 是 ic12（512px）——选它而不是 128px 的 ic07
    await expect(readAppBundleIcon(appDir)).resolves.toBe(expectedDataUrl('ic12'));
  });

  it('should prefer the smallest chunk at or above the target size when the icns contains many sizes', async () => {
    const appDir = await writeAppBundle(dir, {
      plist: PLIST_WITH_APP_ICON,
      iconEntries: [
        { type: 'ic10', pngSize: 1024 },
        { type: 'ic12', pngSize: 512 },
        { type: 'ic13', pngSize: 256 },
        { type: 'ic07', pngSize: 128 },
        { type: 'ic11', pngSize: 64 },
      ],
    });
    // ≥256 的候选是 ic10/ic12/ic13，最小的是 ic13（256px）
    await expect(readAppBundleIcon(appDir)).resolves.toBe(expectedDataUrl('ic13'));
  });

  it('should fall back to the largest chunk when no chunk reaches the target size', async () => {
    const appDir = await writeAppBundle(dir, {
      plist: PLIST_WITH_APP_ICON,
      iconEntries: [
        { type: 'ic11', pngSize: 64 },
        { type: 'ic07', pngSize: 128 },
      ],
    });
    // 全部 <256：即使 ic07 是第二个 chunk，也应按尺寸（而非位置）选中最大的
    await expect(readAppBundleIcon(appDir)).resolves.toBe(expectedDataUrl('ic07'));
  });

  it('should return undefined when the icns contains no png chunk', async () => {
    const appDir = await writeAppBundle(dir, {
      plist: PLIST_WITH_APP_ICON,
      iconEntries: [{ type: 'is32', pngSize: 0 }],
    });
    // legacy is32/il32+mask 是像素数据而非 PNG —— 提取不到任何 PNG chunk
    await expect(readAppBundleIcon(appDir)).resolves.toBeUndefined();
  });

  it('should return undefined when an icns chunk declares a length beyond the file end', async () => {
    const appDir = await writeAppBundle(dir, {
      plist: PLIST_WITH_APP_ICON,
      iconEntries: [{ type: 'ic13', pngSize: 256 }],
    });
    const icnsPath = join(appDir, 'Contents', 'Resources', 'AppIcon');
    const icns = await readFile(icnsPath);
    icns.writeUInt32BE(4000, 12); // chunk length 字段：文件头 8 字节后的 4 字节
    await writeFile(icnsPath, icns);
    // 唯一的 chunk 声明长度远超文件末尾（整文件 ~88 字节）→ 不能读出越界/垃圾数据
    await expect(readAppBundleIcon(appDir)).resolves.toBeUndefined();
  });

  it('should return undefined when an icns chunk declares a length below its 8-byte header', async () => {
    const appDir = await writeAppBundle(dir, {
      plist: PLIST_WITH_APP_ICON,
      iconEntries: [{ type: 'ic13', pngSize: 256 }],
    });
    const icnsPath = join(appDir, 'Contents', 'Resources', 'AppIcon');
    const icns = await readFile(icnsPath);
    icns.writeUInt32BE(4, 12); // chunkLength = 4 < 8
    await writeFile(icnsPath, icns);
    await expect(readAppBundleIcon(appDir)).resolves.toBeUndefined();
  });

  it('should return undefined when every icns chunk has a non-png payload', async () => {
    const appDir = await writeAppBundle(dir, {
      plist: PLIST_WITH_APP_ICON,
      iconEntries: [
        { type: 'is32', pngSize: 0 },
        { type: 'il32', pngSize: 0 },
      ],
    });
    // 全部是 legacy 像素数据（is32/il32 对）——一个 PNG chunk 都没有
    await expect(readAppBundleIcon(appDir)).resolves.toBeUndefined();
  });

  it('should return undefined when the plist has no CFBundleIconFile', async () => {
    const appDir = await writeAppBundle(dir, { plist: PLIST_WITHOUT_ICON_FILE });
    await expect(readAppBundleIcon(appDir)).resolves.toBeUndefined();
  });

  it('should return undefined when the icon file is missing from Resources', async () => {
    const appDir = await writeAppBundle(dir, { plist: PLIST_WITH_APP_ICON });
    await expect(readAppBundleIcon(appDir)).resolves.toBeUndefined();
  });

  it('should read the icon when CFBundleIconFile has no .icns extension', async () => {
    const appDir = await writeAppBundle(dir, {
      plist: PLIST_WITH_APP_ICON,
      iconEntries: [{ type: 'ic13', pngSize: 256 }],
    });
    // plist 写的是 `AppIcon`（无扩展名）→ 按 `AppIcon.icns` 找到 Resources 里的文件
    await expect(readAppBundleIcon(appDir)).resolves.toBe(expectedDataUrl('ic13'));
  });

  it('should read the icon when CFBundleIconFile already ends with .icns', async () => {
    const appDir = await writeAppBundle(dir, {
      plist: PLIST_WITH_CODED_ICON_FILE,
      iconEntries: [{ type: 'ic13', pngSize: 256 }],
      iconFileName: 'Code.icns',
    });
    await expect(readAppBundleIcon(appDir)).resolves.toBe(expectedDataUrl('ic13'));
  });

  it('should fall back to the raw value when the .icns-appended file does not exist', async () => {
    const appDir = await writeAppBundle(dir, {
      plist: PLIST_WITH_APP_ICON,
      iconEntries: [{ type: 'ic13', pngSize: 256 }],
      iconFileName: 'AppIcon', // Resources 里只有无扩展名的原始文件
    });
    // `AppIcon.icns` 不存在 → 再试原始值 `AppIcon`
    await expect(readAppBundleIcon(appDir)).resolves.toBe(expectedDataUrl('ic13'));
  });

  it('should resolve a binary plist via the plutil fallback when Info.plist is bplist00', async () => {
    const plist = await readFile(join(__dirname, '..', '..', 'test', 'fixtures', 'Info-binary.plist'));
    // 预先用 `plutil -convert binary1` 转好的真实 bplist00 文件（见文件头注释）
    const appDir = await writeAppBundle(dir, {
      plist,
      iconEntries: [{ type: 'ic13', pngSize: 256 }],
    });
    // macOS 上走 plutil → XML 的路径；Linux/CI 上 plutil 不存在 → 回落 undefined
    if (process.platform === 'darwin') {
      await expect(readAppBundleIcon(appDir)).resolves.toBe(expectedDataUrl('ic13'));
    } else {
      await expect(readAppBundleIcon(appDir)).resolves.toBeUndefined();
    }
  });

  it('should return undefined when the plist is not a plist', async () => {
    const appDir = await writeAppBundle(dir, { plist: 'garbage, definitely not a plist' });
    await expect(readAppBundleIcon(appDir)).resolves.toBeUndefined();
  });

  it('should return undefined when the bundle path does not exist', async () => {
    await expect(readAppBundleIcon(join(dir, 'Missing.app'))).resolves.toBeUndefined();
  });
});