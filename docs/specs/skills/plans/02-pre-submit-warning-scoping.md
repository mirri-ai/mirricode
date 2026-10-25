# 发送前警示按芯片来源收窄（pre-submit warning scoping）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 消除发送前警示对粘贴文本的误报——警示与移除只作用于输入框插入的 `skillMention` 芯片节点，纯文本（含 `[$xxx]` 形态的日志/代码）永不触发警示，旧草稿芯片载入时按名回填目标。

**Architecture:** 三层改动：纯函数层（`skillMention.ts` 新增 doc-JSON 芯片遍历 helper + `inlineReferences.ts` 新增节点属性列表校验入口，退役文本扫描入口）→ Composer 接线层（警示判定、警示条数据源、移除 issue 集全部换轨为节点入口）→ 草稿回填层（`buildSkillMentionDocJson` 增加按名回填 target 的变体）。wire 格式、服务端激活判定、rust core 零改动。

**Tech Stack:** Vue 3 + TipTap（ProseMirror）、vitest（web 测试基建不挂载 Vue 组件——全部测试以纯函数为对象）。

**Spec:** 需求 [../skills-requirements.md](../skills-requirements.md) SDR-007 / US3.6；架构 [../skills-architecture.md](../skills-architecture.md) ADR-009。

## Global Constraints

- 只动 `apps/mirri-web`；`@mirri-ai/protocol`、`packages/*`、`rust/*` 零改动。
- 命令芯片（`kind: 'command'`）永不进入警示（既有语义，`Composer.vue:412` 注释「Command references never block」）。
- 提交序列化 `normalizeInlineReferencesForSubmit` 保留原样——粘贴/手打同形 token 的展开激活是 ADR-004 已接受残余（ADR-009 决策 2 显式登记），不动。
- `findInvalidInlineReferences` / `removeInvalidInlineReferences`（文本扫描入口）迁移后 src 无调用方：前者删除并收敛其测试，后者整体删除（含测试）。
- 测试命名 Given-When-Then；加进既有测试文件（`apps/mirri-web/test/inline-references.test.ts`、`apps/mirri-web/test/skill-mention.test.ts`），不新建测试文件；TDD 先红后绿。
- `props.skills` 的元素类型为 `AppSkill`（`apps/mirri-web/src/api/types.ts:1099`：`{ name, description, source, path?, dir?, enabled? }`）。
- 验证命令：`pnpm --filter mirri-web run typecheck`、`pnpm --filter mirri-web run test`（若 filter 名不同以 `apps/mirri-web/package.json` name 字段为准，进目录跑 `pnpm run test` 亦可）。

---

### Task 1: 纯函数层 — doc JSON 芯片遍历 helper + 节点校验入口

**Files:**
- Modify: `apps/mirri-web/src/lib/skillMention.ts`（新增 `collectSkillMentionAttrs` + `backfillSkillMentionTargets`，后者 Task 3 使用）
- Modify: `apps/mirri-web/src/lib/inlineReferences.ts`（新增 `findInvalidSkillMentionAttrs`，提取共享比对 helper）
- Test: `apps/mirri-web/test/inline-references.test.ts`（新增 describe 块）
- Test: `apps/mirri-web/test/skill-mention.test.ts`（`collectSkillMentionAttrs` 用例）

**Interfaces:**
- Produces（Task 2/3 消费）:
  - `collectSkillMentionAttrs(doc: DocJson): SkillMentionAttrs[]`（`skillMention.ts`，深度遍历收集全部 skillMention 节点属性，文档序）
  - `findInvalidSkillMentionAttrs(attrs: ReadonlyArray<SkillMentionAttrs>, skills: ReadonlyArray<AppSkill>): Array<{ attrs: SkillMentionAttrs; reason: 'missing' | 'disabled' }>`（`inlineReferences.ts`）
  - `backfillSkillMentionTargets(doc: DocJson, skills: ReadonlyArray<{ name: string; path?: string }>): DocJson`（Task 3 实现并使用）
- 注意：`skillMention.ts` 不 import `AppSkill`（api 层类型）——参数类型用结构化 `{ name: string; path?: string; enabled?: boolean }`，与 `inlineReferences.ts` 既有函数的入参风格一致。

- [ ] **Step 1: 写失败测试（`skill-mention.test.ts`，`collectSkillMentionAttrs`）**

在 `apps/mirri-web/test/skill-mention.test.ts` 顶部 import 区加入 `collectSkillMentionAttrs`，文件末尾追加：

```ts
describe('collectSkillMentionAttrs — doc JSON chip collection (ADR-009)', () => {
  it('should return an empty list when the doc contains only plain text', () => {
    const doc = buildSkillMentionDocJson('paste log [$envs[$this->environment]] here');
    expect(collectSkillMentionAttrs(doc)).toEqual([]);
  });

  it('should collect every skillMention node attrs in document order', () => {
    const linkA = formatSkillReferenceLink('alpha', SKILL_A);
    const doc = buildSkillMentionDocJson(`use ${linkA}, then run [$model](command:/model)`);
    expect(collectSkillMentionAttrs(doc)).toEqual([
      { display: 'alpha', target: SKILL_A, kind: 'skill' },
      { display: 'model', target: 'command:/model', kind: 'command' },
    ]);
  });

  it('should include targetless nodes (legacy compact-token drafts)', () => {
    const doc = buildSkillMentionDocJson('go [$alpha] now');
    expect(collectSkillMentionAttrs(doc)).toEqual([
      { display: 'alpha', target: '', kind: 'skill' },
    ]);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd apps/mirri-web && pnpm vitest run test/skill-mention.test.ts`
Expected: FAIL — `collectSkillMentionAttrs` 未导出（import 报错或 undefined）。

- [ ] **Step 3: 实现 `collectSkillMentionAttrs`（`skillMention.ts`）**

在 `apps/mirri-web/src/lib/skillMention.ts` 的 `buildMentionNodeJson` 之后追加：

```ts
/**
 * Deep-collect every skillMention node's attrs from a doc JSON, in document
 * order (ADR-009): the pre-submit warning validates ONLY these structural
 * chips — plain text (pasted logs/code with `[$var]`-shaped content) never
 * enters the warning path.
 */
export function collectSkillMentionAttrs(doc: DocJson): SkillMentionAttrs[] {
  const attrs: SkillMentionAttrs[] = [];
  const walk = (node: DocJson): void => {
    if (node.type === 'skillMention') {
      const a = (node.attrs ?? {}) as Partial<SkillMentionAttrs>;
      attrs.push({ display: a.display ?? '', target: a.target ?? '', kind: a.kind ?? 'skill' });
      return; // atom node — no children
    }
    for (const child of node.content ?? []) walk(child);
  };
  walk(doc);
  return attrs;
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd apps/mirri-web && pnpm vitest run test/skill-mention.test.ts`
Expected: PASS（3 个新用例 + 既有用例全绿）。

- [ ] **Step 5: 写失败测试（`inline-references.test.ts`，`findInvalidSkillMentionAttrs`）**

在 `apps/mirri-web/test/inline-references.test.ts` 的 import 区加入 `findInvalidSkillMentionAttrs`，文件末尾追加（SKILLS 常量文件头已定义）：

```ts
// ---------------------------------------------------------------------------
// ADR-009 — node-attrs validation (pre-submit warning scoping)
// ---------------------------------------------------------------------------

describe('findInvalidSkillMentionAttrs — chip-attrs validation (ADR-009, SDR-007)', () => {
  it('should return no issues when the list is empty even though the pasted text looks like references', () => {
    // Given a message whose TEXT contains PHP-style array indexing but no chips,
    // When validating the collected chip attrs (empty list),
    // Then nothing is flagged — plain text never enters the warning path.
    expect(findInvalidSkillMentionAttrs([], SKILLS)).toEqual([]);
  });

  it('should flag a skill chip whose target is absent from the skill list', () => {
    const attrs = [{ display: 'gone', target: '/Users/example/skills/gone/SKILL.md', kind: 'skill' as const }];
    expect(findInvalidSkillMentionAttrs(attrs, SKILLS)).toEqual([
      { attrs: attrs[0], reason: 'missing' },
    ]);
  });

  it('should flag a skill chip whose target hits a disabled skill', () => {
    const attrs = [{ display: 'gamma', target: '/Users/example/skills/gamma/SKILL.md', kind: 'skill' as const }];
    // gamma 在 SKILLS 中无 path，展开补上 path 以命中 pathMap 的 disabled 分支
    expect(findInvalidSkillMentionAttrs(attrs, [{ ...SKILLS[2], path: '/Users/example/skills/gamma/SKILL.md' }])).toEqual([
      { attrs: attrs[0], reason: 'disabled' },
    ]);
  });

  it('should never flag command chips even when their target is unresolvable', () => {
    const attrs = [{ display: 'whatever', target: 'command:/nope', kind: 'command' as const }];
    expect(findInvalidSkillMentionAttrs(attrs, SKILLS)).toEqual([]);
  });

  it('should resolve a targetless chip by name and flag it only when the name misses', () => {
    // Given a legacy compact-token node (targetless) whose name hits the list,
    // When validating, Then it is valid.
    expect(
      findInvalidSkillMentionAttrs([{ display: 'alpha', target: '', kind: 'skill' }], SKILLS),
    ).toEqual([]);
    // Given a targetless node whose name misses every skill,
    // When validating, Then it is flagged missing.
    expect(
      findInvalidSkillMentionAttrs([{ display: 'nope', target: '', kind: 'skill' }], SKILLS),
    ).toEqual([{ attrs: { display: 'nope', target: '', kind: 'skill' }, reason: 'missing' }]);
  });

  it('should skip validation entirely when no skill carries a path (older backends)', () => {
    const pathless = [{ name: 'alpha' }, { name: 'beta' }];
    const attrs = [{ display: 'alpha', target: '/anything/SKILL.md', kind: 'skill' as const }];
    expect(findInvalidSkillMentionAttrs(attrs, pathless)).toEqual([]);
  });
});
```

- [ ] **Step 6: 跑测试确认失败**

Run: `cd apps/mirri-web && pnpm vitest run test/inline-references.test.ts`
Expected: FAIL — `findInvalidSkillMentionAttrs` 未导出。

- [ ] **Step 7: 实现 `findInvalidSkillMentionAttrs`（`inlineReferences.ts`）**

在 `apps/mirri-web/src/lib/inlineReferences.ts` 的 `findInvalidInlineReferences` 之前，先提取共享比对 helper（把既有函数体的 pathMap/nameMap 构建逻辑提为私有函数），再新增节点校验入口：

```ts
/** Build the lookup maps shared by the text-scan and node-attrs validators. */
function buildSkillLookupMaps(skills: ReadonlyArray<{ name: string; path?: string; enabled?: boolean }>): {
  pathMap: Map<string, { enabled?: boolean }>;
  nameMap: Map<string, { enabled?: boolean }>;
  hasPaths: boolean;
} {
  const pathMap = new Map<string, { enabled?: boolean }>();
  const nameMap = new Map<string, { enabled?: boolean }>();
  let hasPaths = false;
  for (const skill of skills) {
    if (skill.path) {
      hasPaths = true;
      pathMap.set(skill.path, { enabled: skill.enabled });
    }
    nameMap.set(skill.name.toLowerCase(), { enabled: skill.enabled });
  }
  return { pathMap, nameMap, hasPaths };
}

/**
 * Validate structural chip attrs (ADR-009): the pre-submit warning checks
 * ONLY the chips the user inserted (skillMention nodes) — pasted or typed
 * text never enters the warning path. Command chips never block (established
 * semantics); a targetless chip (legacy compact-token draft) resolves by
 * NAME; a chipped target resolves by path. Backends reporting no path on any
 * skill disable path checking entirely.
 */
export function findInvalidSkillMentionAttrs(
  attrs: ReadonlyArray<{ display: string; target: string; kind: 'skill' | 'command' }>,
  skills: ReadonlyArray<{ name: string; path?: string; enabled?: boolean }>,
): Array<{ attrs: { display: string; target: string; kind: 'skill' | 'command' }; reason: InvalidReferenceReason }> {
  const { pathMap, nameMap, hasPaths } = buildSkillLookupMaps(skills);
  if (!hasPaths) return []; // path authority unavailable — nothing to check
  const issues: Array<{ attrs: { display: string; target: string; kind: 'skill' | 'command' }; reason: InvalidReferenceReason }> = [];
  for (const a of attrs) {
    if (a.kind === 'command') continue; // commands never block the send
    const hit = a.target ? pathMap.get(a.target) : nameMap.get(a.display.toLowerCase());
    if (hit === undefined) issues.push({ attrs: a, reason: 'missing' });
    else if (hit.enabled === false) issues.push({ attrs: a, reason: 'disabled' });
  }
  return issues;
}
```

同时把既有 `findInvalidInlineReferences` 函数体里的 map 构建段替换为 `const { pathMap, nameMap, hasPaths } = buildSkillLookupMaps(skills);`（行为不变，消除重复）。

- [ ] **Step 8: 跑测试确认通过**

Run: `cd apps/mirri-web && pnpm vitest run test/inline-references.test.ts`
Expected: PASS（新用例 + 既有 `findInvalidInlineReferences` 用例全绿——本步只提取共享，不改文本扫描行为）。

- [ ] **Step 9: 提交**

```bash
git add apps/mirri-web/src/lib/skillMention.ts apps/mirri-web/src/lib/inlineReferences.ts apps/mirri-web/test/skill-mention.test.ts apps/mirri-web/test/inline-references.test.ts
git commit -m "feat(web): node-attrs validation entry for pre-submit warning scoping"
```

---

### Task 2: Composer 接线 — 警示判定、警示条数据源、移除全部换轨为节点入口

**Files:**
- Modify: `apps/mirri-web/src/components/chat/Composer.vue:408-459`（警示区块）
- Modify: `apps/mirri-web/src/components/chat/Composer.vue:141-145`（`serializeDoc` 旁新增 `collectDocChipAttrs`）
- Modify: `apps/mirri-web/src/lib/inlineReferences.ts`（删除 `findInvalidInlineReferences` / `removeInvalidInlineReferences`）
- Test: `apps/mirri-web/test/inline-references.test.ts`（收敛退役入口的用例）

**Interfaces:**
- Consumes: `collectSkillMentionAttrs`、`findInvalidSkillMentionAttrs`（Task 1）
- Produces: Composer 内部函数（不外露）。

- [ ] **Step 1: 确认基线绿**

Task 2 是纯接线任务（web 测试基建不挂载 Vue 组件——Tech Stack 已声明），无新单测；依赖 typecheck + Task 1/3 的纯函数测试 + Task 4 手动验收兜底。先跑基线确认现状全绿：

Run: `cd apps/mirri-web && pnpm vitest run test/inline-references.test.ts`
Expected: PASS（基线绿，无改动失败）。

- [ ] **Step 2: Composer 换轨**

在 `apps/mirri-web/src/components/chat/Composer.vue`：

1. import 区（第 18 行附近）把 `findInvalidInlineReferences` 的导入换成 `findInvalidSkillMentionAttrs`（`inlineReferences` 模块），并在 `skillMention` 的导入里加入 `collectSkillMentionAttrs`。
2. `serializeDoc` 函数之后（约 146 行）新增：

```ts
/** The structural chips in the doc (ADR-009): the warning path validates
 *  ONLY these — pasted text never enters the pre-submit warning. */
function collectDocChipAttrs(): SkillMentionAttrs[] {
  const e = editor.value;
  if (!e) return [];
  return collectSkillMentionAttrs(e.getJSON());
}
```

（`SkillMentionAttrs` 类型从 `skillMention` 模块 import。）

3. 警示区块（415-436 行）替换为：

```ts
const invalidRefWarning = ref(false);
const invalidRefs = computed(() =>
  invalidRefWarning.value ? findInvalidSkillMentionAttrs(collectDocChipAttrs(), props.skills) : [],
);
/** Set by "send anyway": skip the inline-reference gate for one submission. */
let sendDespiteInvalidRefs = false;

function checkInlineReferencesBeforeSubmit(): boolean {
  if (sendDespiteInvalidRefs) {
    sendDespiteInvalidRefs = false;
    invalidRefWarning.value = false;
    return true;
  }
  const issues = findInvalidSkillMentionAttrs(collectDocChipAttrs(), props.skills);
  if (issues.length === 0) {
    invalidRefWarning.value = false;
    return true;
  }
  // Show the warning bar; the caller must NOT submit yet.
  invalidRefWarning.value = true;
  return false;
}
```

4. `removeInvalidRefs`（438-459 行）替换为——**两阶段删除**（先收集区间再按位置降序删，修复既有实现「遍历中边删边漂移」的位置错位隐患）：

```ts
function removeInvalidRefs(): void {
  const e = editor.value;
  if (!e) return;
  // Delete every skillMention node whose attrs failed validation (ADR-009:
  // removal can only touch real chips — pasted text has no deletion path).
  // Two-phase: collect ranges first, then delete in DESCENDING position order
  // so earlier deletions cannot shift later nodes' positions.
  const issues = new Set(
    findInvalidSkillMentionAttrs(collectDocChipAttrs(), props.skills).map(
      (i) => `${i.attrs.display}\u0000${i.attrs.target}`,
    ),
  );
  const ranges: Array<{ from: number; to: number }> = [];
  e.state.doc.descendants((node, pos) => {
    if (node.type.name !== 'skillMention') return;
    const { display, target } = node.attrs as { display: string; target: string };
    if (issues.has(`${display}\u0000${target}`)) {
      ranges.push({ from: pos, to: pos + node.nodeSize });
    }
  });
  ranges.sort((a, b) => b.from - a.from);
  for (const range of ranges) {
    e.chain().focus().deleteRange(range).run();
  }
  invalidRefWarning.value = false;
  syncDraftFromEditor();
  void nextTick(() => {
    editor.value?.commands.focus();
  });
}
```

5. 警示条模板中 `issue.reference.display` 改为 `issue.attrs.display`——仅 1110 行一处；`issue.reason` 键名新旧一致，无需改动。
6. 区块头注释（409-413 行）更新为 ADR-009 语义：「pre-submit validation (ADR-009): chips the user inserted are validated against the live session skill list — pasted text never enters the warning path…」。

- [ ] **Step 3: 删除退役入口（`inlineReferences.ts`）**

删除 `findInvalidInlineReferences` 与 `removeInvalidInlineReferences` 两个导出函数，以及失去引用方的 `InvalidInlineReference` 接口（164-169 行；`InlineReference` 类型 import 仍被 `extractInlineReferenceSpan` 使用，保留）。

同步从 `apps/mirri-web/test/inline-references.test.ts` 删除：
- import 区的 `findInvalidInlineReferences`、`removeInvalidInlineReferences` 与 `parseInlineReferences`（protocol import——仅被删除的用例使用，删除后成孤儿）；
- 两个整段 describe（`findInvalidInlineReferences …` 与 `removeInvalidInlineReferences …`）；
- `compact tokens — parse / expand / validate` describe 内部 4 个直接调用被删函数的 `it` 用例（missing token / disabled token / valid token / remove token 四条）。

保留其余全部用例——`parseCompactTokens`、`expandCompactTokens`、`splitMessageInlineReferences`、`hasInlineReferences` 等渲染/提交路径用例不动。

- [ ] **Step 4: typecheck + 全量测试**

Run: `cd apps/mirri-web && pnpm run typecheck && pnpm vitest run`
Expected: typecheck 零错误；测试全绿（确认 src 无残留引用退役函数——`grep -rn "findInvalidInlineReferences\|removeInvalidInlineReferences" apps/mirri-web/src` 应为空）。

- [ ] **Step 5: 提交**

```bash
git add apps/mirri-web/src/components/chat/Composer.vue apps/mirri-web/src/lib/inlineReferences.ts apps/mirri-web/test/inline-references.test.ts
git commit -m "fix(web): pre-submit warning validates only inserted chips, not pasted text"
```

---

### Task 3: 草稿回填 — 旧紧凑 token 芯片载入时按名回填目标

**Files:**
- Modify: `apps/mirri-web/src/lib/skillMention.ts`（新增 `backfillSkillMentionTargets`）
- Modify: `apps/mirri-web/src/components/chat/Composer.vue:148-153`（`applyTextToEditor` 接线回填）
- Test: `apps/mirri-web/test/skill-mention.test.ts`

**Interfaces:**
- Consumes: `DocJson`、`SkillMentionAttrs`（`skillMention.ts` 既有类型）。
- Produces: `backfillSkillMentionTargets(doc: DocJson, skills: ReadonlyArray<{ name: string; path?: string }>): DocJson`。

- [ ] **Step 1: 写失败测试（`skill-mention.test.ts`）**

文件末尾追加：

```ts
describe('backfillSkillMentionTargets — legacy compact-token chips (ADR-009 D2)', () => {
  it('should backfill a targetless chip whose name hits the skill list', () => {
    // Given a legacy draft chip `[$alpha]` and a live list carrying alpha's path,
    // When backfilling, Then the node gains the target and serializes as a full link.
    const doc = backfillSkillMentionTargets(buildSkillMentionDocJson('go [$alpha] now'), [
      { name: 'alpha', path: SKILL_A },
    ]);
    expect(collectSkillMentionAttrs(doc)).toEqual([
      { display: 'alpha', target: SKILL_A, kind: 'skill' },
    ]);
    expect(serializeDocJsonToText(doc)).toBe(`go ${formatSkillReferenceLink('alpha', SKILL_A)} now`);
  });

  it('should leave a targetless chip untouched when the name misses every skill', () => {
    // Given a chip whose name is gone from the list,
    // When backfilling, Then it stays targetless (the node gate flags it missing).
    const doc = backfillSkillMentionTargets(buildSkillMentionDocJson('go [$ghost] now'), [
      { name: 'alpha', path: SKILL_A },
    ]);
    expect(collectSkillMentionAttrs(doc)).toEqual([{ display: 'ghost', target: '', kind: 'skill' }]);
  });

  it('should not touch chips that already carry a target', () => {
    const link = formatSkillReferenceLink('alpha', SKILL_A);
    const doc = backfillSkillMentionTargets(buildSkillMentionDocJson(`use ${link}`), [
      { name: 'alpha', path: '/different/path/SKILL.md' },
    ]);
    expect(collectSkillMentionAttrs(doc)).toEqual([
      { display: 'alpha', target: SKILL_A, kind: 'skill' },
    ]);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd apps/mirri-web && pnpm vitest run test/skill-mention.test.ts`
Expected: FAIL — `backfillSkillMentionTargets` 未导出。

- [ ] **Step 3: 实现 `backfillSkillMentionTargets`（`skillMention.ts`）**

在 `collectSkillMentionAttrs` 之后追加：

```ts
/**
 * Backfill targets of targetless skillMention nodes from the live skill list
 * by NAME (ADR-009 D2): legacy compact-token drafts load as targetless nodes;
 * a resolvable name restores the chip to a full link on serialization, an
 * unresolvable one stays targetless and the node gate flags it missing.
 * Chips that already carry a target are never touched.
 */
export function backfillSkillMentionTargets(
  doc: DocJson,
  skills: ReadonlyArray<{ name: string; path?: string }>,
): DocJson {
  const byName = new Map(skills.map((s) => [s.name.toLowerCase(), s]));
  const walk = (node: DocJson): DocJson => {
    if (node.type === 'skillMention') {
      const a = (node.attrs ?? {}) as Partial<SkillMentionAttrs>;
      if (a.kind !== 'command' && !a.target && a.display) {
        const hit = byName.get(a.display.toLowerCase());
        if (hit?.path !== undefined) {
          return { ...node, attrs: { ...a, display: a.display, target: hit.path, kind: a.kind ?? 'skill' } };
        }
      }
      return node;
    }
    if (!node.content) return node;
    return { ...node, content: node.content.map(walk) };
  };
  return walk(doc);
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd apps/mirri-web && pnpm vitest run test/skill-mention.test.ts`
Expected: PASS。

- [ ] **Step 5: Composer 接线（`applyTextToEditor`）**

`apps/mirri-web/src/components/chat/Composer.vue` 第 148-153 行替换为：

```ts
function applyTextToEditor(value: string): void {
  const e = editor.value;
  if (!e) return;
  e.commands.setContent(backfillSkillMentionTargets(buildSkillMentionDocJson(value), props.skills));
  e.commands.focus('end');
}
```

（import 区补 `backfillSkillMentionTargets`。）

**初始挂载回填（修复刷新窗口）**：`useEditor` 初始 content（207 行）在 skills 未就绪时同步回填不可行，但刷新后旧紧凑 token 芯片会因用户首键触发草稿保存被降级重写为纯文本（既有破坏性行为）。补一次性延迟回填：在 `applyTextToEditor` 之后新增

```ts
// One-shot backfill when the skill list first arrives (ADR-009 D2): the
// initial mount cannot backfill (skills not ready); without this, a legacy
// compact-token chip survives only until the first keystroke rewrites the
// draft as plain text.
const skillsArrived = ref(false);
watch(
  () => props.skills,
  (skills) => {
    if (skillsArrived.value || skills.length === 0) return;
    skillsArrived.value = true;
    const e = editor.value;
    if (!e) return;
    e.commands.setContent(backfillSkillMentionTargets(e.getJSON(), skills));
  },
  { immediate: true },
);
```

- [ ] **Step 6: typecheck + 全量测试**

Run: `cd apps/mirri-web && pnpm run typecheck && pnpm vitest run`
Expected: 全绿。

- [ ] **Step 7: 提交**

```bash
git add apps/mirri-web/src/lib/skillMention.ts apps/mirri-web/src/components/chat/Composer.vue apps/mirri-web/test/skill-mention.test.ts
git commit -m "feat(web): backfill legacy compact-token chips by name on draft load"
```

---

### Task 4: 收尾 — changeset + 手动验收

**Files:**
- Create: `.changeset/xxx.md`（gen-changesets skill 生成）

**Interfaces:** 无代码接口。

- [ ] **Step 1: 生成 changeset**

调用 `gen-changesets` skill：锚 `@mirri-ai/release`，`patch` 级（用户可感知 bug 修复），英文 changelog 条目描述「web composer no longer flags pasted log/code text as invalid skill references」。

- [ ] **Step 2: 手动验收（浏览器，`pnpm dev:v2` 后端）**

验收清单：
1. 粘贴含 `[$envs[$this->environment]]` 的 PHP 报错日志发送 → 无警示条、直接发送、消息原文完整。
2. `$` 菜单插入一个芯片 → 停用该技能（设置页）→ 发送 → 警示条出现（列出该芯片 + 原因）、发送被拦一次。
3. 警示条点「移除失效引用」→ 仅芯片被删，其余文本原样保留。
4. 失效芯片 + 粘贴同形文本混排 → 警示条只列芯片、不列文本。
5. （草稿回填）`localStorage` 中注入含 `[$alpha]` 的旧草稿 → 刷新页面 → 芯片正常显示且发送后激活（服务端日志确认 `user-inline` 激活）——覆盖一次性 skills 就绪回填路径。

- [ ] **Step 3: 提交 changeset**

```bash
git add .changeset/
git commit -m "chore: changeset for pre-submit warning scoping fix"
```

---

## Parallelization

Task 1 → Task 2 → Task 3 串行（接口依赖：Task 2/3 消费 Task 1 的产物；Task 3 修改的 `applyTextToEditor` 在 Task 2 同一文件附近）。Task 4 依赖前三个全部完成。无并行批次。

## Self-Review 结论

- **Spec 覆盖**：ADR-009 决策 1（节点校验）→ Task 1+2；决策 2（紧凑 token 退役 + 草稿回填）→ Task 3（含初始挂载的一次性 skills 就绪回填，堵住刷新窗口的草稿降级）；SDR-007「移除永不触碰普通文本」→ Task 2 `removeInvalidRefs` 两阶段节点删除。`with_skills` 回放路径不受影响（无任务，符合 ADR「不受影响」断言）。
- **已知残余（显式登记）**：`invalidRefs` computed 对编辑器内容不具响应性（警示条挂起期间用户编辑不刷新列表）——与现状同构且每次提交前有 `checkInlineReferencesBeforeSubmit` 重新判定兜底；如需改进，接入既有的 `docVersion` ref 即可（optional improvement，非本计划必改项）。
- **占位符扫描**：无 TBD/TODO/略写步骤；所有代码块完整可执行。
- **类型一致性**：`SkillMentionAttrs`（`skillMention.ts` 既有导出）、`InvalidReferenceReason`（`inlineReferences.ts` 既有导出）跨任务一致；`findInvalidSkillMentionAttrs` 入参用结构化类型而非 `AppSkill`（避免 lib 层反向依赖 api 层）。
