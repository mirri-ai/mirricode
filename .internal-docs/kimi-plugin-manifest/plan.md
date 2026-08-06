# kimi 插件清单兼容支持 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 v1 / v2 两套引擎的插件清单发现与 zip 根目录探测识别 kimi 生态清单名（`kimi.plugin.json` / `.kimi-plugin/plugin.json`），使 `obra/superpowers` 这类只带 kimi 清单的插件可被安装。

**Architecture:** 在 v1（`packages/agent-core`）与 v2（`packages/agent-core-v2`）引擎各自的 `manifest.ts`（清单解析）与 `archive.ts`（zip 解压后根目录探测）中，把 kimi 两件套追加为「兼容兜底」候选，优先级表由用户确认（root 形态先于 dir 形态、Mirri 品牌先于 kimi）；同步更新发布脚本与用户文档。测试全部追加到既有测试文件。

**Tech Stack:** TypeScript、vitest、Node.js `node:fs/promises`、yazl/yauzl、zod（无关，不涉及）。

## Global Constraints

- 清单全局优先级（两引擎共用，来自 spec §3.1，**用户已确认**）：
  1. `mirri-plugin.json`（root，现行）
  2. `mirri.plugin.json`（root，legacy）
  3. `.mirr-plugin/plugin.json`（dir，legacy）
  4. `.mirricode-plugin/plugin.json`（dir，现行）
  5. `kimi.plugin.json`（root，compat）
  6. `.kimi-plugin/plugin.json`（dir，compat）
- root 形态一律优先于 dir 形态；Mirri 命名（1–4）一律优先于 kimi（5–6）。
- 「shadowed」语义不变：本任务仅在「同名 root + dir 成对」且用户已安装时报告；`kimi` 与其他清单并存**不算** shadowed。
- 不改动 klient wire 契约、不改动已有测试对行为断言的语义（只可能新增测试）。
- 测试命名遵循 Given-When-Then；**所有新增测试追加到现有测试文件**；不新建测试文件。
- 桌面约束：改代码不得脱离此仓库。提交前缀为任务标题规范（每个 commit 一个任务粒度）。

---

## File Structure

- 修改（源码层）：
  - `packages/agent-core/src/plugin/manifest.ts` — v1 清单解析（报错入口）
  - `packages/agent-core/src/plugin/types.ts` — v1 `PluginManifestKind` 枚举
  - `packages/agent-core/src/plugin/archive.ts` — v1 zip 解压根探测
  - `packages/agent-core-v2/src/app/plugin/manifest.ts` — v2 清单解析
  - `packages/agent-core-v2/src/app/plugin/archive.ts` — v2 zip 根探测
- 脚本：`apps/mirri-code/scripts/plugin-manifest-version.mjs`
- 文档：`docs/zh/customization/plugins.md`、`docs/en/customization/plugins.md`
- 测试（全部追加）：`packages/agent-core/test/plugin/manifest.test.ts`、`packages/agent-core/test/plugin/archive.test.ts`、`packages/agent-core/test/plugin/manager.test.ts`、`packages/agent-core-v2/test/app/plugin/manifest.test.ts`、`packages/agent-core-v2/test/app/plugin/archive.test.ts`

---

### Task 1: v1 引擎清单发现支持 kimi

**Files:**
- Modify: `packages/agent-core/src/plugin/manifest.ts:19-82`
- Modify: `packages/agent-core/src/plugin/types.ts:86`
- Test: `packages/agent-core/test/plugin/manifest.test.ts`

**Interfaces:**
- 复用既有导出：`parseManifest(pluginRoot: string): Promise<ParsedManifestResult>`（`ParsedManifestResult.manifestKind`/`manifestPath`/`shadowedManifestPath`/`diagnostics` 不变）。
- 新增枚举值：`PluginManifestKind` = `'mirri-plugin-root' | 'mirri-plugin-dir' | 'kimi-plugin-root' | 'kimi-plugin-dir'`。

- [ ] **Step 1: 写失败的测试**（追加到 `packages/agent-core/test/plugin/manifest.test.ts`，放在 `describe('parseManifest', ...)` 内、文件末尾最后一个 `it` 之后）：

```ts
  it('parses a .kimi-plugin/plugin.json manifest', async () => {
    const root = await makePlugin(
      { '.kimi-plugin/plugin.json': JSON.stringify({ name: 'superpowers', skills: './skills/' }) },
      { dirs: ['skills'] },
    );
    const result = await parseManifest(root);
    expect(result.manifest?.name).toBe('superpowers');
    expect(result.manifest?.skills).toEqual([path.join(root, 'skills')]);
    expect(result.manifestKind).toBe('kimi-plugin-dir');
    expect(result.manifestPath).toBe(path.join(root, '.kimi-plugin', 'plugin.json'));
    expect(result.diagnostics).toEqual([]);
  });

  it('parses a kimi.plugin.json manifest', async () => {
    const root = await makePlugin({
      'kimi.plugin.json': JSON.stringify({ name: 'kimi-demo', version: '1.0.0' }),
    });
    const result = await parseManifest(root);
    expect(result.manifest?.name).toBe('kimi-demo');
    expect(result.manifestKind).toBe('kimi-plugin-root');
  });

  it('prefers a mirri manifest when a kimi manifest also exists', async () => {
    const root = await makePlugin({
      'mirri.plugin.json': JSON.stringify({ name: 'mirri-wins', version: '2.0.0' }),
      '.kimi-plugin/plugin.json': JSON.stringify({ name: 'kimi-loses' }),
    });
    const result = await parseManifest(root);
    expect(result.manifest?.name).toBe('mirri-wins');
    expect(result.manifestKind).toBe('mirri-plugin-root');
    expect(result.shadowedManifestPath).toBeUndefined();
  });

  it('prefers .mirr-plugin/plugin.json over .mirricode-plugin/plugin.json', async () => {
    const root = await makePlugin({
      '.mirr-plugin/plugin.json': JSON.stringify({ name: 'legacy-dir-wins' }),
      '.mirricode-plugin/plugin.json': JSON.stringify({ name: 'named-dir' }),
    });
    const result = await parseManifest(root);
    expect(result.manifest?.name).toBe('legacy-dir-wins');
    expect(result.manifestKind).toBe('mirri-plugin-dir');
  });

  it('prefers kimi.plugin.json over .kimi-plugin/plugin.json', async () => {
    const root = await makePlugin({
      'kimi.plugin.json': JSON.stringify({ name: 'kimi-root-wins' }),
      '.kimi-plugin/plugin.json': JSON.stringify({ name: 'kimi-dir' }),
    });
    const result = await parseManifest(root);
    expect(result.manifest?.name).toBe('kimi-root-wins');
    expect(result.manifestKind).toBe('kimi-plugin-root');
    expect(result.shadowedManifestPath).toBe(path.join(root, '.kimi-plugin', 'plugin.json'));
  });

  it('reports all six manifest candidates when no manifest exists', async () => {
    const root = await makePlugin({});
    const result = await parseManifest(root);
    expect(result.diagnostics).toContainEqual(
      expect.objectContaining({
        severity: 'error',
        message: expect.stringMatching(
          /No manifest at mirri-plugin\.json, mirri\.plugin\.json, \.mirri-plugin\/plugin\.json, \.mirricode-plugin\/plugin\.json, kimi\.plugin\.json, or \.kimi-plugin\/plugin\.json/,
        ),
      }),
    );
  });
```

- [ ] **Step 2: 运行并确认失败**

Run: `pnpm --filter @mirri-ai/agent-core exec vitest run test/plugin/manifest.test.ts`
Expected: 新增 6 条用例失败，报错如 `expect(received).toBe('kimi-plugin-dir')`（当前不识别 kimi 名）。

- [ ] **Step 3: 实现**（改 `packages/agent-core/src/plugin/types.ts:86` 与 `packages/agent-core/src/plugin/manifest.ts`）

先改 `types.ts`，把第 86 行替换为：

```ts
export type PluginManifestKind =
  | 'mirri-plugin-root'
  | 'mirri-plugin-dir'
  | 'kimi-plugin-root'
  | 'kimi-plugin-dir';
```

然后改 `manifest.ts`：把第 19–22 行的常量块替换为：

```ts
const MIRRI_PLUGIN_ROOT_PATH = 'mirri-plugin.json';
const LEGACY_PLUGIN_ROOT_PATH = 'mirri.plugin.json';
const LEGACY_PLUGIN_DIR_PATH = '.mirri-plugin/plugin.json';
const MIRRI_PLUGIN_DIR_PATH = '.mirricode-plugin/plugin.json';
const KIMI_PLUGIN_ROOT_PATH = 'kimi.plugin.json';
const KIMI_PLUGIN_DIR_PATH = '.kimi-plugin/plugin.json';
```

把第 44–82 行（`parseManifest` 开头的发现逻辑）整体替换为：

```ts
export async function parseManifest(pluginRoot: string): Promise<ParsedManifestResult> {
  // Candidates are ordered by the global priority confirmed in the design:
  // root forms before dir forms, Mirri names before kimi compat names.
  const mirrRootPath = path.join(pluginRoot, MIRRI_PLUGIN_ROOT_PATH);
  const legacyRootPath = path.join(pluginRoot, LEGACY_PLUGIN_ROOT_PATH);
  const legacyDirPath = path.join(pluginRoot, LEGACY_PLUGIN_DIR_PATH);
  const mirrDirPath = path.join(pluginRoot, MIRRI_PLUGIN_DIR_PATH);
  const kimiRootPath = path.join(pluginRoot, KIMI_PLUGIN_ROOT_PATH);
  const kimiDirPath = path.join(pluginRoot, KIMI_PLUGIN_DIR_PATH);

  const mirrRootExists = await isFile(mirrRootPath);
  const legacyRootExists = await isFile(legacyRootPath);
  const legacyDirExists = await isFile(legacyDirPath);
  const mirrDirExists = await isFile(mirrDirPath);
  const kimiRootExists = await isFile(kimiRootPath);
  const kimiDirExists = await isFile(kimiDirPath);

  if (
    !mirrRootExists &&
    !legacyRootExists &&
    !legacyDirExists &&
    !mirrDirExists &&
    !kimiRootExists &&
    !kimiDirExists
  ) {
    return {
      diagnostics: [
        {
          severity: 'error',
          message:
            `No manifest at ${MIRRI_PLUGIN_ROOT_PATH}, ${LEGACY_PLUGIN_ROOT_PATH}, ` +
            `${LEGACY_PLUGIN_DIR_PATH}, ${MIRRI_PLUGIN_DIR_PATH}, ${KIMI_PLUGIN_ROOT_PATH}, or ${KIMI_PLUGIN_DIR_PATH}`,
        },
      ],
    };
  }

  const manifestPath = mirrRootExists
    ? mirrRootPath
    : legacyRootExists
      ? legacyRootPath
      : legacyDirExists
        ? legacyDirPath
        : mirrDirExists
          ? mirrDirPath
          : kimiRootExists
            ? kimiRootPath
            : kimiDirPath;
  const manifestKind: PluginManifestKind =
    manifestPath === mirrDirPath || manifestPath === legacyDirPath
      ? 'mirri-plugin-dir'
      : manifestPath === kimiDirPath
        ? 'kimi-plugin-dir'
        : manifestPath === kimiRootPath
          ? 'kimi-plugin-root'
          : 'mirri-plugin-root';
  const shadowedManifestPath =
    mirrRootExists && mirrDirExists
      ? mirrDirPath
      : legacyRootExists && legacyDirExists
        ? legacyDirPath
        : kimiRootExists && kimiDirExists
          ? kimiDirPath
          : undefined;
```

（`parseManifest` 其余部分——JSON 解析、字段读取、返回——保持不变。）

- [ ] **Step 4: 运行测试确认通过**

Run: `pnpm --filter @mirri-ai/agent-core exec vitest run test/plugin/manifest.test.ts`
Expected: PASS（既有用例 + 新增 6 条）。

- [ ] **Step 5: 类型检查**

Run: `pnpm --filter @miri-ai/agent-core run typecheck`
Expected: PASS。

- [ ] **Step 6: 提交**（先暂停询问用户确认，仓库规则：git 操作需确认）

```bash
git add packages/agent-core/src/plugin/manifest.ts packages/agent-core/src/plugin/types.ts packages/agent-core/test/plugin/manifest.test.ts
git commit -m "feat: recognize kimi plugin manifest names in v1 engine"
```

---

### Task 2: v1 zip 根目录探测支持 kimi

**Files:**
- Modify: `packages/agent-core/src/plugin/archive.ts:135-139`（`hasManifest`）
- Test: `packages/agent-core/test/plugin/archive.test.ts`

**Interfaces:**
- 不变：`extractZip(buffer: Buffer, destDir: string): Promise<string>`（返回探测到的插件根目录）。

- [ ] **Step 1: 写失败的测试**（追加到 `describe('extractZip', ...)` 内 `it('detects plugin root with .mirri-plugin/plugin.json', ...)` 用例之后）：

```ts
  it('detects plugin root with .kimi-plugin/plugin.json', async () => {
    const destDir = await mkdtemp(path.join(tmpdir(), 'archive-test-'));
    const zipBuffer = await createZipBuffer([
      { name: 'superpowers/.kimi-plugin/plugin.json', data: '{"name":"superpowers"}' },
      { name: 'superpowers/skills/brainstorming/SKILL.md', data: 'body' },
    ]);

    const root = await extractZip(zipBuffer, destDir);
    expect(root).toBe(path.join(destDir, 'superpowers'));
    const manifest = await readFile(path.join(root, '.kimi-plugin', 'plugin.json'), 'utf8');
    expect(manifest).toBe('{"name":"superpowers"}');
  });
```

- [ ] **Step 2: 运行并确认失败**

Run: `pnpm --filter @mirri-ai/agent-core exec vitest run test/plugin/archive.test.ts`
Expected: 新用例失败，`root` 等于解压根 `destDir` 而不是 `destDir/superpowers`。

- [ ] **Step 3: 实现** — 把 `packages/agent-core/src/plugin/archive.ts` 的 `hasManifest`（第 135–139 行）替换为：

```ts
async function hasManifest(dir: string): Promise<boolean> {
  const rootManifest = path.join(dir, 'mirri-plugin.json');
  const dirManifest = path.join(dir, '.mirricode-plugin', 'plugin.json');
  const kimiRootManifest = path.join(dir, 'kimi.plugin.json');
  const kimiDirManifest = path.join(dir, '.kimi-plugin', 'plugin.json');
  return (
    (await isFile(rootManifest)) ||
    (await isFile(dirManifest)) ||
    (await isFile(kimiRootManifest)) ||
    (await isFile(kimiDirManifest))
  );
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `pnpm --filter @mirri-ai/agent-core exec vitest run test/plugin/archive.test.ts`
Expected: PASS。

- [ ] **Step 5: 提交（需用户确认）**

```bash
git add packages/agent-core/src/plugin/archive.ts packages/agent-core/test/plugin/archive.test.ts
git commit -m "feat: detect kimi plugin root when extracting archives"
```

---

### Task 3: v1 安装链路回归（manager 集成测试）

**Files:**
- Test: `packages/agent-core/test/plugin/manager.test.ts`（实现无需改动——`PluginManager.install` 已串起 `parseManifest` + `extractZip`）

**Interfaces:**
- 不变：`PluginManager({ mirriHomeDir }): install(source): Promise<PluginRecord>`；测试辅助函数 `makeMirriHome()`、`managedPluginRoot(home, id)` 已存在（见文件顶部）。

- [ ] **Step 1: 写测试**（追加到 `it('install() accepts a .mirricode-plugin manifest', ...)` 之后，约第 125 行）：

```ts
  it('install() accepts a .kimi-plugin manifest', async () => {
    const home = await makeMirriHome();
    const root = await mkdtemp(path.join(tmpdir(), 'mirri-plugin-'));
    await mkdir(path.join(root, '.kimi-plugin'), { recursive: true });
    await mkdir(path.join(root, 'skills'), { recursive: true });
    await writeFile(
      path.join(root, '.kimi-plugin', 'plugin.json'),
      JSON.stringify({
        name: 'superpowers',
        skills: './skills/',
        skillInstructions: 'Use Mirri tools.',
      }),
      'utf8',
    );

    const manager = new PluginManager({ mirriHomeDir: home });
    await manager.load();
    const record = await manager.install(root);
    const managedRoot = await managedPluginRoot(home, 'superpowers');

    expect(record.id).toBe('superpowers');
    expect(record.manifestKind).toBe('kimi-plugin-dir');
    expect(record.root).toBe(managedRoot);
    expect(record.originalSource).toBe(root);
    expect(record.manifest?.skills).toEqual([path.join(managedRoot, 'skills')]);
    expect(manager.pluginSkillRoots()).toContainEqual({
      path: path.join(managedRoot, 'skills'),
      source: 'extra',
      plugin: { id: 'superpowers', instructions: 'Use Mirri tools.' },
    });
  });
```

- [ ] **Step 2: 运行确认（Task 1 已实现，本用例应直接通过）**

Run: `pnpm --filter @mirri-ai/agent-core exec vitest run test/plugin/manager.test.ts -t "kimi"`
Expected: PASS——若在 Task 1 之前执行本用例会失败（`record.manifestKind` 为 `undefined`，安装报 "Cannot install plugin"）；该用例的价值是为「本地路径安装 kimi 清单插件」整条链路提供回归保障。

- [ ] **Step 3: 无需实现改动**（`PluginManager.install` 已串起 `parseManifest` + `extractZip`，Task 1 的修复自动生效）

- [ ] **Step 4: 提交（需用户确认）**

```bash
git add packages/agent-core/test/plugin/manager.test.ts
git commit -m "test: cover kimi manifest install via plugin manager"
```

---

### Task 4: v2 引擎同步（manifest + archive）

**Files:**
- Modify: `packages/agent-core-v2/src/app/plugin/manifest.ts:16-82`
- Modify: `packages/agent-core-v2/src/app/plugin/archive.ts:137-140`
- Test: `packages/agent-core-v2/test/app/plugin/manifest.test.ts`、`packages/agent-core-v2/test/app/plugin/archive.test.ts`

**Interfaces:**
- v2 的 `PluginManifestKind`（`agent-core-v2/src/app/plugin/types.ts:86`）已含 `'kimi-plugin-root' | 'kimi-plugin-dir'`，**类型无需改**；v2 的 kind 按「形态」归类（root→`kimi-plugin-root`，dir→`kimi-plugin-dir`），与既有测试一致。

- [ ] **Step 1: 写失败的测试**

追加到 `packages/agent-core-v2/test/app/plugin/manifest.test.ts`（`describe('plugin manifest parser', ...)` 内最后一个 `it` 之后；该文件用 `beforeEach` 生成 `dir` 临时目录，`join` 从 `node:path` 已引入，`writeFile` 已引入）：

```ts
  it('parses a .kimi-plugin/plugin.json manifest', async () => {
    await mkdir(join(dir, '.kimi-plugin'), { recursive: true });
    await writeFile(
      join(dir, '.kimi-plugin', 'plugin.json'),
      JSON.stringify({ name: 'superpowers', skills: './skills/' }),
      'utf8',
    );
    const result = await parseManifest(dir);
    expect(result.manifest?.name).toBe('superpowers');
    expect(result.manifestKind).toBe('kimi-plugin-dir');
    expect(result.diagnostics).toEqual([]);
  });

  it('parses a kimi.plugin.json manifest', async () => {
    await writeFile(
      join(dir, 'kimi.plugin.json'),
      JSON.stringify({ name: 'kimi-demo', version: '1.0.0' }),
      'utf8',
    );
    const result = await parseManifest(dir);
    expect(result.manifest?.name).toBe('kimi-demo');
    expect(result.manifestKind).toBe('kimi-plugin-root');
  });

  it('prefers a mirr manifest when a kimi manifest also exists', async () => {
    await writeFile(join(dir, 'mirri.plugin.json'), JSON.stringify({ name: 'mirri-wins' }), 'utf8');
    await mkdir(join(dir, '.kimi-plugin'), { recursive: true });
    await writeFile(
      join(dir, '.kimi-plugin', 'plugin.json'),
      JSON.stringify({ name: 'kimi-loses' }),
      'utf8',
    );
    const result = await parseManifest(dir);
    expect(result.manifest?.name).toBe('mirri-wins');
    expect(result.shadowedManifestPath).toBeUndefined();
  });
```

追加到 `packages/agent-core-v2/test/app/plugin/archive.test.ts`（现有唯一用例后）：

```ts
  it('detects a nested plugin root with a kimi manifest', async () => {
    const source = join(dir, 'source');
    const nested = join(source, 'plugin');
    await mkdir(join(nested, '.kimi-plugin'), { recursive: true });
    await writeFile(
      join(nested, '.kimi-plugin', 'plugin.json'),
      JSON.stringify({ name: 'kimi-demo' }),
      'utf8',
    );
    const zipPath = join(dir, 'plugin.zip');
    execFileSync('zip', ['-qr', zipPath, '.'], { cwd: source });

    const outDir = join(dir, 'out');
    const detectedRoot = await extractZip(await readFile(zipPath), outDir);

    expect(detectedRoot).toBe(join(outDir, 'plugin'));
    await expect(readFile(join(detectedRoot, '.kimi-plugin', 'plugin.json'), 'utf8')).resolves.toContain(
      'kimi-demo',
    );
  });
```

- [ ] **Step 2: 运行并确认失败**

Run: `pnpm --filter @mirri-ai/agent-core-v2 exec vitest run test/app/plugin/manifest.test.ts test/app/plugin/archive.test.ts`
Expected: 新用例失败。

- [ ] **Step 3: 实现**

`packages/agent-core-v2/src/app/plugin/manifest.ts`：把第 16–17 行常量块替换为：

```ts
const MIRRI_PLUGIN_ROOT_PATH = 'mirri.plugin.json';
const MIRRI_PLUGIN_DIR_PATH = '.mirr-plugin/plugin.json';
const KIMI_PLUGIN_ROOT_PATH = 'kimi.plugin.json';
const KIMI_PLUGIN_DIR_PATH = '.kimi-plugin/plugin.json';
```

把 `parseManifest` 开头的发现逻辑（现第 39–57 行）替换为：

```ts
export async function parseManifest(pluginRoot: string): Promise<ParsedManifestResult> {
  const mirrRootPath = path.join(pluginRoot, MIRRI_PLUGIN_ROOT_PATH);
  const mirrDirPath = path.join(pluginRoot, MIRRI_PLUGIN_DIR_PATH);
  const kimiRootPath = path.join(pluginRoot, KIMI_PLUGIN_ROOT_PATH);
  const kimiDirPath = path.join(pluginRoot, KIMI_PLUGIN_DIR_PATH);
  const mirrRootExists = await isFile(mirrRootPath);
  const mirrDirExists = await isFile(mirrDirPath);
  const kimiRootExists = await isFile(kimiRootPath);
  const kimiDirExists = await isFile(kimiDirPath);

  if (!mirrRootExists && !mirrDirExists && !kimiRootExists && !kimiDirExists) {
    return {
      diagnostics: [
        {
          severity: 'error',
          message:
            `No manifest at ${MIRRI_PLUGIN_ROOT_PATH}, ${MIRRI_PLUGIN_DIR_PATH}, ` +
            `${KIMI_PLUGIN_ROOT_PATH}, or ${KIMI_PLUGIN_DIR_PATH}`,
        },
      ],
    };
  }

  const manifestPath = mirrRootExists
    ? mirrRootPath
    : mirrDirExists
      ? mirrDirPath
      : kimiRootExists
        ? kimiRootPath
        : kimiDirPath;
  const manifestKind: PluginManifestKind =
    manifestPath === mirrDirPath || manifestPath === kimiDirPath
      ? 'kimi-plugin-dir'
      : 'kimi-plugin-root';
  const shadowedManifestPath =
    mirrRootExists && mirrDirExists
      ? mirrDirPath
      : kimiRootExists && kimiDirExists
        ? kimiDirPath
        : undefined;
```

（其余部分不变。）

`packages/agent-core-v2/src/app/plugin/archive.ts`：把 `hasManifest`（现第 136–140 行）替换为：

```ts
async function hasManifest(dir: string): Promise<boolean> {
  const rootManifest = path.join(dir, 'mirri.plugin.json');
  const dirManifest = path.join(dir, '.mirri-plugin', 'plugin.json');
  const kimiRootManifest = path.join(dir, 'kimi.plugin.json');
  const kimiDirManifest = path.join(dir, '.kimi-plugin', 'plugin.json');
  return (
    (await isFile(rootManifest)) ||
    (await isFile(dirManifest)) ||
    (await isFile(kimiRootManifest)) ||
    (await isFile(kimiDirManifest))
  );
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `pnpm --filter @mirri-ai/agent-core-v2 exec vitest run test/app/plugin/manifest.test.ts test/app/plugin/archive.test.ts`
Expected: PASS。

- [ ] **Step 5: typecheck**

Run: `pnpm --filter @mirri-ai/agent-core-v2 run typecheck`
Expected: PASS。

- [ ] **Step 6: 提交（需用户确认）**

```bash
git add packages/agent-core-v2/src/app/plugin/manifest.ts packages/agent-core-v2/src/app/plugin/archive.ts packages/agent-core-v2/test/app/plugin/manifest.test.ts packages/agent-core-v2/test/app/plugin/archive.test.ts
git commit -m "feat: recognize kimi plugin manifest names in v2 engine"
```

---

### Task 5: 发布脚本顺序对齐

**Files:**
- Modify: `apps/mirri-code/scripts/plugin-manifest-version.mjs`

**Interfaces:**
- 不变：`readPluginManifestVersion(pluginDir): Promise<string | undefined>`。

- [ ] **Step 1: 改实现** — 把第 4–9 行的注释与 `for...of` 列表替换为：

```js
// Read a local plugin directory's declared version from its manifest, mirroring
// the plugin loader's precedence (packages/agent-core/src/plugin/manifest.ts):
// root files before dir files, Mirri names before kimi compat names, as listed
// below. Returns undefined when no manifest is present or the chosen manifest
// has no version - callers then leave the marketplace entry's existing version
// untouched.
export async function readPluginManifestVersion(pluginDir) {
  for (const rel of [
    'mirri-plugin.json',
    'mirri.plugin.json',
    '.mirri-plugin/plugin.json',
    '.mirricode-plugin/plugin.json',
    'kimi.plugin.json',
    '.kimi-plugin/plugin.json',
  ]) {
    const raw = await readFileOrUndefined(resolve(pluginDir, rel));
    if (raw === undefined) continue; // manifest absent - fall back to the next candidate
    return versionFromManifest(raw); // the chosen manifest wins, even if it has no version
  }
  return undefined;
}
```

- [ ] **Step 2: 手工验证**

Run:
```bash
node --input-type=module -e "import('./apps/mirri-code/scripts/plugin-manifest-version.mjs').then(async (m) => { const fsp = await import('node:fs/promises'); const os = await import('node:os'); const path = await import('node:path'); const dir = await fsp.mkdtemp(path.join(os.tmpdir(), 'plugin-ver-')); await fsp.mkdir(path.join(dir, '.kimi-plugin'), { recursive: true }); await fsp.writeFile(path.join(dir, '.kimi-plugin', 'plugin.json'), JSON.stringify({ name: 'demo', version: '6.2.0' })); console.log(await m.readPluginManifestVersion(dir)); await fsp.rm(dir, { recursive: true, force: true }); })"
```
Expected: 输出 `6.2.0`（`node --input-type=module` 使顶层 `import()` 在 ESM 语义下可解析）。

- [ ] **Step 3: 提交（需用户确认）**

```bash
git add apps/mirri-code/scripts/plugin-manifest-version.mjs
git commit -m "fix: align marketplace version probe with kimi manifest support"
```

---

### Task 6: 用户文档更新（zh/en）

**Files:**
- Modify: `docs/zh/customization/plugins.md:122-129`
- Modify: `docs/en/customization/plugins.md:122-129`

- [ ] **Step 1: 改 zh 文档** — 把第 122–129 行替换为（注意保留原文件的行内代码风格）：

```markdown
    Plugin 是一个带 manifest 的目录或 zip 文件。Manifest 可以放在以下任一位置（按优先级从高到低）：

    ```text
    <plugin_root>/mirri-plugin.json
    <plugin_root>/mirri.plugin.json
    <plugin_root>/.mirri-plugin/plugin.json
    <plugin_root>/.mirricode-plugin/plugin.json
    <plugin_root>/kimi.plugin.json
    <plugin_root>/.kimi-plugin/plugin.json
    ```

    规则：

    - root 形态（单个 JSON 文件）优先于目录形态（`.mirricode-plugin/` 等目录中的 `plugin.json`）。
    - 同一插件存在多份清单时，采用优先级最高的一份。
    - `kimi.plugin.json` / `.kimi-plugin/plugin.json` 是 kimi 生态的兼容命名：仅当上述 Mirri 命名都不存在时才作为兜底解析，用于安装只携带 kimi 清单的插件（如 superpowers）。
```

- [ ] **Step 2: 改 en 文档** — 把第 122–129 行替换为：

```markdown
    A plugin is a directory or zip file containing a manifest. The manifest can be placed at any of the following locations (in descending priority):

    ```text
    <plugin_root>/mirri-plugin.json
    <plugin_root>/mirri.plugin.json
    <plugin_root>/.mirri-plugin/plugin.json
    <plugin_root>/.mirricode-plugin/plugin.json
    <plugin_root>/kimi.plugin.json
    <plugin_root>/.kimi-plugin/plugin.json
    ```

    Rules:

    - Root files (a single JSON file) take precedence over directory manifests (`plugin.json` inside `.mirricode-plugin/`, etc.).
    - When several manifests exist, the highest-priority one wins.
    - `kimi.plugin.json` / `.kimi-plugin/plugin.json` are kimi ecosystem compat names: they are only parsed as a fallback when none of the Mirri names exist, enabling installation of plugins that ship kimi manifests only (e.g. superpowers).
```

- [ ] **Step 3: 检查生成文档站点构建不受影响（如本地有 docs 构建依赖可跳过）**

Run（仓库根目录，若 docs 依赖已装）: `pnpm --filter docs run build`（可选；构建失败不阻塞本任务，只需确认 Markdown 语法无误）。

- [ ] **Step 4: 提交（需用户确认）**

```bash
git add docs/zh/customization/plugins.md docs/en/customization/plugins.md
git commit -m "docs: document kimi manifest fallback for plugins"
```

---

## 完整验证（提交全部完成后）

```bash
pnpm --filter @mirri-ai/agent-core run typecheck
pnpm --filter @mirri-ai/agent-core-v2 run typecheck
pnpm --filter @mirri-ai/agent-core exec vitest run test/plugin/manifest.test.ts test/plugin/archive.test.ts test/plugin/manager.test.ts
pnpm --filter @mirri-ai/agent-core-v2 exec vitest run test/app/plugin/manifest.test.ts test/app/plugin/archive.test.ts
```

全部应为 PASS；随后（可选，需网络）手工场景：`/plugins install https://github.com/obra/superpowers`，确认成功且 `getPluginInfo` 的 `manifestKind` 为 `kimi-plugin-dir`、`manifestPath` 指向 `.kimi-plugin/plugin.json`。