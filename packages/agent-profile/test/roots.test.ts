import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { rm } from 'node:fs/promises';
import { join } from 'pathe';
import { userAgentRoots, projectAgentRoots } from '#/roots';
import type { ProfileFs } from '#/fs';

// Real Node.js fs-based ProfileFs for integration testing.
import { promises as fs } from 'node:fs';
const nodeFs: ProfileFs = {
  readText: (p) => fs.readFile(p, 'utf-8'),
  readdir: async (p) => {
    const entries = await fs.readdir(p, { withFileTypes: true });
    return entries.map((e) => ({
      name: e.name,
      isFile: e.isFile(),
      isDirectory: e.isDirectory(),
    }));
  },
  realpath: (p) => fs.realpath(p),
  stat: async (p) => {
    const s = await fs.stat(p);
    return { isFile: s.isFile(), isDirectory: s.isDirectory() };
  },
};

let tmpDir: string;

describe('userAgentRoots', () => {
  beforeEach(async () => {
    tmpDir = await fs.mkdtemp('/tmp/agent-profile-roots-test-');
  });

  afterEach(async () => {
    await rm(tmpDir, { recursive: true, force: true });
  });

  it('should return empty array when no agents directory exists', async () => {
    const roots = await userAgentRoots(nodeFs, tmpDir, tmpDir);
    expect(roots).toEqual([]);
  });

  it('should return brand agents dir when <homeDir>/agents exists', async () => {
    const agentsDir = join(tmpDir, 'agents');
    await fs.mkdir(agentsDir, { recursive: true });
    const roots = await userAgentRoots(nodeFs, tmpDir, tmpDir);
    expect(roots).toHaveLength(1);
    expect(roots[0]!.source).toBe('user');
    expect(roots[0]!.path).toContain('agents');
  });
});

describe('projectAgentRoots', () => {
  beforeEach(async () => {
    tmpDir = await fs.mkdtemp('/tmp/agent-profile-roots-test-');
  });

  afterEach(async () => {
    await rm(tmpDir, { recursive: true, force: true });
  });

  it('should return empty array when no project agents dir exists', async () => {
    const roots = await projectAgentRoots(nodeFs, tmpDir);
    expect(roots).toEqual([]);
  });

  it('should find .mirri-code/agents when it exists under project root with .git', async () => {
    await fs.mkdir(join(tmpDir, '.git'), { recursive: true });
    await fs.mkdir(join(tmpDir, '.mirri-code', 'agents'), { recursive: true });
    const roots = await projectAgentRoots(nodeFs, tmpDir);
    expect(roots.length).toBeGreaterThanOrEqual(1);
    expect(roots.some((r) => r.path.includes('.mirri-code'))).toBe(true);
  });
});
