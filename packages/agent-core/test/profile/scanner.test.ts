import { mkdtempSync, writeFileSync, mkdirSync } from 'node:fs';
import { rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'pathe';

import { afterEach, describe, expect, it } from 'vitest';

import { discoverAgentProfiles, resolveAgentProfileRoots } from '../../src/profile/scanner';

const tempDirs: string[] = [];

afterEach(async () => {
  for (const dir of tempDirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

function makeTempDir(): string {
  const dir = mkdtempSync(join(tmpdir(), 'agent-profile-scanner-'));
  tempDirs.push(dir);
  return dir;
}

function writeYaml(dir: string, filename: string, content: string): string {
  const filePath = join(dir, filename);
  writeFileSync(filePath, content, 'utf-8');
  return filePath;
}

function writeMarkdown(dir: string, filename: string, frontmatter: string, body: string): string {
  const filePath = join(dir, filename);
  writeFileSync(filePath, `---\n${frontmatter}\n---\n${body}`, 'utf-8');
  return filePath;
}

const VALID_YAML = (name: string, extends_ = 'agent'): string =>
  `name: ${name}\nextends: ${extends_}\ndescription: Test agent\n`;

const VALID_MD_FRONTMATTER = (name: string, extends_ = 'agent'): string =>
  `name: ${name}\nextends: ${extends_}\ndescription: Test agent\n`;

describe('agent profile scanner', () => {
  it('should discover yaml profiles from user dir with source=user', async () => {
    const userDir = makeTempDir();
    const agentsDir = join(userDir, 'agents');
    mkdirSync(agentsDir, { recursive: true });
    writeYaml(agentsDir, 'reviewer.yaml', VALID_YAML('reviewer'));

    const roots = await resolveAgentProfileRoots({
      paths: { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
    });
    const discovered = await discoverAgentProfiles(roots);

    const reviewer = discovered.find((p) => p.name === 'reviewer');
    expect(reviewer).toBeDefined();
    expect(reviewer?.source).toBe('user');
  });

  it('should discover profiles from project dir with source=project', async () => {
    const workDir = makeTempDir();
    const projectAgentsDir = join(workDir, '.mirri-code', 'agents');
    mkdirSync(projectAgentsDir, { recursive: true });
    writeYaml(projectAgentsDir, 'test-runner.yaml', VALID_YAML('test-runner'));

    const roots = await resolveAgentProfileRoots({
      paths: { brandHomeDir: makeTempDir(), workDir, userHomeDir: makeTempDir() },
    });
    const discovered = await discoverAgentProfiles(roots);

    const found = discovered.find((p) => p.name === 'test-runner');
    expect(found).toBeDefined();
    expect(found?.source).toBe('project');
  });

  it('should let project override user when same name', async () => {
    const userDir = makeTempDir();
    const workDir = makeTempDir();

    const userAgentsDir = join(userDir, 'agents');
    mkdirSync(userAgentsDir, { recursive: true });
    writeYaml(userAgentsDir, 'reviewer.yaml', VALID_YAML('reviewer'));

    const projectAgentsDir = join(workDir, '.mirri-code', 'agents');
    mkdirSync(projectAgentsDir, { recursive: true });
    writeYaml(projectAgentsDir, 'reviewer.yaml', VALID_YAML('reviewer'));

    const roots = await resolveAgentProfileRoots({
      paths: { brandHomeDir: userDir, workDir, userHomeDir: makeTempDir() },
    });
    const discovered = await discoverAgentProfiles(roots);

    const reviewers = discovered.filter((p) => p.name === 'reviewer');
    expect(reviewers).toHaveLength(1);
    expect(reviewers[0]?.source).toBe('project');
  });

  it('should discover from extra dirs with source=extra', async () => {
    const extraDir = makeTempDir();
    writeYaml(extraDir, 'linter.yaml', VALID_YAML('linter'));

    const roots = await resolveAgentProfileRoots({
      paths: { brandHomeDir: makeTempDir(), workDir: makeTempDir(), userHomeDir: makeTempDir() },
      extraDirs: [extraDir],
    });
    const discovered = await discoverAgentProfiles(roots);

    const found = discovered.find((p) => p.name === 'linter');
    expect(found).toBeDefined();
    expect(found?.source).toBe('extra');
  });

  it('should skip directories that do not exist', async () => {
    const roots = await resolveAgentProfileRoots({
      paths: {
        brandHomeDir: '/nonexistent-brand-home',
        workDir: '/nonexistent-work-dir',
        userHomeDir: '/nonexistent-user-home',
      },
    });
    expect(roots).toHaveLength(0);

    const discovered = await discoverAgentProfiles(roots);
    expect(discovered).toHaveLength(0);
  });

  it('should discover and parse .md agent files with frontmatter + body', async () => {
    const userDir = makeTempDir();
    const agentsDir = join(userDir, 'agents');
    mkdirSync(agentsDir, { recursive: true });
    writeMarkdown(
      agentsDir,
      'reviewer.md',
      VALID_MD_FRONTMATTER('reviewer'),
      'You are a code reviewer.',
    );

    const roots = await resolveAgentProfileRoots({
      paths: { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
    });
    const discovered = await discoverAgentProfiles(roots);

    const reviewer = discovered.find((p) => p.name === 'reviewer');
    expect(reviewer).toBeDefined();
    expect(reviewer?.source).toBe('user');
    expect(reviewer?.raw.systemPromptTemplate).toBe('You are a code reviewer.');
  });

  it('should prefer .md over .yaml when both exist with the same name in the same directory', async () => {
    const userDir = makeTempDir();
    const agentsDir = join(userDir, 'agents');
    mkdirSync(agentsDir, { recursive: true });
    writeYaml(agentsDir, 'reviewer.yaml', VALID_YAML('reviewer'));
    writeMarkdown(
      agentsDir,
      'reviewer.md',
      VALID_MD_FRONTMATTER('reviewer'),
      'You are a code reviewer.',
    );

    const roots = await resolveAgentProfileRoots({
      paths: { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
    });
    const discovered = await discoverAgentProfiles(roots);

    const reviewer = discovered.find((p) => p.name === 'reviewer');
    expect(reviewer).toBeDefined();
    expect(reviewer?.path.endsWith('.md')).toBe(true);
    expect(reviewer?.raw.systemPromptTemplate).toBe('You are a code reviewer.');
  });

  it('should warn when both .md and .yaml exist with the same name in the same directory', async () => {
    const userDir = makeTempDir();
    const agentsDir = join(userDir, 'agents');
    mkdirSync(agentsDir, { recursive: true });
    writeYaml(agentsDir, 'reviewer.yaml', VALID_YAML('reviewer'));
    writeMarkdown(
      agentsDir,
      'reviewer.md',
      VALID_MD_FRONTMATTER('reviewer'),
      'You are a code reviewer.',
    );

    const roots = await resolveAgentProfileRoots({
      paths: { brandHomeDir: userDir, workDir: makeTempDir(), userHomeDir: makeTempDir() },
    });

    const warnings: string[] = [];
    const discovered = await discoverAgentProfiles(roots, (msg) => warnings.push(msg));

    expect(warnings.some((w) => w.includes('reviewer') && w.toLowerCase().includes('duplicate'))).toBe(true);
    expect(discovered).toHaveLength(1);
  });
});
