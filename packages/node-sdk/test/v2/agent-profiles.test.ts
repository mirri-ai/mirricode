/**
 * Agent profile data plane (R5-2) — `SDKRpcClientV2` over the real in-process
 * v2 engine through the klient facade: the read (`listAgentProfiles`), the
 * toggle write channel (`setAgentProfileEnabled`), and the parity surface
 * with the rust engine's rows (protocol `profileEntrySchema`).
 */
import { mkdtempSync } from 'node:fs';
import { mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, describe, expect, it } from 'vitest';
import { profileEntrySchema } from '@mirri-ai/protocol';

import { SDKRpcClientV2 } from '#/sdk-rpc-client-v2';

const clients: SDKRpcClientV2[] = [];
const tempDirs: string[] = [];

afterEach(async () => {
  await Promise.allSettled(clients.splice(0).map((client) => client.close()));
  for (const dir of tempDirs.splice(0)) {
    await rm(dir, { recursive: true, force: true, maxRetries: 3, retryDelay: 25 } as never);
  }
});

function createClient(): { client: SDKRpcClientV2; home: string } {
  const home = mkdtempSync(join(tmpdir(), 'mirri-sdk-v2-profiles-'));
  tempDirs.push(home);
  const client = new SDKRpcClientV2({
    homeDir: home,
    identity: {
      productName: 'mirri-sdk-test',
      version: '0.0.0',
      platform: 'mirri_code_cli',
    },
    uiMode: 'cli',
  });
  clients.push(client);
  return { client, home };
}

async function writeAgentFile(home: string, name: string, text: string): Promise<void> {
  const dir = join(home, 'agents');
  await mkdir(dir, { recursive: true });
  await writeFile(join(dir, `${name}.md`), text, 'utf-8');
}

describe('SDKRpcClientV2 agent profile data plane', () => {
  it('lists the merged view with protocol-shaped rows and an always-present skipped array', async () => {
    const { client, home } = createClient();
    await writeAgentFile(
      home,
      'code-reviewer',
      '---\nname: code-reviewer\ndescription: "Reviews code"\n---\nYou review code.',
    );
    // A parse failure → skipped entry.
    await writeAgentFile(home, 'broken', 'not frontmatter at all');

    const result = await client.listAgentProfiles();

    // `skipped` is always present on the v2 side, even before any assertion.
    expect(Array.isArray(result.skipped)).toBe(true);

    const names = result.items.map((row) => row.name);
    expect(names).toContain('agent'); // builtin essential default
    expect(names).toContain('code-reviewer');
    expect(names).not.toContain('broken');

    const reviewer = result.items.find((row) => row.name === 'code-reviewer')!;
    expect(reviewer).toMatchObject({
      source: 'user',
      builtin: false,
      essential: false,
      enabled: true,
      has_override: false,
      file_path: join(home, 'agents', 'code-reviewer.md'),
    });

    const agent = result.items.find((row) => row.name === 'agent')!;
    expect(agent).toMatchObject({ source: 'builtin', builtin: true, essential: true });

    expect(result.skipped).toHaveLength(1);
    expect(result.skipped[0]!.path).toBe(join(home, 'agents', 'broken.md'));
    expect(result.skipped[0]!.reason).toBeTruthy();

    // The row shape is the shared protocol REST contract — every listed
    // row must survive `profileEntrySchema.parse` untouched (the rust
    // engine's parity assertion).
    for (const item of result.items) {
      expect(profileEntrySchema.parse(item)).toEqual(item);
    }
  });

  it('disables a profile through the same write channel the REST endpoint applies', async () => {
    const { client, home } = createClient();
    await writeAgentFile(
      home,
      'code-reviewer',
      '---\nname: code-reviewer\ndescription: "Reviews code"\n---\nYou review code.',
    );

    const result = await client.setAgentProfileEnabled('code-reviewer', false);

    expect(result).toEqual({ ok: true });
    const written = await readFile(join(home, 'agents', 'code-reviewer.md'), 'utf-8');
    expect(written).toContain('enabled: false');
    expect(written).toContain('You review code.');

    // The read reflects the toggle.
    const after = await client.listAgentProfiles();
    expect(after.items.find((row) => row.name === 'code-reviewer')!.enabled).toBe(false);
  });

  it('re-enables a previously disabled profile', async () => {
    const { client, home } = createClient();
    await writeAgentFile(
      home,
      'off-role',
      '---\nname: off-role\ndescription: "Disabled"\nenabled: false\n---\nYou are off.',
    );

    await client.setAgentProfileEnabled('off-role', true);

    const written = await readFile(join(home, 'agents', 'off-role.md'), 'utf-8');
    // Re-enabling drops the `enabled: false` frontmatter key entirely.
    expect(written).not.toContain('enabled');
    const after = await client.listAgentProfiles();
    expect(after.items.find((row) => row.name === 'off-role')!.enabled).toBe(true);
  });

  it('rejects disabling the essential profile and unknown names with engine errors', async () => {
    const { client } = createClient();

    await expect(client.setAgentProfileEnabled('agent', false)).rejects.toMatchObject({
      code: 'agent_profile.essential',
      message: 'Cannot disable the essential profile "agent"',
    });
    await expect(client.setAgentProfileEnabled('no-such-role', true)).rejects.toMatchObject({
      code: 'agent_profile.not_found',
      message: 'Agent profile "no-such-role" was not found',
    });
  });
});
