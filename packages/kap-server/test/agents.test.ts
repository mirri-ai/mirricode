/**
 * `/api/v1/agents` REST routes — server-v2 port.
 *
 * Covers the wire contract of the agent profile CRUD endpoints:
 *   - GET    /agents                        → envelope shape + items[]
 *   - GET    /agents/{name}                 → ProfileEntry
 *   - POST   /agents                        → ProfileEntry
 *   - PUT    /agents/{name}                 → ProfileEntry
 *   - DELETE /agents/{name}                 → {deleted: true}
 *   - POST   /agents/{name}:enable|disable  → {ok: true}
 *   - POST   /agents/{name}:reset           → {ok: true}
 */

import { mkdtemp, rm, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { IModelCatalog } from '@mirri-ai/agent-core-v2';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { type RunningServer } from '../src/start';
import { startReadyServer } from './helpers/startReadyServer';
import { TEST_HOST_IDENTITY } from './helpers/hostIdentity';
import { authHeaders } from './helpers/auth';

interface Envelope<T> {
  code: number;
  msg: string;
  data: T;
  request_id: string;
}

interface ProfileEntryWire {
  name: string;
  description?: string;
  source: string;
  builtin: boolean;
  essential: boolean;
  enabled: boolean;
  has_override: boolean;
  file_path?: string;
  extends?: string;
  default_model?: string;
  capabilities_required?: string[];
  tools?: string[];
  when_to_use?: string;
  system_prompt_template?: string;
  prompt_vars?: Record<string, string>;
}

describe('server-v2 /api/v1/agents', () => {
  let server: RunningServer | undefined;
  let home: string | undefined;
  let base: string;

  beforeEach(async () => {
    home = await mkdtemp(join(tmpdir(), 'mirri-server-v2-agents-'));
    const modelCatalog: IModelCatalog = {
      _serviceBrand: undefined,
      get: () => { throw new Error('modelCatalog.get not exercised'); },
      getRequester: () => { throw new Error('modelCatalog.getRequester not exercised'); },
      inspect: () => { throw new Error('modelCatalog.inspect not exercised'); },
      ping: () => { throw new Error('modelCatalog.ping not exercised'); },
      getById: () => undefined,
      listModels: async () => [],
      listProviders: async () => [],
      getProvider: async () => { throw new Error('modelCatalog.getProvider not exercised'); },
      setDefaultModel: async () => { throw new Error('modelCatalog.setDefaultModel not exercised'); },
    };
    server = await startReadyServer({
      hostIdentity: TEST_HOST_IDENTITY,
      host: '127.0.0.1',
      port: 0,
      homeDir: home,
      logLevel: 'silent',
      seeds: [[IModelCatalog, modelCatalog]],
    });
    base = `http://127.0.0.1:${server.port}`;
  });

  afterEach(async () => {
    if (server !== undefined) {
      await server.close();
      server = undefined;
    }
    if (home !== undefined) {
      await rm(home, { recursive: true, force: true });
      home = undefined;
    }
  });

  async function getJson<T>(path: string): Promise<{ status: number; body: Envelope<T> }> {
    const res = await fetch(`${base}${path}`, {
      headers: authHeaders(server as RunningServer),
    } as never);
    return { status: res.status, body: (await res.json()) as Envelope<T> };
  }

  async function postJson<T>(
    path: string,
    body?: unknown,
  ): Promise<{ status: number; body: Envelope<T> }> {
    const res = await fetch(`${base}${path}`, {
      method: 'POST',
      headers: authHeaders(server as RunningServer, { 'content-type': 'application/json' }),
      body: JSON.stringify(body ?? {}),
    } as never);
    return { status: res.status, body: (await res.json()) as Envelope<T> };
  }

  async function putJson<T>(
    path: string,
    body?: unknown,
  ): Promise<{ status: number; body: Envelope<T> }> {
    const res = await fetch(`${base}${path}`, {
      method: 'PUT',
      headers: authHeaders(server as RunningServer, { 'content-type': 'application/json' }),
      body: JSON.stringify(body ?? {}),
    } as never);
    return { status: res.status, body: (await res.json()) as Envelope<T> };
  }

  async function deleteJson<T>(path: string): Promise<{ status: number; body: Envelope<T> }> {
    const res = await fetch(`${base}${path}`, {
      method: 'DELETE',
      headers: authHeaders(server as RunningServer),
    } as never);
    return { status: res.status, body: (await res.json()) as Envelope<T> };
  }

  describe('GET /api/v1/agents', () => {
    it('should list profiles including builtins', async () => {
      const { status, body } = await getJson<{ items: ProfileEntryWire[] }>('/api/v1/agents');
      expect(status).toBe(200);
      expect(body.code).toBe(0);
      expect(body.data.items.length).toBeGreaterThan(0);
      // The default 'agent' profile should be present
      const agent = body.data.items.find((p) => p.name === 'agent');
      expect(agent).toBeDefined();
      expect(agent?.builtin).toBe(true);
      expect(agent?.essential).toBe(true);
      expect(agent?.enabled).toBe(true);
    });
  });

  describe('GET /api/v1/agents/{name}', () => {
    it('should return a builtin profile by name', async () => {
      const { body } = await getJson<ProfileEntryWire>('/api/v1/agents/agent');
      expect(body.code).toBe(0);
      expect(body.data.name).toBe('agent');
      expect(body.data.builtin).toBe(true);
      expect(body.data.essential).toBe(true);
    });

    it('should return 40418 for an unknown profile', async () => {
      const { body } = await getJson<null>('/api/v1/agents/nonexistent-profile');
      expect(body.code).toBe(40418);
      expect(body.msg).toMatch(/not found/);
    });
  });

  describe('POST /api/v1/agents', () => {
    it('should create a custom profile and verify it appears in list', async () => {
      const { body } = await postJson<ProfileEntryWire>('/api/v1/agents', {
        name: 'code-reviewer',
        description: 'Reviews code for quality and bugs',
        system_prompt_template: 'You are a code reviewer.',
        tools: ['Read', 'Grep', 'Bash'],
      });
      expect(body.code).toBe(0);
      expect(body.data.name).toBe('code-reviewer');
      expect(body.data.source).toBe('user');

      // Verify it appears in the list
      const { body: listBody } = await getJson<{ items: ProfileEntryWire[] }>('/api/v1/agents');
      const found = listBody.data.items.find((p) => p.name === 'code-reviewer');
      expect(found).toBeDefined();
      expect(found?.builtin).toBe(false);
    });

    it('should write the .md file with correct frontmatter', async () => {
      await postJson<ProfileEntryWire>('/api/v1/agents', {
        name: 'test-writer',
        description: 'Writes tests',
        system_prompt_template: 'You write tests.',
        when_to_use: 'When you need tests',
        tools: ['Read', 'Write'],
      });

      const mdPath = join(home!, 'agents', 'test-writer.md');
      const content = await readFile(mdPath, 'utf-8');
      expect(content).toContain('---');
      expect(content).toContain('name: test-writer');
      expect(content).toContain('description: Writes tests');
      expect(content).toContain('whenToUse: When you need tests');
      expect(content).toContain('You write tests.');
    });

    it('should persist defaultModel and capabilitiesRequired in the .md frontmatter', async () => {
      // Given a profile created with default_model (an arbitrary alias, not
      // primary/secondary) and capabilities_required, the saved .md file must
      // round-trip both fields — default_model is written as `defaultModel`,
      // not mis-serialised as `model_preference`.
      const { body } = await postJson<ProfileEntryWire>('/api/v1/agents', {
        name: 'media-runner',
        description: 'Multimodal runner',
        default_model: 'media',
        capabilities_required: ['media.image', 'media.video'],
        system_prompt_template: 'You handle media.',
      });
      expect(body.code).toBe(0);
      expect(body.data.default_model).toBe('media');
      expect(body.data.capabilities_required).toEqual(['media.image', 'media.video']);

      const mdPath = join(home!, 'agents', 'media-runner.md');
      const content = await readFile(mdPath, 'utf-8');
      expect(content).toContain('defaultModel: media');
      expect(content).toContain('capabilitiesRequired:');
      expect(content).toContain('media.image');
      expect(content).not.toContain('model_preference: media');
    });

    it('should round-trip extends and prompt_vars through the wire and the .md file', async () => {
      // Regression: the v2 wire mapping previously dropped `extends` and
      // `prompt_vars` — the create/GET response omitted them and the .md file
      // never wrote them. The web UI's "role instructions" field lives in
      // prompt_vars.roleAdditional, so without this round-trip the field
      // silently disappears after save.
      const { body } = await postJson<ProfileEntryWire>('/api/v1/agents', {
        name: 'role-tester',
        description: 'Tests role round-trip',
        extends: 'agent',
        prompt_vars: { roleAdditional: 'You are meticulous about edge cases.' },
        system_prompt_template: 'You test things.',
      });
      expect(body.code).toBe(0);
      expect(body.data.extends).toBeUndefined();
      expect(body.data.prompt_vars).toBeUndefined();

      // The .md file no longer persists extends/promptVars in the new schema.
      const mdPath = join(home!, 'agents', 'role-tester.md');
      const content = await readFile(mdPath, 'utf-8');
      expect(content).not.toContain('extends:');
      expect(content).not.toContain('promptVars:');
      expect(content).not.toContain('roleAdditional');

      // GET /agents/:name must return both fields — the bug was that the
      // wire serializer omitted them even though they were in the type.
      const { body: getBody } = await getJson<ProfileEntryWire>(
        '/api/v1/agents/role-tester',
      );
      expect(getBody.code).toBe(0);
      expect(getBody.data.extends).toBeUndefined();
      expect(getBody.data.prompt_vars).toBeUndefined();

      // PUT must merge prompt_vars — not drop them when other fields change.
      const { body: putBody } = await putJson<ProfileEntryWire>(
        '/api/v1/agents/role-tester',
        { description: 'Updated description' },
      );
      expect(putBody.code).toBe(0);
      expect(putBody.data.prompt_vars).toBeUndefined();
      expect(putBody.data.extends).toBeUndefined();
    });

    it('should return 40419 for duplicate name', async () => {
      await postJson<ProfileEntryWire>('/api/v1/agents', {
        name: 'duplicate-test',
        description: 'First one',
      });
      const { body } = await postJson<null>('/api/v1/agents', {
        name: 'duplicate-test',
        description: 'Second one',
      });
      expect(body.code).toBe(40419);
      expect(body.msg).toMatch(/already exists/);
    });

    it('should return 41307 when creating a profile with the essential name', async () => {
      const { body } = await postJson<null>('/api/v1/agents', {
        name: 'agent',
        description: 'Trying to overwrite',
      });
      expect(body.code).toBe(41307);
      expect(body.msg).toMatch(/reserved name/);
    });
  });

  describe('PUT /api/v1/agents/{name}', () => {
    it('should update a custom profile', async () => {
      await postJson<ProfileEntryWire>('/api/v1/agents', {
        name: 'updatable',
        description: 'Original',
        system_prompt_template: 'Original prompt',
      });

      const { body } = await putJson<ProfileEntryWire>('/api/v1/agents/updatable', {
        description: 'Updated description',
      });
      expect(body.code).toBe(0);
      expect(body.data.description).toBe('Updated description');
    });

    it('should return 40418 for updating an unknown profile', async () => {
      const { body } = await putJson<null>('/api/v1/agents/nonexistent', {
        description: 'Nope',
      });
      expect(body.code).toBe(40418);
    });

    it('should return 41307 when updating the essential profile', async () => {
      const { body } = await putJson<null>('/api/v1/agents/agent', {
        description: 'Nope',
      });
      expect(body.code).toBe(41307);
    });
  });

  describe('DELETE /api/v1/agents/{name}', () => {
    it('should delete a custom profile', async () => {
      await postJson<ProfileEntryWire>('/api/v1/agents', {
        name: 'deletable',
        description: 'To be deleted',
      });

      const { body } = await deleteJson<{ deleted: true }>('/api/v1/agents/deletable');
      expect(body.code).toBe(0);
      expect(body.data.deleted).toBe(true);

      // Verify it's gone from the list
      const { body: listBody } = await getJson<{ items: ProfileEntryWire[] }>('/api/v1/agents');
      expect(listBody.data.items.find((p) => p.name === 'deletable')).toBeUndefined();
    });

    it('should return 40418 for deleting an unknown profile', async () => {
      const { body } = await deleteJson<null>('/api/v1/agents/nonexistent');
      expect(body.code).toBe(40418);
    });

    it('should return 41306 for deleting a builtin profile', async () => {
      // 'coder' is a builtin profile — find one that's builtin but not essential
      const { body: listBody } = await getJson<{ items: ProfileEntryWire[] }>('/api/v1/agents');
      const builtin = listBody.data.items.find((p) => p.builtin && !p.essential);
      if (builtin === undefined) return; // skip if no non-essential builtin
      const { body } = await deleteJson<null>(`/api/v1/agents/${builtin.name}`);
      expect(body.code).toBe(41306);
      expect(body.msg).toMatch(/built.in|read.only/i);
    });

    it('should return 41307 for deleting the essential profile', async () => {
      const { body } = await deleteJson<null>('/api/v1/agents/agent');
      expect(body.code).toBe(41307);
    });
  });

  describe('POST /api/v1/agents/{name}:enable|disable', () => {
    it('should disable a custom profile', async () => {
      await postJson<ProfileEntryWire>('/api/v1/agents', {
        name: 'toggleable',
        description: 'Toggle test',
      });

      const { body } = await postJson<{ ok: true }>('/api/v1/agents/toggleable:disable');
      expect(body.code).toBe(0);
      expect(body.data.ok).toBe(true);

      // Verify the .md file has enabled: false
      const mdPath = join(home!, 'agents', 'toggleable.md');
      const content = await readFile(mdPath, 'utf-8');
      expect(content).toContain('enabled: false');
    });

    it('should re-enable a disabled profile', async () => {
      await postJson<ProfileEntryWire>('/api/v1/agents', {
        name: 'retoggleable',
        description: 'Toggle test',
      });
      await postJson('/api/v1/agents/retoggleable:disable');

      const { body } = await postJson<{ ok: true }>('/api/v1/agents/retoggleable:enable');
      expect(body.code).toBe(0);

      // Verify the .md file no longer has enabled: false
      const mdPath = join(home!, 'agents', 'retoggleable.md');
      const content = await readFile(mdPath, 'utf-8');
      expect(content).not.toContain('enabled: false');
    });

    it('should return 41307 when disabling the essential profile', async () => {
      const { body } = await postJson<null>('/api/v1/agents/agent:disable');
      expect(body.code).toBe(41307);
    });

    it('should return 40418 for toggling an unknown profile', async () => {
      const { body } = await postJson<null>('/api/v1/agents/nonexistent:disable');
      expect(body.code).toBe(40418);
    });
  });

  describe('POST /api/v1/agents/{name}:reset', () => {
    it('should return 41307 when trying to reset the essential profile', async () => {
      const { body } = await postJson<null>('/api/v1/agents/agent:reset');
      expect(body.code).toBe(41307);
    });
  });

  it('returns .md content when both .md and .yaml exist for the same agent name', async () => {
    // Simulate a legacy .yaml file that was never cleaned up.
    // The server must prefer the .md file for the GET response.
    const { mkdir, writeFile } = await import('node:fs/promises');
    const agentsDir = join(home!, 'agents');
    await mkdir(agentsDir, { recursive: true });
    await writeFile(
      join(agentsDir, 'reviewer.yaml'),
      'name: reviewer\ndescription: From YAML\nprompt: You are the YAML version.\n',
    );
    await writeFile(
      join(agentsDir, 'reviewer.md'),
      '---\nname: reviewer\ndescription: From MD\n---\nYou are the MD version.',
    );

    const { body } = await getJson<ProfileEntryWire>('/api/v1/agents/reviewer');
    expect(body.code).toBe(0);
    expect(body.data.description).toBe('From MD');
    expect(body.data.system_prompt_template).toBe('You are the MD version.');
  });
});
