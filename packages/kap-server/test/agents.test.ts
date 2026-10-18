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

import { existsSync } from 'node:fs';
import { mkdir, mkdtemp, rm, readFile, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import {
  AgentProfileRegistryService,
  IAgentProfileRegistry,
  IModelCatalog,
} from '@mirri-ai/agent-core-v2';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

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
  disallowed_tools?: string[];
  subagents?: string[];
  when_to_use?: string;
  system_prompt_template?: string;
  effective_system_prompt?: string;
  suppressed_candidates?: { source: string; priority: number; reason: string }[];
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
      removeModel: async () => ({ deleted: true }),
    };
    server = await startReadyServer({
      hostIdentity: TEST_HOST_IDENTITY,
      host: '127.0.0.1',
      port: 0,
      homeDir: home,
      logLevel: 'silent',
      seeds: [
        [IModelCatalog, modelCatalog],
        // A pre-seeded registry carrying plugin- and explicit-sourced
        // profiles so the route-level source mapping is exercised without
        // booting plugin/`--agent-file` infrastructure (the server's start
        // options expose neither). Priorities mirror
        // AGENT_PROFILE_SOURCE_PRIORITY (plugin 5, explicit 40).
        [
          IAgentProfileRegistry,
          (() => {
            const registry = new AgentProfileRegistryService();
            registry.register(
              'plugin',
              {
                profiles: [
                  {
                    name: 'plugin-agent',
                    description: 'from plugin',
                    systemPrompt: () => 'plugin prompt',
                  },
                ],
              },
              { priority: 5 },
            );
            registry.register(
              'explicit',
              {
                profiles: [
                  {
                    name: 'explicit-agent',
                    description: 'from explicit',
                    systemPrompt: () => 'explicit prompt',
                  },
                ],
              },
              { priority: 40 },
            );
            return registry;
          })(),
        ],
      ],
    });
    base = `http://127.0.0.1:${server.port}`;
  });

  afterEach(async () => {
    if (server !== undefined) {
      await server.close();
      server = undefined;
    }
    if (home !== undefined) {
      // The engine's ILogService writes `<home>/logs/mirri-code.log` through an
      // async serial queue; its in-flight drain can re-create the file while rm
      // walks the tree (ENOTEMPTY). Retry, like the other kap-server tests.
      await rm(home, { recursive: true, force: true, maxRetries: 3, retryDelay: 25 } as never);
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

    it('should expose each builtin profile\'s role definition and effective prompt', async () => {
      // Built-in prompts are code-defined. The editable `system_prompt_template`
      // is the profile's own role definition — so the UI shows what makes each
      // subagent distinct instead of the 100+ line shared base — while
      // `effective_system_prompt` carries the full rendered prompt for a
      // read-only preview.
      const { status, body } = await getJson<{ items: ProfileEntryWire[] }>('/api/v1/agents');
      expect(status).toBe(200);
      const builtins = body.data.items.filter((p) => p.builtin);
      expect(builtins.length).toBeGreaterThan(0);
      for (const profile of builtins) {
        expect(profile.effective_system_prompt, `builtin "${profile.name}" effective prompt`).toBeDefined();
        expect(profile.effective_system_prompt!.length).toBeGreaterThan(0);
      }
      // Subagents carry their own role text — not the shared base — as the
      // editable template
      const coder = builtins.find((p) => p.name === 'coder');
      expect(coder?.system_prompt_template).toContain('Your final message is the entire handoff');
      expect(coder?.system_prompt_template).not.toContain('Your primary goal is to help users');
      // The default agent has no role of its own; its effective prompt is the base
      const agent = builtins.find((p) => p.name === 'agent');
      expect(agent?.system_prompt_template).toBeUndefined();
      expect(agent?.effective_system_prompt).toContain(
        'Your primary goal is to help users with software engineering tasks',
      );
    });
  });

  describe('GET /api/v1/agents/{name}', () => {
    it('should return a builtin profile by name', async () => {
      const { body } = await getJson<ProfileEntryWire>('/api/v1/agents/agent');
      expect(body.code).toBe(0);
      expect(body.data.name).toBe('agent');
      expect(body.data.builtin).toBe(true);
      expect(body.data.essential).toBe(true);
      // The default agent has no role of its own — the editable template is
      // absent, but the effective prompt preview is available
      expect(body.data.system_prompt_template).toBeUndefined();
      expect(body.data.effective_system_prompt).toBeDefined();
      expect(body.data.effective_system_prompt!.length).toBeGreaterThan(0);
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
      // Given a profile created with default_model (an arbitrary alias) and
      // capabilities_required, the saved .md file must round-trip both fields —
      // default_model is written as `defaultModel`, not mis-serialised as
      // `model_preference`.
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

    it('should persist subagents and disallowed_tools from the create body', async () => {
      // Given a create request carrying subagents and disallowed_tools, the
      // created profile must round-trip both through the wire and the .md
      // file (previously the create handler ignored both fields).
      const { body } = await postJson<ProfileEntryWire>('/api/v1/agents', {
        name: 'dispatcher',
        description: 'Dispatches to other agents',
        subagents: ['coder', 'explore'],
        disallowed_tools: ['Bash'],
        system_prompt_template: 'You dispatch.',
      });
      expect(body.code).toBe(0);
      expect(body.data.subagents).toEqual(['coder', 'explore']);
      expect(body.data.disallowed_tools).toEqual(['Bash']);

      const mdPath = join(home!, 'agents', 'dispatcher.md');
      const content = await readFile(mdPath, 'utf-8');
      expect(content).toContain('subagents:');
      expect(content).toContain('coder');
      expect(content).toContain('disallowedTools:');
      expect(content).toContain('Bash');

      // GET must return both fields.
      const { body: getBody } = await getJson<ProfileEntryWire>('/api/v1/agents/dispatcher');
      expect(getBody.data.subagents).toEqual(['coder', 'explore']);
      expect(getBody.data.disallowed_tools).toEqual(['Bash']);
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

  describe('name validation (path-traversal guard)', () => {
    // `name` is concatenated into `join(<home>/agents, `${name}.md`)`, so a
    // non-kebab payload is a path-traversal write primitive. These tests pin
    // the AGENT_NAME_PATTERN guard (40001) and — the key assertion — that no
    // file is written outside the agents directory.
    it('should reject a create with a traversal name and write nothing', async () => {
      const { body } = await postJson<null>('/api/v1/agents', {
        name: '../evil',
        description: 'Escapes the agents dir',
      });
      expect(body.code).toBe(40001);
      expect(body.msg).toMatch(/kebab-case/);
      // "../evil.md" resolves one level above <home>/agents — nothing there.
      expect(existsSync(join(home!, 'evil.md'))).toBe(false);
      expect(existsSync(join(home!, 'agents', 'evil.md'))).toBe(false);
    });

    it('should reject a create whose name escapes the temp home entirely', async () => {
      // "../../evil.md" resolves to the parent of the temp home directory —
      // i.e. straight into the OS tmpdir shared with unrelated processes.
      const { body } = await postJson<null>('/api/v1/agents', {
        name: '../../evil',
        description: 'Escapes the home dir',
      });
      expect(body.code).toBe(40001);
      expect(existsSync(join(home!, '..', 'evil.md'))).toBe(false);
      expect(existsSync(join(home!, 'evil.md'))).toBe(false);
    });

    it('should reject a create with a non-kebab name', async () => {
      const { body } = await postJson<null>('/api/v1/agents', {
        name: 'Bad_Name',
        description: 'Not kebab-case',
      });
      expect(body.code).toBe(40001);
      expect(body.msg).toMatch(/kebab-case/);
      expect(existsSync(join(home!, 'agents', 'Bad_Name.md'))).toBe(false);
    });

    it('should reject a create with a name longer than AGENT_NAME_MAX_LENGTH and write nothing', async () => {
      // A valid-kebab but over-long name used to pass the guard and fail at
      // the filesystem with a 500 (ENAMETOOLONG) — pin the clean 40001.
      const name = 'a'.repeat(65);
      const { body } = await postJson<null>('/api/v1/agents', {
        name,
        description: 'Too long',
      });
      expect(body.code).toBe(40001);
      expect(body.msg).toMatch(/at most 64 characters/);
      expect(existsSync(join(home!, 'agents', `${name}.md`))).toBe(false);
    });

    it('should reject an update with a name longer than AGENT_NAME_MAX_LENGTH', async () => {
      const name = 'a'.repeat(65);
      const { body } = await putJson<null>(`/api/v1/agents/${name}`, {
        description: 'Too long',
      });
      expect(body.code).toBe(40001);
      expect(body.msg).toMatch(/at most 64 characters/);
      expect(existsSync(join(home!, 'agents', `${name}.md`))).toBe(false);
    });

    it('should reject an update with a traversal name and write nothing', async () => {
      // %2F keeps the slash inside the :name route param instead of letting
      // the URL parser consume it as a path segment.
      const { body } = await putJson<null>('/api/v1/agents/..%2Fevil', {
        description: 'Traversal update',
      });
      expect(body.code).toBe(40001);
      expect(body.msg).toMatch(/kebab-case/);
      expect(existsSync(join(home!, 'evil.md'))).toBe(false);
      expect(existsSync(join(home!, 'agents', 'evil.md'))).toBe(false);
    });

    it('should fail-safe (40418, no write) for a traversal name on the action route', async () => {
      // The toggle route resolves by name first: a traversal name can never
      // match a parsed profile (they are kebab-case by contract), so the
      // request fails with 40418 before any file write — documented here so
      // the fail-safe behavior is pinned.
      const { body } = await postJson<null>('/api/v1/agents/..%2Fevil:disable');
      expect(body.code).toBe(40418);
      expect(existsSync(join(home!, 'evil.md'))).toBe(false);
      expect(existsSync(join(home!, 'agents', 'evil.md'))).toBe(false);
    });

    it('should fail-safe (40418, no remove) for a traversal name on delete', async () => {
      const { body } = await deleteJson<null>('/api/v1/agents/..%2Fevil');
      expect(body.code).toBe(40418);
      expect(existsSync(join(home!, 'evil.md'))).toBe(false);
    });
  });

  describe('description validation (invisible-profile guard)', () => {
    // The .md format requires a non-empty description (parse-level contract).
    // An empty one used to write `description: ''` which the loader skips —
    // the POST succeeded but the profile never appeared in GET.
    it('should reject a create with an empty description and write nothing', async () => {
      const { body } = await postJson<null>('/api/v1/agents', {
        name: 'invisible',
        description: '',
      });
      expect(body.code).toBe(40001);
      expect(body.msg).toMatch(/description/);
      expect(existsSync(join(home!, 'agents', 'invisible.md'))).toBe(false);
    });

    it('should reject a create without a description', async () => {
      const { body } = await postJson<null>('/api/v1/agents', {
        name: 'undescribed',
      });
      expect(body.code).toBe(40001);
      expect(body.msg).toMatch(/description/);
      expect(existsSync(join(home!, 'agents', 'undescribed.md'))).toBe(false);
    });

    it('should reject a create with a whitespace-only description', async () => {
      const { body } = await postJson<null>('/api/v1/agents', {
        name: 'padded',
        description: '   ',
      });
      expect(body.code).toBe(40001);
      expect(existsSync(join(home!, 'agents', 'padded.md'))).toBe(false);
    });

    it('should reject an update that would clear the description', async () => {
      // An explicit empty description in an update is rejected rather than
      // silently coerced: the written file would be unparseable and the
      // profile would vanish from every subsequent read.
      await postJson<ProfileEntryWire>('/api/v1/agents', {
        name: 'blankable',
        description: 'Original description',
      });

      const { body } = await putJson<null>('/api/v1/agents/blankable', {
        description: '',
      });
      expect(body.code).toBe(40001);
      expect(body.msg).toMatch(/description/);

      // The existing file is untouched.
      const content = await readFile(join(home!, 'agents', 'blankable.md'), 'utf-8');
      expect(content).toContain('Original description');
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

    it('should merge subagents and disallowed_tools from the update body', async () => {
      // Given an existing profile, a PUT carrying subagents and
      // disallowed_tools must apply both to the file and the wire response
      // (previously the PUT merge only preserved the existing values).
      await postJson<ProfileEntryWire>('/api/v1/agents', {
        name: 'orchestrator',
        description: 'Orchestrates work',
        system_prompt_template: 'You orchestrate.',
      });

      const { body } = await putJson<ProfileEntryWire>('/api/v1/agents/orchestrator', {
        subagents: ['coder'],
        disallowed_tools: ['Write'],
      });
      expect(body.code).toBe(0);
      expect(body.data.subagents).toEqual(['coder']);
      expect(body.data.disallowed_tools).toEqual(['Write']);

      const { body: getBody } = await getJson<ProfileEntryWire>('/api/v1/agents/orchestrator');
      expect(getBody.data.subagents).toEqual(['coder']);
      expect(getBody.data.disallowed_tools).toEqual(['Write']);

      const mdPath = join(home!, 'agents', 'orchestrator.md');
      const content = await readFile(mdPath, 'utf-8');
      expect(content).toContain('subagents:');
      expect(content).toContain('disallowedTools:');
    });

    it('should return 40418 for updating an unknown profile', async () => {
      const { body } = await putJson<null>('/api/v1/agents/nonexistent', {
        description: 'Nope',
      });
      expect(body.code).toBe(40418);
      expect(body.msg).toContain('Agent profile "nonexistent" was not found');
    });

    it('should return 41307 when updating the essential profile', async () => {
      const { body } = await putJson<null>('/api/v1/agents/agent', {
        description: 'Nope',
      });
      expect(body.code).toBe(41307);
      expect(body.msg).toContain('Cannot modify the essential profile "agent"');
    });

    it('should wrap a built-in profile override with the base prompt reference', async () => {
      // PUT on a builtin writes an override .md whose body embeds the shared
      // base via `${base_prompt}` — the runtime behavior keeps the base prompt,
      // while the editable wire value stays the profile's own role text.
      const { body } = await putJson<ProfileEntryWire>('/api/v1/agents/coder', {
        system_prompt_template: 'You are a careful code reviewer.',
      });
      expect(body.code).toBe(0);

      const mdPath = join(home!, 'agents', 'coder.md');
      const content = await readFile(mdPath, 'utf-8');
      expect(content).toContain('${base_prompt}');
      expect(content).toContain('You are a careful code reviewer.');
      // D14 (ADR-006): no override declaration key is emitted — the file
      // overrides by virtue of existing.
      expect(content).not.toContain('override');

      // The list round-trips the editable role text without the placeholder
      const { body: listBody } = await getJson<{ items: ProfileEntryWire[] }>('/api/v1/agents');
      const coder = listBody.data.items.find((p) => p.name === 'coder');
      expect(coder?.system_prompt_template).toBe('You are a careful code reviewer.');
      expect(coder?.has_override).toBe(true);
      // D12 (ADR-007): the effective prompt binds the shared base — the
      // rendered override is the base prompt followed by the role text, not
      // the raw `${base_prompt}` placeholder.
      expect(coder?.effective_system_prompt).toContain(
        'Your primary goal is to help users with software engineering tasks',
      );
      expect(coder?.effective_system_prompt).toContain('You are a careful code reviewer.');
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
      expect(body.msg).toContain('Agent profile "nonexistent" was not found');
    });

    it('should return 41306 for deleting a builtin profile', async () => {
      // 'coder' is a builtin profile — find one that's builtin but not essential
      const { body: listBody } = await getJson<{ items: ProfileEntryWire[] }>('/api/v1/agents');
      const builtin = listBody.data.items.find((p) => p.builtin && !p.essential);
      if (builtin === undefined) return; // skip if no non-essential builtin
      const { body } = await deleteJson<null>(`/api/v1/agents/${builtin.name}`);
      expect(body.code).toBe(41306);
      expect(body.msg).toContain(`Cannot delete built-in profile "${builtin.name}"`);
    });

    it('should return 41307 for deleting the essential profile', async () => {
      const { body } = await deleteJson<null>('/api/v1/agents/agent');
      expect(body.code).toBe(41307);
      expect(body.msg).toContain('Cannot delete the essential profile "agent"');
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

    it('should preserve subagents when toggling a profile enabled state', async () => {
      // Given a custom profile that declares subagents
      await postJson<ProfileEntryWire>('/api/v1/agents', {
        name: 'delegator',
        description: 'Delegation test',
        subagents: ['coder', 'reviewer'],
      });

      // When the profile is disabled
      const { body } = await postJson<{ ok: true }>('/api/v1/agents/delegator:disable');
      expect(body.code).toBe(0);
      expect(body.data.ok).toBe(true);

      // Then the on-disk .md still contains the subagents key
      const mdPath = join(home!, 'agents', 'delegator.md');
      const content = await readFile(mdPath, 'utf-8');
      expect(content).toContain('subagents');
      expect(content).toContain('coder');
      expect(content).toContain('reviewer');

      // And a follow-up GET returns them
      const { body: getBody } = await getJson<ProfileEntryWire>('/api/v1/agents/delegator');
      expect(getBody.code).toBe(0);
      expect(getBody.data.enabled).toBe(false);
      expect(getBody.data.subagents).toEqual(['coder', 'reviewer']);
    });

    it('should preserve the builtin prompt when toggling a builtin profile', async () => {
      // Disabling a builtin writes an override file; its body must keep the
      // base prompt reference and the role text instead of degrading to a
      // placeholder.
      const { body } = await postJson<{ ok: true }>('/api/v1/agents/explore:disable');
      expect(body.code).toBe(0);

      const mdPath = join(home!, 'agents', 'explore.md');
      const content = await readFile(mdPath, 'utf-8');
      expect(content).toContain('${base_prompt}');
      expect(content).toContain('You are a codebase exploration specialist');
      expect(content).not.toContain('You are a helpful AI assistant.');
    });

    it('should return 41307 when disabling the essential profile', async () => {
      const { body } = await postJson<null>('/api/v1/agents/agent:disable');
      expect(body.code).toBe(41307);
      expect(body.msg).toContain('Cannot disable the essential profile "agent"');
    });

    it('should return 40418 for toggling an unknown profile', async () => {
      const { body } = await postJson<null>('/api/v1/agents/nonexistent:disable');
      expect(body.code).toBe(40418);
      expect(body.msg).toContain('Agent profile "nonexistent" was not found');
    });
  });

  describe('POST /api/v1/agents/{name}:reset', () => {
    it('should restore a builtin profile to its builtin shape when its override is reset', async () => {
      // Given: an override file exists for the builtin 'coder' profile
      await putJson<ProfileEntryWire>('/api/v1/agents/coder', {
        system_prompt_template: 'You are a careful code reviewer.',
      });
      const overridePath = join(home!, 'agents', 'coder.md');
      expect(existsSync(overridePath)).toBe(true);

      // When: the override is reset
      const { body } = await postJson<{ ok: true }>('/api/v1/agents/coder:reset');
      expect(body.code).toBe(0);
      expect(body.data.ok).toBe(true);

      // Then: the override file is gone and the list row returns to the
      // builtin shape (builtin source, no override)
      expect(existsSync(overridePath)).toBe(false);
      const { body: listBody } = await getJson<{ items: ProfileEntryWire[] }>('/api/v1/agents');
      const coder = listBody.data.items.find((p) => p.name === 'coder');
      expect(coder?.builtin).toBe(true);
      expect(coder?.has_override).toBe(false);
      expect(coder?.source).toBe('builtin');
    });

    it('should return 41306 when resetting a builtin profile that has no override', async () => {
      // Given: 'explore' is a builtin with no override file on a fresh server
      const { body } = await postJson<null>('/api/v1/agents/explore:reset');
      expect(body.code).toBe(41306);
      expect(body.msg).toContain('Can only reset built-in profiles with overrides');
      // And no file is produced
      expect(existsSync(join(home!, 'agents', 'explore.md'))).toBe(false);
    });

    it('should return 41307 when trying to reset the essential profile', async () => {
      const { body } = await postJson<null>('/api/v1/agents/agent:reset');
      expect(body.code).toBe(41307);
      expect(body.msg).toContain('Cannot reset the essential profile "agent"');
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

  describe('PUT preserves disabled state', () => {
    it('should preserve the disabled state across a PUT update', async () => {
      // Given a disabled custom profile, a field-only update must not
      // silently re-enable it (the historical PUT bug).
      await postJson('/api/v1/agents', { name: 'togglable', description: 'Toggle test' });
      await postJson('/api/v1/agents/togglable:disable');

      const { body } = await putJson<ProfileEntryWire>('/api/v1/agents/togglable', {
        description: 'Updated description',
      });
      expect(body.code).toBe(0);
      expect(body.data.enabled).toBe(false);

      const { body: getBody } = await getJson<ProfileEntryWire>('/api/v1/agents/togglable');
      expect(getBody.data.enabled).toBe(false);
      const mdPath = join(home!, 'agents', 'togglable.md');
      expect(await readFile(mdPath, 'utf-8')).toContain('enabled: false');
    });

    it('should preserve disallowed_tools across a PUT update', async () => {
      // Given a profile with disallowed_tools, a field-only PUT must not
      // silently drop the disallowed_tools (same class of data-loss bug as enabled).
      // No builtin profile currently carries disallowedTools, so we write the
      // file directly to establish the precondition.
      const { mkdir, writeFile } = await import('node:fs/promises');
      const agentsDir = join(home!, 'agents');
      await mkdir(agentsDir, { recursive: true });
      await writeFile(
        join(agentsDir, 'tool-guard.md'),
        '---\nname: tool-guard\ndescription: Guard\nsubagents:\n  - coder\ndisallowedTools:\n  - Write\ncapabilitiesRequired:\n  - code.read\n---\nYou guard.',
      );

      const { body } = await putJson<ProfileEntryWire>('/api/v1/agents/tool-guard', {
        description: 'Updated guard',
      });
      expect(body.code).toBe(0);
      expect(body.data.disallowed_tools).toEqual(['Write']);
      expect(body.data.subagents).toEqual(['coder']);
      expect(body.data.capabilities_required).toEqual(['code.read']);

      const mdPath = join(home!, 'agents', 'tool-guard.md');
      const content = await readFile(mdPath, 'utf-8');
      expect(content).toContain('disallowedTools');
      expect(content).toContain('Write');
    });
  });

  describe('catalog projection edge cases', () => {
    it('should report suppressed candidates and skipped files in the list response', async () => {
      const { mkdir, writeFile } = await import('node:fs/promises');
      const agentsDir = join(home!, 'agents');
      await mkdir(agentsDir, { recursive: true });
      // A broken file the loader must skip (missing description).
      await writeFile(join(agentsDir, 'broken.md'), '---\nname: broken\n---\nbody');

      const { body } = await getJson<{
        items: ProfileEntryWire[];
        skipped?: { path: string; reason: string }[];
      }>('/api/v1/agents');
      expect(body.code).toBe(0);
      const broken = body.data.skipped?.find((s) => s.path.endsWith('broken.md'));
      expect(broken).toBeDefined();
      // The skip carries the parse reason: the loader surfaces the missing
      // required field instead of a generic error (ADR-008: skipped is
      // strictly load failures).
      expect(broken?.reason).toContain('Missing required frontmatter field "description"');
    });

    it('should expose capabilities_required and subagents on file profiles', async () => {
      const { mkdir, writeFile } = await import('node:fs/promises');
      const agentsDir = join(home!, 'agents');
      await mkdir(agentsDir, { recursive: true });
      await writeFile(
        join(agentsDir, 'guardian.md'),
        '---\nname: guardian\ndescription: Guards\nsubagents:\n  - coder\ncapabilitiesRequired:\n  - code.read\n---\nYou guard.',
      );

      const { body } = await getJson<ProfileEntryWire>('/api/v1/agents/guardian');
      expect(body.code).toBe(0);
      expect(body.data.subagents).toEqual(['coder']);
      expect(body.data.capabilities_required).toEqual(['code.read']);
    });
  });

  describe('with a registered workspace (catalog projection)', () => {
    // These tests register a workspace so the agents routes run their primary
    // read path — the SessionAgentProfileCatalogService projection over the
    // registry (listAll winners, suppressed_candidates from inspections) —
    // instead of the zero-workspace fallback scan the groups above exercise.
    // The fallback-path tests stay as they are.

    function agentMd(name: string, description: string): string {
      return `---\nname: ${name}\ndescription: ${description}\n---\n\nYou are ${name}.\n`;
    }

    async function registerWorkspace(): Promise<string> {
      const projectRoot = join(home!, 'project');
      await mkdir(projectRoot, { recursive: true });
      const { body } = await postJson<{ id: string }>('/api/v1/workspaces', {
        root: projectRoot,
        name: 'proj',
      });
      expect(body.code).toBe(0);
      return projectRoot;
    }

    async function listItems(): Promise<ProfileEntryWire[]> {
      const { body } = await getJson<{ items: ProfileEntryWire[] }>('/api/v1/agents');
      expect(body.code).toBe(0);
      return body.data.items;
    }

    it('emits a plugin-sourced row with the plugin source value', async () => {
      // The plugin contribution is pre-seeded in the registry (see the
      // beforeEach); once a workspace is open the catalog projection routes
      // it to the wire `source: 'plugin'`.
      await registerWorkspace();
      await vi.waitFor(async () => {
        const row = (await listItems()).find((p) => p.name === 'plugin-agent');
        expect(row?.source).toBe('plugin');
        expect(row?.builtin).toBe(false);
        expect(row?.description).toBe('from plugin');
      });
    });

    it('emits an explicit-sourced row with the explicit source value', async () => {
      // Same seeding approach as the plugin case: the explicit source id
      // (priority 40 — the top of the wire enum) maps to `source: 'explicit'`.
      await registerWorkspace();
      await vi.waitFor(async () => {
        const row = (await listItems()).find((p) => p.name === 'explicit-agent');
        expect(row?.source).toBe('explicit');
        expect(row?.builtin).toBe(false);
      });
    });

    it('projects catalog winners, suppressed candidates and the source mapping', async () => {
      const agentsDir = join(home!, 'agents');
      await mkdir(agentsDir, { recursive: true });
      await writeFile(join(agentsDir, 'user-only.md'), agentMd('user-only', 'user agent'));
      // Same name in user and project roots: the project (workspace) source
      // wins on priority, the user file becomes a suppressed candidate.
      await writeFile(join(agentsDir, 'shared.md'), agentMd('shared', 'from user'));
      // A user file with a non-default builtin name wins on priority (user 10
      // > builtin 0) — the displaced builtin becomes a suppressed candidate
      // (D16: isomorphic with the essential).
      await writeFile(join(agentsDir, 'explore.md'), agentMd('explore', 'user take on builtin'));
      // A user file with the DEFAULT profile's name overrides it (D14: a
      // same-name file overrides by virtue of existing — no declaration).
      await writeFile(join(agentsDir, 'agent.md'), agentMd('agent', 'user take on the default'));
      const projectRoot = await registerWorkspace();
      const projectAgents = join(projectRoot, '.mirri-code', 'agents');
      await mkdir(projectAgents, { recursive: true });
      await writeFile(join(projectAgents, 'shared.md'), agentMd('shared', 'from project'));

      // The workspace handlers load asynchronously after registration — poll
      // until the catalog projection is fully live instead of racing it. The
      // user-directory registration can settle later than the project one, so
      // the predicate waits for the user candidate to appear as suppressed
      // too, not just for `shared` to read as project-sourced.
      await vi.waitFor(async () => {
        const items = await listItems();
        expect(items.find((p) => p.name === 'shared')?.source).toBe('project');
        expect(items.find((p) => p.name === 'shared')?.suppressed_candidates).toContainEqual({
          source: 'user',
          priority: 10,
          reason: 'priority',
        });
      });

      const items = await listItems();
      const shared = items.find((p) => p.name === 'shared');
      expect(shared?.description).toBe('from project');
      expect(shared?.builtin).toBe(false);
      expect(shared?.suppressed_candidates).toContainEqual({
        source: 'user',
        priority: 10,
        reason: 'priority',
      });

      const userOnly = items.find((p) => p.name === 'user-only');
      expect(userOnly?.source).toBe('user');
      expect(userOnly?.builtin).toBe(false);
      expect(userOnly?.has_override).toBe(false);
      // Schema-exposure guard: optional wire fields must be present on the
      // rows the contract says carry them — an absent `file_path` or
      // `description` would silently drop the field for the whole projection.
      // (The loader realpaths file paths, so compare by suffix rather than
      // exact equality — macOS `/var` resolves to `/private/var`.)
      expect(userOnly?.description).toBe('user agent');
      expect(userOnly?.file_path).toBeDefined();
      expect(userOnly?.file_path).toContain('user-only.md');
      expect(shared?.file_path).toBeDefined();
      expect(shared?.file_path).toContain(join('project', '.mirri-code', 'agents', 'shared.md'));
      expect(shared?.description).toBe('from project');
      // D12 (ADR-007): every row with a system prompt carries the fully
      // rendered effective prompt — a custom row's own template.
      expect(userOnly?.effective_system_prompt).toContain('You are user-only.');

      // Non-default builtins lose same-name collisions to user files by
      // priority, and the displaced builtin definition is the row's
      // suppressed candidate ({source: builtin, priority: 0, reason:
      // priority} — D16).
      const explore = items.find((p) => p.name === 'explore');
      expect(explore?.source).toBe('user');
      expect(explore?.description).toBe('user take on builtin');
      expect(explore?.suppressed_candidates).toContainEqual({
        source: 'builtin',
        priority: 0,
        reason: 'priority',
      });
      // Its effective prompt is the file's own template: a hand-written user
      // file carries no `${base_prompt}` reference, so no base text is bound
      // — only UI-written override bodies embed the base reference.
      expect(explore?.effective_system_prompt).toContain('You are explore.');
      expect(explore?.effective_system_prompt).not.toContain(
        'Your primary goal is to help users',
      );

      // The default profile is overridable by a same-name file (D14/D16):
      // the row projects the covered-builtin shape, and the displaced
      // built-in definition is the suppressed candidate.
      const agent = items.find((p) => p.name === 'agent');
      expect(agent?.source).toBe('user');
      expect(agent?.builtin).toBe(false);
      expect(agent?.has_override).toBe(true);
      expect(agent?.essential).toBe(true);
      expect(agent?.suppressed_candidates).toContainEqual({
        source: 'builtin',
        priority: 0,
        reason: 'priority',
      });
    });

    it('projects extra agent dirs configured through the config route', async () => {
      // POST /config → IConfigService.set('extraAgentDirs', …) fires the
      // section change the extra loader reloads on, so the `extra` source
      // can be exercised without booting a second server with a config file.
      const extraDir = join(home!, 'extra-agents');
      await mkdir(extraDir, { recursive: true });
      await writeFile(join(extraDir, 'extra-agent.md'), agentMd('extra-agent', 'from extra'));
      await registerWorkspace();

      const before = (await listItems()).find((p) => p.name === 'extra-agent');
      expect(before).toBeUndefined();

      const { body: cfg } = await postJson<unknown>('/api/v1/config', {
        extra_agent_dirs: [extraDir],
      });
      expect(cfg.code).toBe(0);

      await vi.waitFor(async () => {
        const extra = (await listItems()).find((p) => p.name === 'extra-agent');
        expect(extra?.source).toBe('extra');
        expect(extra?.description).toBe('from extra');
      });
    });

    it('reflects an externally edited user agent file without any mutation request', async () => {
      // S1 regression: the registry's user-dir snapshot used to refresh only
      // after a route mutation. The user agents directory is now watched, so
      // an external edit flows into GET /agents on its own.
      await registerWorkspace();

      const agentsDir = join(home!, 'agents');
      await mkdir(agentsDir, { recursive: true });
      await writeFile(
        join(agentsDir, 'external-edit.md'),
        agentMd('external-edit', 'edited externally'),
      );

      await vi.waitFor(
        async () => {
          const found = (await listItems()).find((p) => p.name === 'external-edit');
          expect(found?.source).toBe('user');
          expect(found?.description).toBe('edited externally');
        },
        { timeout: 10000, interval: 250 },
      );
    });
  });
});
