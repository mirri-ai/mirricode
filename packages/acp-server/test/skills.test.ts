import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, describe, expect, it } from 'vitest';

import type { SkillSummary } from '@mirri-ai/agent-core-v2';

import { ACP_BUILTIN_SLASH_COMMANDS } from '../src/builtin-commands';
import { buildAcpSkillSlashCommands } from '../src/slash';
import { createTestClient, type TestClient } from './_helpers/acpClient';
import { writeFakeModelConfig } from './_helpers/fakeModelConfig';
import { createScriptedProvider, type ScriptedProvider } from './_helpers/scriptedProvider';

interface AvailableCommand {
  readonly name: string;
  readonly description?: string;
  readonly input?: { readonly hint?: string } | null;
}

/** The update payload's command list, typed loosely for assertions. */
function commandsOf(notification: unknown): readonly AvailableCommand[] {
  const params = (notification as { params?: { update?: { availableCommands?: unknown } } })
    .params;
  return (params?.update?.availableCommands ?? []) as readonly AvailableCommand[];
}

function skill(name: string, overrides: Partial<SkillSummary> = {}): SkillSummary {
  return {
    name,
    description: `desc for ${name}`,
    path: `/skills/${name}/SKILL.md`,
    source: 'project',
    ...overrides,
  };
}

describe('buildAcpSkillSlashCommands', () => {
  it('prefixes non-builtin skills with `skill:` and keeps builtin/sub-skill names bare', () => {
    const { commands, commandMap } = buildAcpSkillSlashCommands([
      skill('workspace-one'),
      skill('engine-one', { source: 'builtin' }),
      skill('nested', { isSubSkill: true }),
    ]);

    expect(commands.map((command) => command.name)).toEqual([
      'engine-one',
      'nested',
      'skill:workspace-one',
    ]);
    expect(commandMap.get('skill:workspace-one')).toBe('workspace-one');
    expect(commandMap.get('engine-one')).toBe('engine-one');
  });

  it('filters out skills the user cannot activate', () => {
    const { commands } = buildAcpSkillSlashCommands([
      skill('reference-only', { type: 'reference' }),
      skill('flow-one', { type: 'flow' }),
      skill('inline-one'),
    ]);

    expect(commands.map((command) => command.name)).toEqual(['skill:flow-one', 'skill:inline-one']);
  });

  it('drops skills whose command name collides with an ACP builtin', () => {
    const { commands, commandMap } = buildAcpSkillSlashCommands([
      skill('compact', { source: 'builtin' }),
      skill('help'),
    ]);

    expect(commands.map((command) => command.name)).toEqual(['skill:help']);
    expect(commandMap.has('compact')).toBe(false);
  });
});

describe('acp-server skills / available commands', () => {
  let homeDir: string | undefined;
  let client: TestClient | undefined;
  let scripted: ScriptedProvider | undefined;

  afterEach(async () => {
    if (client !== undefined) {
      await client.close();
      client = undefined;
    }
    if (homeDir !== undefined) {
      await rm(homeDir, { recursive: true, force: true });
      homeDir = undefined;
    }
  });

  async function boot(): Promise<TestClient> {
    homeDir = await mkdtemp(join(tmpdir(), 'acp-skills-'));
    client = await createTestClient({ homeDir });
    await client.send('initialize', { protocolVersion: 1, clientCapabilities: {} });
    return client;
  }

  /**
   * Boot with the scripted LLM and a project skill fixture at
   * `<cwd>/.mirri-code/skills/acp-fixture/SKILL.md` (the engine's project
   * skill discovery root; the temp cwd has no `.git`, so it IS the project
   * root). The engine resolves and can ACTIVATE this skill; only the session
   * skill LISTING is missing from the klient facade (KLIENT-GAP).
   */
  async function bootWithFixtureSkill(): Promise<TestClient> {
    homeDir = await mkdtemp(join(tmpdir(), 'acp-skills-turn-'));
    await writeFakeModelConfig(homeDir);
    await mkdir(join(homeDir, '.mirri-code', 'skills', 'acp-fixture'), { recursive: true });
    await writeFile(
      join(homeDir, '.mirri-code', 'skills', 'acp-fixture', 'SKILL.md'),
      '---\nname: acp-fixture\ndescription: ACP fixture skill\n---\n\n' +
        '# ACP Fixture\n\nAlways answer with the word FIXTURE.\n',
    );
    scripted = createScriptedProvider();
    client = await createTestClient({ homeDir, extraSeeds: [scripted.seed] });
    await client.send('initialize', { protocolVersion: 1, clientCapabilities: {} });
    return client;
  }

  async function newSession(c: TestClient): Promise<string> {
    const created = (await c.send('session/new', { cwd: homeDir, mcpServers: [] })) as {
      sessionId: string;
    };
    await c.waitForSessionUpdate('available_commands_update', 10_000);
    return created.sessionId;
  }

  it('session/new pushes the builtin (and host) commands only — no engine-skill entries (KLIENT-GAP)', async () => {
    const c = await boot();
    await c.send('session/new', { cwd: homeDir, mcpServers: [] });

    const notification = await c.waitForSessionUpdate('available_commands_update', 10_000);
    const commandsOf = (m: unknown) =>
      (
        (m as { params?: { update?: { availableCommands?: { name: string }[] } } }).params?.update
          ?.availableCommands ?? []
      ).map((command) => command.name);
    const commands = commandsOf(notification);
    // The six ACP builtins (executed locally by the host) are the whole
    // palette: skills are not advertised (the facade has no skill listing).
    expect(commands).toEqual(ACP_BUILTIN_SLASH_COMMANDS.map((command) => command.name));
    const compact = commands.find((command) => command === 'compact');
    expect(compact).toBe('compact');
  }, 30_000);

  it('pushes available_commands_update only after the session/new response settles', async () => {
    const c = await boot();
    await c.send('session/new', { cwd: homeDir, mcpServers: [] });
    await c.waitForSessionUpdate('available_commands_update', 10_000);

    // Clients register the session when the response lands and silently drop
    // `session/update` notifications that arrive earlier (Zed), so the slash
    // commands push must come after the `session/new` response on the wire.
    const responseIndex = c.received.findIndex(
      (m) => (m.result as { sessionId?: string } | undefined)?.sessionId !== undefined,
    );
    const notificationIndex = c.received.findIndex((m) => {
      const update = (m.params as { update?: { sessionUpdate?: string } } | undefined)?.update;
      return m.method === 'session/update' && update?.sessionUpdate === 'available_commands_update';
    });
    expect(responseIndex).toBeGreaterThanOrEqual(0);
    expect(notificationIndex).toBeGreaterThan(responseIndex);
  }, 30_000);

  it('does not advertise a workspace skill in the palette (facade has no skill listing — KLIENT-GAP)', async () => {
    const c = await bootWithFixtureSkill();
    await c.send('session/new', { cwd: homeDir, mcpServers: [] });

    const notification = await c.waitForSessionUpdate('available_commands_update', 10_000);
    const commands = commandsOf(notification);
    expect(commands).not.toContain('skill:acp-fixture');
  }, 30_000);

  it('/skill:acp-fixture activates the skill and drives a normal turn', async () => {
    // The kimi reference projected the `skill:` entry points from the session
    // skill listing; without that listing (KLIENT-GAP) the host supplies the
    // `skill:`-prefixed alias explicitly, exactly like any other alias.
    const c = await bootWithFixtureSkill();
    await c.close();
    client = await createTestClient({
      homeDir: homeDir!,
      extraSeeds: [scripted!.seed],
      slashCommands: {
        commands: [{ name: 'skill:acp-fixture', description: 'ACP fixture skill' }],
        skillCommandMap: new Map([['skill:acp-fixture', 'acp-fixture']]),
      },
    });
    await client.send('initialize', { protocolVersion: 1, clientCapabilities: {} });
    scripted!.mockNextText('FIXTURE');
    const sessionId = await newSession(client);

    const promptPromise = client.send('session/prompt', {
      sessionId,
      prompt: [{ type: 'text', text: '/skill:acp-fixture some args' }],
    });

    // The engine renders the skill prompt and drives it as a normal turn, so
    // the streamed reply crosses the wire exactly like a plain prompt.
    const chunk = await client.waitForSessionUpdate('agent_message_chunk', 10_000);
    expect(
      (chunk.params as { update?: { content?: { text?: string } } }).update?.content?.text,
    ).toContain('FIXTURE');
    const result = (await promptPromise) as { stopReason: string };
    expect(result.stopReason).toBe('end_turn');
    expect(scripted!.callCount()).toBe(1);

    // The model received the rendered skill activation (content + args), not
    // the raw slash text.
    const history = JSON.stringify(scripted!.callHistory()[0]);
    expect(history).toContain('kimi-skill-loaded');
    expect(history).toContain('acp-fixture');
    expect(history).toContain('Always answer with the word FIXTURE');
    expect(history).toContain('ARGUMENTS: some args');
    expect(history).not.toContain('/skill:acp-fixture');
  }, 30_000);

  it('a host-declared bare skill command activates a builtin skill without a `skill:` prefix', async () => {
    // No `session.skills` listing (KLIENT-GAP), so the engine's built-in
    // skills never project themselves into the map — the host must declare
    // the bare alias, exactly like any other skill alias.
    const c = await bootWithFixtureSkill();
    await c.close();
    scripted!.mockNextText('goal noted');
    client = await createTestClient({
      homeDir: homeDir!,
      extraSeeds: [scripted!.seed],
      slashCommands: {
        commands: [{ name: 'write-goal', description: 'Write a session goal' }],
        skillCommandMap: new Map([['write-goal', 'write-goal']]),
      },
    });
    await client.send('initialize', { protocolVersion: 1, clientCapabilities: {} });
    const sessionId = await newSession(client);

    const result = (await client.send('session/prompt', {
      sessionId,
      prompt: [{ type: 'text', text: '/write-goal ship it' }],
    })) as { stopReason: string };
    expect(result.stopReason).toBe('end_turn');
    expect(scripted!.callCount()).toBe(1);
    const history = JSON.stringify(scripted!.callHistory()[0]);
    expect(history).toContain('kimi-skill-loaded');
    expect(history).toContain('write-goal');
  }, 30_000);

  it('an unknown slash command is answered locally and never reaches the model', async () => {
    const c = await bootWithFixtureSkill();
    scripted!.mockNextText('echoed');
    const sessionId = await newSession(c);

    const before = c.sessionUpdates().length;
    const result = (await c.send('session/prompt', {
      sessionId,
      prompt: [{ type: 'text', text: '/no-such-skill hi' }],
    })) as { stopReason: string };
    expect(result.stopReason).toBe('end_turn');

    // No LLM turn was launched — the notice is a local agent_message_chunk.
    expect(scripted!.callCount()).toBe(0);
    type Update = { sessionUpdate?: string; content?: { text?: string } };
    const chunk = c
      .sessionUpdates()
      .slice(before)
      .map((m) => (m.params as { update?: Update }).update)
      .find((u) => u?.sessionUpdate === 'agent_message_chunk');
    expect(chunk?.content?.text).toBe(
      'Unknown ACP command: /no-such-skill. Use /help to see available commands.',
    );
  }, 30_000);

  it('/help lists the host skill aliases but no engine skill listing (session.skills KLIENT-GAP)', async () => {
    const c = await bootWithFixtureSkill();
    await c.close();
    const alias = 'fixture-alias';
    client = await createTestClient({
      homeDir: homeDir!,
      extraSeeds: [scripted!.seed],
      slashCommands: {
        commands: [{ name: alias, description: 'Activate the fixture skill' }],
        skillCommandMap: new Map([[alias, 'acp-fixture']]),
      },
    });
    await client.send('initialize', { protocolVersion: 1, clientCapabilities: {} });
    const sessionId = await newSession(client);

    await client.send('session/prompt', {
      sessionId,
      prompt: [{ type: 'text', text: '/help' }],
    });

    const help = client
      .sessionUpdates()
      .map((m) =>
        (m.params as { update?: { sessionUpdate?: string; content?: { text?: string } } }).update,
      )
      .find((update) => update?.sessionUpdate === 'agent_message_chunk')?.content?.text;
    expect(help).toContain('/compact — Compact the conversation context');
    // The host alias reached the palette…
    expect(help).toContain(`/${alias} — Activate the fixture skill`);
    // …while a project skill is NOT listed (the facade has no session skill
    // listing to project from).
    expect(help).not.toContain('/skill:acp-fixture');
  }, 30_000);

  it('supports a host-provided skill alias in the advertised palette', async () => {
    const c = await bootWithFixtureSkill();
    const alias = 'fixture-alias';
    const aliasCommand = { name: alias, description: 'Activate the fixture skill' };
    await c.close();
    const aliasClient = await createTestClient({
      homeDir: homeDir!,
      extraSeeds: [scripted!.seed],
      slashCommands: {
        commands: [aliasCommand],
        skillCommandMap: new Map([[alias, 'acp-fixture']]),
      },
    });
    client = aliasClient;
    await aliasClient.send('initialize', { protocolVersion: 1, clientCapabilities: {} });
    scripted!.mockNextText('ALIAS');
    const sessionId = await newSession(aliasClient);

    const result = (await aliasClient.send('session/prompt', {
      sessionId,
      prompt: [{ type: 'text', text: '/fixture-alias extra args' }],
    })) as { stopReason: string };
    expect(result.stopReason).toBe('end_turn');
    expect(scripted!.callCount()).toBe(1);
    expect(JSON.stringify(scripted!.callHistory()[0])).toContain('acp-fixture');
  }, 30_000);
});
