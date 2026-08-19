import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { mkdir, writeFile, rm } from 'node:fs/promises';
import { join, resolve } from 'pathe';
import { promises as fs } from 'node:fs';
import { discoverAgentFiles } from '#/discovery';
import { projectAgentRootCandidates } from '#/roots';
import type { ProfileFs } from '#/fs';
import type { AgentFileRoot } from '#/schema';

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
let root: AgentFileRoot;

describe('discoverAgentFiles', () => {
  beforeEach(async () => {
    tmpDir = await fs.mkdtemp('/tmp/agent-profile-disc-test-');
    root = { path: tmpDir, source: 'user' };
  });

  afterEach(async () => {
    await rm(tmpDir, { recursive: true, force: true });
  });

  it('should discover a single valid .md agent file', async () => {
    await writeFile(
      join(tmpDir, 'reviewer.md'),
      '---\nname: reviewer\ndescription: Test\n---\nYou are a reviewer.',
    );
    const result = await discoverAgentFiles({ fs: nodeFs, roots: [root] });
    expect(result.agents).toHaveLength(1);
    expect(result.agents[0]!.name).toBe('reviewer');
    expect(result.skipped).toHaveLength(0);
  });

  it('should skip invalid files and add them to skipped list', async () => {
    await writeFile(join(tmpDir, 'bad.md'), 'No frontmatter at all.');
    const result = await discoverAgentFiles({ fs: nodeFs, roots: [root] });
    expect(result.agents).toHaveLength(0);
    expect(result.skipped).toHaveLength(1);
    expect(result.skipped[0]!.path).toContain('bad.md');
  });

  it('should prefer .md over .yaml when both exist for same name in same directory', async () => {
    await writeFile(
      join(tmpDir, 'reviewer.md'),
      '---\nname: reviewer\ndescription: From MD\n---\nYou are from MD.',
    );
    await writeFile(
      join(tmpDir, 'reviewer.yaml'),
      'name: reviewer\ndescription: From YAML\nprompt: You are from YAML.\n',
    );
    const warnings: string[] = [];
    const result = await discoverAgentFiles({
      fs: nodeFs,
      roots: [root],
      warn: (msg) => warnings.push(msg),
    });
    expect(result.agents).toHaveLength(1);
    expect(result.agents[0]!.description).toBe('From MD');
    expect(warnings.some((w) => w.includes('reviewer'))).toBe(true);
  });

  it('should skip dotfiles and node_modules directories', async () => {
    await mkdir(join(tmpDir, '.hidden'), { recursive: true });
    await writeFile(
      join(tmpDir, '.hidden', 'agent.md'),
      '---\nname: hidden\ndescription: Hidden\n---\nBody.',
    );
    await mkdir(join(tmpDir, 'node_modules'), { recursive: true });
    await writeFile(
      join(tmpDir, 'node_modules', 'agent.md'),
      '---\nname: nm\ndescription: NM\n---\nBody.',
    );
    const result = await discoverAgentFiles({ fs: nodeFs, roots: [root] });
    expect(result.agents).toHaveLength(0);
  });

  it('should return scannedRoots listing all root paths', async () => {
    const result = await discoverAgentFiles({ fs: nodeFs, roots: [root] });
    expect(result.scannedRoots).toEqual([tmpDir]);
  });

  it('should handle non-existent root directory gracefully', async () => {
    const badRoot: AgentFileRoot = { path: '/nonexistent/path', source: 'user' };
    const result = await discoverAgentFiles({ fs: nodeFs, roots: [badRoot] });
    expect(result.agents).toHaveLength(0);
    expect(result.skipped).toHaveLength(0);
  });

  it('should rethrow fs errors when the isUnavailable hook reports them', async () => {
    const unavailableFs: ProfileFs = {
      readText: async () => {
        throw new Error('fs-unavailable');
      },
      readdir: async () => {
        throw new Error('fs-unavailable');
      },
      realpath: async () => {
        throw new Error('fs-unavailable');
      },
      stat: async () => {
        throw new Error('fs-unavailable');
      },
      isUnavailable: (error) => error instanceof Error && error.message === 'fs-unavailable',
    };

    await expect(discoverAgentFiles({ fs: unavailableFs, roots: [root] })).rejects.toThrow(
      'fs-unavailable',
    );
  });

  it('should absorb the same fs errors when no isUnavailable hook is provided', async () => {
    const brokenFs: ProfileFs = {
      readText: async () => {
        throw new Error('fs-unavailable');
      },
      readdir: async () => {
        throw new Error('fs-unavailable');
      },
      realpath: async () => {
        throw new Error('fs-unavailable');
      },
      stat: async () => {
        throw new Error('fs-unavailable');
      },
    };

    const result = await discoverAgentFiles({ fs: brokenFs, roots: [root] });
    expect(result.agents).toHaveLength(0);
    expect(result.skipped).toHaveLength(0);
  });
});

describe('projectAgentRootCandidates', () => {
  let workDir: string;

  beforeEach(async () => {
    workDir = await fs.mkdtemp('/tmp/agent-profile-roots-test-');
  });

  afterEach(async () => {
    await rm(workDir, { recursive: true, force: true });
  });

  it('should return the project root and brand/generic candidate dirs when .git exists', async () => {
    await mkdir(join(workDir, '.git'), { recursive: true });

    const { projectRoot, candidates } = await projectAgentRootCandidates(nodeFs, workDir);
    expect(projectRoot).toBe(resolve(workDir));
    expect(candidates).toEqual([
      join(resolve(workDir), '.mirri-code/agents'),
      join(resolve(workDir), '.agents/agents'),
    ]);
  });

  it('should fall back to the given directory when no .git marker exists above it', async () => {
    const { projectRoot, candidates } = await projectAgentRootCandidates(nodeFs, workDir);
    expect(projectRoot).toBe(resolve(workDir));
    expect(candidates).toHaveLength(2);
  });
});
