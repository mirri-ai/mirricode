/**
 * B1-L9: Skill System port verification.
 *
 * Asserts that v2's builtin skill set, brand directories, and
 * discovery/priority/dedup mechanics match mirri's v1 4-tier system.
 *
 * Run: `pnpm --filter @mirri-ai/agent-core-v2 exec vitest run
 *      test/app/skillCatalog/builtinSkills.test.ts`.
 */

import { mkdtemp, mkdir, realpath, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';

import { join } from 'pathe';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  BUILTIN_SKILLS,
  registerBuiltinSkills,
} from '#/app/skillCatalog/builtin/builtin';
import { InMemorySkillCatalog } from '#/app/skillCatalog/registry';
import {
  projectRoots,
  userRoots,
} from '#/app/skillCatalog/skillRoots';
import { SKILL_SOURCE_PRIORITY } from '#/app/skillCatalog/skillSource';
import { normalizeSkillName } from '#/app/skillCatalog/types';
import { stubSkill } from './stubs';

// ---------------------------------------------------------------------------
// 1. Builtin skill set matches mirri's 6 families
// ---------------------------------------------------------------------------

describe('mirri builtin skills', () => {
  it('should register exactly the 6 mirri builtin skill families (8 definitions)', () => {
    // v1 has 6 skill families yielding 8 SkillDefinition objects:
    //   mcp-config, import-from-cc-codex, update-config, custom-theme,
    //   write-goal, sub-skill (parent), sub-skill.review, sub-skill.consolidate
    expect(BUILTIN_SKILLS).toHaveLength(8);

    const names = BUILTIN_SKILLS.map((s) => s.name);
    expect(names).toContain('mcp-config');
    expect(names).toContain('import-from-cc-codex');
    expect(names).toContain('update-config');
    expect(names).toContain('custom-theme');
    expect(names).toContain('write-goal');
    expect(names).toContain('sub-skill');
    expect(names).toContain('sub-skill.review');
    expect(names).toContain('sub-skill.consolidate');
  });

  it('should not contain the kimi-code-specific check-mirri-code-docs skill', () => {
    const names = BUILTIN_SKILLS.map((s) => s.name);
    expect(names).not.toContain('check-mirri-code-docs');
  });

  it('should mark all builtin skills as source "builtin"', () => {
    for (const skill of BUILTIN_SKILLS) {
      expect(skill.source).toBe('builtin');
    }
  });

  it('should register all builtins into an InMemorySkillCatalog via registerBuiltinSkills', () => {
    const catalog = new InMemorySkillCatalog();
    registerBuiltinSkills(catalog);

    const listed = catalog.listSkills();
    expect(listed).toHaveLength(8);

    for (const skill of BUILTIN_SKILLS) {
      const found = catalog.getSkill(skill.name);
      expect(found, `builtin skill "${skill.name}" should be discoverable`).toBeDefined();
      expect(found?.source).toBe('builtin');
    }
  });

  it('should have model-invocable builtins (update-config, write-goal) in invocable listing', () => {
    const catalog = new InMemorySkillCatalog();
    registerBuiltinSkills(catalog);

    const invocable = catalog.listInvocableSkills();
    const invocableNames = invocable.map((s) => s.name);
    // update-config and write-goal have disableModelInvocation !== true
    expect(invocableNames).toContain('update-config');
    expect(invocableNames).toContain('write-goal');
    // mcp-config, custom-theme, import-from-cc-codex, sub-skill family have disableModelInvocation: true
    expect(invocableNames).not.toContain('mcp-config');
    expect(invocableNames).not.toContain('custom-theme');
    expect(invocableNames).not.toContain('import-from-cc-codex');
    expect(invocableNames).not.toContain('sub-skill');
    expect(invocableNames).not.toContain('sub-skill.review');
    expect(invocableNames).not.toContain('sub-skill.consolidate');
  });
});

// ---------------------------------------------------------------------------
// 2. Brand directory constants include .mirri-code/skills
// ---------------------------------------------------------------------------

describe('skill brand directories', () => {
  let tmp: string;

  beforeEach(async () => {
    tmp = await mkdtemp(join(tmpdir(), 'skill-brand-'));
  });

  afterEach(async () => {
    await rm(tmp, { recursive: true, force: true });
  });

  it('should discover .mirri-code/skills as a project brand root', async () => {
    await mkdir(join(tmp, '.git'), { recursive: true });
    await mkdir(join(tmp, '.mirri-code', 'skills', 'my-skill'), { recursive: true });

    const roots = await projectRoots(tmp);

    const brandRoot = roots.find((r) => r.path.endsWith('.mirri-code/skills'));
    expect(brandRoot, 'project brand root .mirri-code/skills should be discovered').toBeDefined();
    expect(brandRoot?.source).toBe('project');
  });

  it('should discover the user brand skills directory under homeDir', async () => {
    const homeDir = join(tmp, 'home');
    await mkdir(join(homeDir, 'skills', 'my-skill'), { recursive: true });

    const roots = await userRoots(homeDir, tmp);

    const brandRoot = roots.find((r) => r.path.endsWith('/skills') && r.source === 'user');
    expect(brandRoot, 'user brand root skills/ under homeDir should be discovered').toBeDefined();
  });

  it('should prefer .mirri-code/skills over .agents/skills when both exist', async () => {
    await mkdir(join(tmp, '.git'), { recursive: true });
    await mkdir(join(tmp, '.mirri-code', 'skills'), { recursive: true });
    await mkdir(join(tmp, '.agents', 'skills'), { recursive: true });

    const roots = await projectRoots(tmp);
    const brandIdx = roots.findIndex((r) => r.path.endsWith('.mirri-code/skills'));
    const genericIdx = roots.findIndex((r) => r.path.endsWith('.agents/skills'));

    expect(brandIdx).toBeGreaterThanOrEqual(0);
    expect(genericIdx).toBeGreaterThanOrEqual(0);
    expect(brandIdx, 'brand root should come before generic root').toBeLessThan(genericIdx);
  });
});

// ---------------------------------------------------------------------------
// 3. Priority & dedup — 4-tier system matches v1
// ---------------------------------------------------------------------------

describe('skill priority and dedup', () => {
  it('should define priority as workspace > user > extra > plugin > builtin', () => {
    expect(SKILL_SOURCE_PRIORITY.workspace).toBeGreaterThan(SKILL_SOURCE_PRIORITY.user);
    expect(SKILL_SOURCE_PRIORITY.user).toBeGreaterThan(SKILL_SOURCE_PRIORITY.extra);
    expect(SKILL_SOURCE_PRIORITY.extra).toBeGreaterThan(SKILL_SOURCE_PRIORITY.plugin);
    expect(SKILL_SOURCE_PRIORITY.plugin).toBeGreaterThan(SKILL_SOURCE_PRIORITY.builtin);
  });

  it('should let higher-priority sources override lower on name collision', () => {
    // InMemorySkillCatalog uses first-write-wins by default, so we test
    // the replace=true path that WorkspaceSkillCatalogService uses during
    // its ordered-merge: higher-priority contributions overwrite lower.
    const catalog = new InMemorySkillCatalog();
    catalog.register(stubSkill('shared', { description: 'from builtin', source: 'builtin' }), { replace: true });
    catalog.register(stubSkill('shared', { description: 'from extra', source: 'extra' }), { replace: true });
    catalog.register(stubSkill('shared', { description: 'from user', source: 'user' }), { replace: true });
    catalog.register(stubSkill('shared', { description: 'from project', source: 'project' }), { replace: true });

    expect(catalog.getSkill('shared')?.description).toBe('from project');
  });

  it('should dedup by normalized (lowercased) skill name', () => {
    const catalog = new InMemorySkillCatalog();

    catalog.register(stubSkill('My-Skill', { description: 'first', source: 'user' }));

    // Same name, different case — should be treated as the same skill
    expect(catalog.getSkill('my-skill')).toBeDefined();
    expect(catalog.getSkill('MY-SKILL')).toBeDefined();
    expect(catalog.getSkill('My-Skill')?.description).toBe('first');
  });

  it('should list builtin skills under the "Built-in" heading in the model listing', () => {
    const catalog = new InMemorySkillCatalog();
    registerBuiltinSkills(catalog);
    // Add a user skill to verify grouping
    catalog.register(stubSkill('user-skill', { source: 'user' }));

    const listing = catalog.getModelSkillListing();

    // Builtin heading must appear
    expect(listing).toContain('Built-in');
    // User heading must appear before Built-in
    expect(listing.indexOf('User')).toBeLessThan(listing.indexOf('Built-in'));
    // Invocable builtins should be listed
    expect(listing).toContain('update-config');
    expect(listing).toContain('write-goal');
  });
});
