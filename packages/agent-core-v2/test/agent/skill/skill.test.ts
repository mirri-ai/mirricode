import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { SyncDescriptor } from '#/_base/di/descriptors';
import { DisposableStore } from '#/_base/di/lifecycle';
import { createServices, type TestInstantiationService } from '#/_base/di/test';
import type { ContextMessage } from '#/agent/contextMemory/types';
import { IAgentPromptService } from '#/agent/prompt/prompt';
import { IAgentSkillService } from '#/agent/skill/skill';
import { IAgentScopeContext, makeAgentScopeContext } from '#/agent/scopeContext/scopeContext';
import { InMemorySkillCatalog } from '#/app/skillCatalog/registry';
import { ISessionSkillCatalog } from '#/session/sessionSkillCatalog/skillCatalog';
import { ISessionContext } from '#/session/sessionContext/sessionContext';
import { AgentSkillService } from '#/agent/skill/skillService';
import {
  MAX_SKILL_QUERY_DEPTH,
  NestedSkillTooDeepError,
  SkillToolInputSchema,
} from '#/agent/tools/skill/skill';
import { SkillTool } from '#/agent/tools/skill/skillTool';
import { ITelemetryService } from '#/app/telemetry/telemetry';
import { IAgentToolRegistryService } from '#/agent/toolRegistry/toolRegistry';
import type { Turn } from '#/agent/loop/loop';
import { executeTool } from '../../tools/fixtures/execute-tool';
import { stubSkill } from '../../app/skillCatalog/stubs';
import { registerTestAgentWireServices } from '../../wire/stubs';

const COMMIT_SKILL = stubSkill('commit', {
  description: 'commit changes',
  path: '/skills/commit/SKILL.md',
  dir: '/skills/commit',
  content: '# Commit',
  metadata: {},
  source: 'user',
});

function stubSessionContext(sessionId = 'test-session'): ISessionContext {
  return {
    _serviceBrand: undefined,
    sessionId,
    workspaceId: 'test-workspace',
    sessionDir: '/sessions/test',
    metaScope: 'sessions/test',
    cwd: '/sessions/test',
    scope: (subKey?: string) => (subKey ? `sessions/test/${subKey}` : 'sessions/test'),
  };
}

function fakeTurn(): Turn {
  return {
    id: 1,
    signal: new AbortController().signal,
    ready: Promise.resolve(),
    result: Promise.resolve({ type: 'completed', steps: 0, truncated: false }),
    cancel: () => true,
  };
}

describe('AgentSkillService', () => {
  let disposables: DisposableStore;
  let ix: TestInstantiationService;
  let prompted: ContextMessage[];
  let skills: InMemorySkillCatalog;
  let lastMaterialize: (() => Promise<ContextMessage>) | undefined;

  beforeEach(() => {
    disposables = new DisposableStore();
    prompted = [];
    lastMaterialize = undefined;
    ix = createServices(disposables, {
      additionalServices: (reg) => {
        reg.definePartialInstance(IAgentPromptService, {
          enqueue: (input: { message: ContextMessage; materialize?: () => Promise<ContextMessage> }) => {
            prompted.push(input.message);
            lastMaterialize = input.materialize;
            return Promise.resolve({ launched: Promise.resolve(fakeTurn()) } as never);
          },
          retry: () => Promise.resolve(undefined),
          clear: () => {},
        });
        registerTestAgentWireServices(reg, 'wire/skill-test');
        reg.definePartialInstance(ITelemetryService, { track: () => {}, track2: () => {} });
        reg.definePartialInstance(IAgentToolRegistryService, {
          register: () => ({ dispose: () => {} }),
        });
        reg.defineInstance(ISessionContext, stubSessionContext());
        reg.defineInstance(IAgentScopeContext, makeAgentScopeContext({ agentId: 'main', agentScope: '' }));
      },
    });
    skills = new InMemorySkillCatalog();
    skills.register(COMMIT_SKILL);
    const skillCatalog: ISessionSkillCatalog = {
      _serviceBrand: undefined,
      catalog: skills,
      ready: Promise.resolve(),
      onDidChange: () => ({ dispose: () => {} }),
      load: async () => {},
      reload: async () => {},
    };
    ix.set(ISessionSkillCatalog, skillCatalog);
    ix.set(IAgentSkillService, new SyncDescriptor(AgentSkillService));
  });
  afterEach(() => disposables.dispose());

  it('activate prompts with the rendered skill for a known skill', async () => {
    const svc = ix.get(IAgentSkillService);
    const turn = await svc.activate({ name: 'commit' });

    expect(turn).toBeDefined();
    expect(prompted).toHaveLength(1);
    expect(prompted[0]!.role).toBe('user');
    expect(prompted[0]!.origin).toMatchObject({
      kind: 'skill_activation',
      skillName: 'commit',
    });
  });

  it('activate throws for an unknown skill', async () => {
    const svc = ix.get(IAgentSkillService);
    await expect(svc.activate({ name: 'missing' })).rejects.toThrow(/not found/i);
  });

  it('activate waits for the catalog to be ready before resolving', async () => {
    let resolveReady!: () => void;
    const ready = new Promise<void>((resolve) => {
      resolveReady = resolve;
    });
    const skills = new InMemorySkillCatalog();
    skills.register(COMMIT_SKILL);
    ix.set(ISessionSkillCatalog, {
      _serviceBrand: undefined,
      catalog: skills,
      ready,
      onDidChange: () => ({ dispose: () => {} }),
      load: async () => {},
      reload: async () => {},
    } satisfies ISessionSkillCatalog);
    ix.set(IAgentSkillService, new SyncDescriptor(AgentSkillService));

    const svc = ix.get(IAgentSkillService);
    let finished = false;
    const activation = svc.activate({ name: 'commit' }).then(() => {
      finished = true;
    });

    await Promise.resolve();
    expect(finished).toBe(false);

    resolveReady();
    await activation;

    expect(finished).toBe(true);
    expect(prompted).toHaveLength(1);
  });

  it('enqueueDeferred rejects an unknown skill synchronously without queueing a prompt', async () => {
    const svc = ix.get(IAgentSkillService);
    await expect(svc.enqueueDeferred({ name: 'missing' })).rejects.toThrow(/not found/i);
    expect(prompted).toHaveLength(0);
    expect(lastMaterialize).toBeUndefined();
  });

  it('enqueueDeferred rejects a skill that is not user-activatable without queueing a prompt', async () => {
    skills.register(stubSkill('model-only', { metadata: { type: 'reference' } }));
    const svc = ix.get(IAgentSkillService);
    await expect(svc.enqueueDeferred({ name: 'model-only' })).rejects.toThrow(/cannot be activated/i);
    expect(prompted).toHaveLength(0);
  });

  it('activate resolves a plugin-qualified skill name and echoes the canonical bare name', async () => {
    skills.register(
      stubSkill('deploy', {
        description: 'deploy the app',
        path: '/plugins/superpowers/deploy/SKILL.md',
        dir: '/plugins/superpowers/deploy',
        content: '# Deploy',
        metadata: {},
        source: 'extra',
        plugin: { id: 'superpowers' },
      }),
    );
    const svc = ix.get(IAgentSkillService);

    const turn = await svc.activate({ name: 'superpowers:deploy' });

    expect(turn).toBeDefined();
    expect(prompted).toHaveLength(1);
    expect(prompted[0]!.origin).toMatchObject({
      kind: 'skill_activation',
      skillName: 'deploy',
    });
    expect((prompted[0]!.content[0] as { text: string }).text).toBe('/deploy');
  });

  it('activate keeps the user-activation type gate for plugin-qualified skill names', async () => {
    skills.register(
      stubSkill('ref-only', {
        metadata: { type: 'reference' },
        source: 'extra',
        plugin: { id: 'superpowers' },
      }),
    );
    const svc = ix.get(IAgentSkillService);

    await expect(svc.activate({ name: 'superpowers:ref-only' })).rejects.toThrow(
      /cannot be activated/i,
    );
    expect(prompted).toHaveLength(0);
  });

  it('activate throws for an unresolvable qualified reference echoing the raw input', async () => {
    const svc = ix.get(IAgentSkillService);

    await expect(svc.activate({ name: 'unknown-plugin:commit' })).rejects.toThrow(
      'Skill "unknown-plugin:commit" was not found',
    );
  });

  it('enqueueDeferred parks the raw intent and defers rendering until the queued prompt materializes', async () => {
    const svc = ix.get(IAgentSkillService);
    const handle = await svc.enqueueDeferred({ name: 'commit', args: 'fix' });

    // Only the raw user-slash intent is queued — nothing rendered yet.
    expect(handle).toBeDefined();
    expect(prompted).toHaveLength(1);
    expect(prompted[0]!.content).toEqual([{ type: 'text', text: '/commit fix' }]);
    expect(prompted[0]!.origin).toMatchObject({
      kind: 'skill_activation',
      skillName: 'commit',
      skillArgs: 'fix',
    });
    expect(lastMaterialize).toBeDefined();

    // Materializing (dequeue time) renders the skill body and keeps the intent origin.
    const materialized = await lastMaterialize!();
    expect(materialized.content.some(
      (part) => part.type === 'text' && (part as { text: string }).text.includes('# Commit'),
    )).toBe(true);
    expect(materialized.origin).toMatchObject({ kind: 'skill_activation', skillName: 'commit' });
  });

  // ---- Inline reference injection (US3.6 / ADR-004) -------------------------

  it('injects one activation wrapper per unique skill before the body when a message carries inline skill references', async () => {
    skills.register(stubSkill('review', { path: '/skills/review/SKILL.md', content: '# Review' }));
    skills.register(stubSkill('lint', { path: '/skills/lint/SKILL.md', content: '# Lint' }));
    const svc = ix.get(IAgentSkillService);

    const text = 'please run [$review](/skills/review/SKILL.md) then [$lint](/skills/lint/SKILL.md)';
    const analysis = await svc.analyzeInlineReferences(text, 'msg-1');

    expect(analysis.pendingCommands).toEqual([]);
    expect(analysis.skipped).toEqual([]);
    expect(analysis.body).toBe('please run [review] then [lint]');
    expect(analysis.wrappers).toHaveLength(2);
    expect(analysis.wrappers[0]).toMatchObject({ skillName: 'review', target: '/skills/review/SKILL.md' });
    expect(analysis.wrappers[1]).toMatchObject({ skillName: 'lint', target: '/skills/lint/SKILL.md' });
    // ADR-006: activated skills are reported for the with_skills record —
    // one entry per successful (deduped, non-command) activation.
    expect(analysis.activatedSkills).toEqual([
      { name: 'review', target: '/skills/review/SKILL.md', trigger: 'user-inline' },
      { name: 'lint', target: '/skills/lint/SKILL.md', trigger: 'user-inline' },
    ]);

    // Materializing each wrapper renders the user-inline activation prompt and
    // records the activation with the derived idempotent activation id.
    const first = await analysis.wrappers[0]!.materialize();
    expect(first.origin).toMatchObject({
      kind: 'skill_activation',
      skillName: 'review',
      trigger: 'user-inline',
      activationId: 'msg-1#0',
    });
    expect((first.content[0] as { text: string }).text).toContain('# Review');
    expect((first.content[0] as { text: string }).text).toContain('trigger="user-inline"');
    const second = await analysis.wrappers[1]!.materialize();
    expect(second.origin).toMatchObject({ activationId: 'msg-1#1' });
  });

  it('collapses repeated references to the same skill path into one wrapper with one activation record', async () => {
    const svc = ix.get(IAgentSkillService);
    const text = '[$commit](/skills/commit/SKILL.md) and again [$commit](/skills/commit/SKILL.md)';
    const analysis = await svc.analyzeInlineReferences(text, 'msg-2');

    expect(analysis.wrappers).toHaveLength(1);
    const wrapper = await analysis.wrappers[0]!.materialize();
    expect(wrapper.origin).toMatchObject({ kind: 'skill_activation', skillName: 'commit', activationId: 'msg-2#0' });
  });

  it('keeps failing references as plain text and reports them, while valid ones still activate', async () => {
    skills.register(stubSkill('review', { path: '/skills/review/SKILL.md', content: '# Review' }));
    const svc = ix.get(IAgentSkillService);
    const text = 'do [$review](/skills/review/SKILL.md) and [$ghost](/gone/SKILL.md) please';
    const analysis = await svc.analyzeInlineReferences(text, 'msg-3');

    expect(analysis.wrappers).toHaveLength(1);
    expect(analysis.wrappers[0]).toMatchObject({ skillName: 'review' });
    expect(analysis.skipped).toEqual([
      expect.objectContaining({ name: 'ghost', target: '/gone/SKILL.md' }),
    ]);
    // The failed reference stays as the original link text in the body.
    expect(analysis.body).toBe('do [review] and [$ghost](/gone/SKILL.md) please');
  });

  it('surfaces command references as pending commands without injecting or recording activations', async () => {
    const svc = ix.get(IAgentSkillService);
    const text = 'run [$commit](/skills/commit/SKILL.md) and [$clear](command:/clear) now';
    const analysis = await svc.analyzeInlineReferences(text, 'msg-4');

    expect(analysis.wrappers).toHaveLength(1);
    expect(analysis.pendingCommands).toEqual([
      { name: '/clear', display: 'clear', target: 'command:/clear' },
    ]);
    expect(analysis.body).toBe('run [commit] and [$clear](command:/clear) now');
  });

  it('keeps the user-activation type gate for inline references (reference-only skills fail, message still proceeds)', async () => {
    skills.register(stubSkill('ref-only', { path: '/skills/ref-only/SKILL.md', metadata: { type: 'reference' } }));
    const svc = ix.get(IAgentSkillService);
    const text = 'use [$ref-only](/skills/ref-only/SKILL.md) heavily';
    const analysis = await svc.analyzeInlineReferences(text, 'msg-5');

    expect(analysis.wrappers).toEqual([]);
    expect(analysis.skipped).toEqual([
      expect.objectContaining({ name: 'ref-only', reason: 'skill_not_activatable' }),
    ]);
    expect(analysis.body).toBe('use [$ref-only](/skills/ref-only/SKILL.md) heavily');
  });

  it('returns no wrappers, no commands, and the unchanged text for a plain message', async () => {
    const svc = ix.get(IAgentSkillService);
    const analysis = await svc.analyzeInlineReferences('just a plain [link](https://example.com) message', 'msg-6');

    expect(analysis.wrappers).toEqual([]);
    expect(analysis.pendingCommands).toEqual([]);
    expect(analysis.skipped).toEqual([]);
    expect(analysis.body).toBe('just a plain [link](https://example.com) message');
  });
});

describe('SkillTool', () => {
  let disposables: DisposableStore;
  let ix: TestInstantiationService;
  let prompted: ContextMessage[];
  let skills: InMemorySkillCatalog;

  beforeEach(() => {
    disposables = new DisposableStore();
    prompted = [];
    ix = createServices(disposables, {
      additionalServices: (reg) => {
        reg.definePartialInstance(IAgentPromptService, {
          enqueue: ({ message }: { message: ContextMessage }) => { prompted.push(message); return Promise.resolve({ launched: Promise.resolve(fakeTurn()) } as never); },
          retry: () => Promise.resolve(undefined),
          clear: () => {},
        });
        registerTestAgentWireServices(reg, 'wire/skill-test');
        reg.definePartialInstance(ITelemetryService, { track: () => {}, track2: () => {} });
        reg.definePartialInstance(IAgentToolRegistryService, {
          register: () => ({ dispose: () => {} }),
        });
        reg.defineInstance(ISessionContext, stubSessionContext());
        reg.defineInstance(IAgentScopeContext, makeAgentScopeContext({ agentId: 'main', agentScope: '' }));
      },
    });
    skills = new InMemorySkillCatalog();
    skills.register(COMMIT_SKILL);
    ix.set(ISessionSkillCatalog, {
      _serviceBrand: undefined,
      catalog: skills,
      ready: Promise.resolve(),
      onDidChange: () => ({ dispose: () => {} }),
      load: async () => {},
      reload: async () => {},
    } satisfies ISessionSkillCatalog);
    ix.set(IAgentSkillService, new SyncDescriptor(AgentSkillService));
  });
  afterEach(() => disposables.dispose());

  function toolContext(args: { readonly skill: string; readonly args?: string }) {
    return {
      turnId: 0,
      toolCallId: 'call_skill',
      args,
      signal: new AbortController().signal,
    };
  }

  function stubSkillService(): IAgentSkillService {
    return {
      _serviceBrand: undefined,
      activate: () => Promise.reject(new Error('not implemented')),
      enqueueDeferred: () => Promise.reject(new Error('not implemented')),
      recordModelToolActivation: () => {},
      analyzeInlineReferences: () =>
        Promise.reject(new Error('not implemented')),
    };
  }

  function makeTool(ix: TestInstantiationService, depth?: number): SkillTool {
    const tool = new SkillTool(
      ix.get(ISessionSkillCatalog),
      stubSkillService(),
      stubSessionContext(),
    );
    return depth === undefined ? tool : tool.withInitialQueryDepth(depth);
  }

  it('exposes metadata and schema for model-invoked skills', () => {
    const tool = makeTool(ix);

    expect(tool.name).toBe('Skill');
    expect(tool.description).toContain('Invoke a registered skill');
    expect(tool.description).toContain('kimi-skill-loaded');
    expect(tool.description).toContain('with the same `args`');
    expect(tool.parameters).toMatchObject({
      type: 'object',
      required: ['skill'],
      additionalProperties: false,
      properties: {
        skill: expect.objectContaining({
          type: 'string',
          description: expect.stringMatching(/skill listing/i),
        }),
        args: expect.objectContaining({
          type: 'string',
          description: expect.stringMatching(/argument/i),
        }),
      },
    });
    expect(SkillToolInputSchema.safeParse({ skill: 'commit' }).success).toBe(true);
    expect(SkillToolInputSchema.safeParse({ skill: 'commit', args: '-m fix' }).success).toBe(true);
    expect(SkillToolInputSchema.safeParse({}).success).toBe(false);
  });

  it('returns a tool error when the skill is unknown', async () => {
    const result = await executeTool(
      makeTool(ix),
      toolContext({ skill: 'missing' }),
    );

    expect(result).toMatchObject({
      isError: true,
      output: 'Skill "missing" not found in the current skill listing.',
    });
  });

  it('rejects skills that disable model invocation', async () => {
    skills.register(stubSkill('private', { metadata: { disableModelInvocation: true } }));

    const result = await executeTool(
      makeTool(ix),
      toolContext({ skill: 'private' }),
    );

    expect(result).toMatchObject({
      isError: true,
      output: 'Skill "private" can only be triggered by the user (model invocation is disabled).',
    });
  });

  it('rejects non-inline skill types in the current v1 runtime', async () => {
    skills.register(stubSkill('flow-only', { metadata: { type: 'flow' } }));

    const result = await executeTool(
      makeTool(ix),
      toolContext({ skill: 'flow-only' }),
    );

    expect(result).toMatchObject({
      isError: true,
      output: 'Skill "flow-only" is not an inline skill and cannot be invoked by the model in v1.',
    });
  });

  it('loads inline skills through the model-tool wrapper without exposing the body in output', async () => {
    const result = await executeTool(
      makeTool(ix),
      toolContext({ skill: 'commit', args: 'src/app.ts' }),
    );

    expect(result).toMatchObject({
      output: 'Skill "commit" loaded inline. Follow its instructions.',
    });
    expect(result.output).not.toContain('# Commit');
    expect(prompted).toHaveLength(0);
    expect(result.delivery?.kind).toBe('steer');
    expect(result.delivery?.message.origin).toMatchObject({
      kind: 'skill_activation',
      skillName: 'commit',
      trigger: 'model-tool',
    });
    expect(result.delivery?.message.content[0]).toMatchObject({
      type: 'text',
      text: expect.stringContaining(
        '<kimi-skill-loaded name="commit" trigger="model-tool" source="user" dir="/skills/commit" args="src/app.ts">',
      ),
    });
    expect(result.delivery?.message.content[0]).toMatchObject({
      type: 'text',
      text: expect.stringContaining('ARGUMENTS: src/app.ts'),
    });
  });

  it('loads a plugin-qualified skill and echoes the canonical bare name downstream', async () => {
    skills.register(
      stubSkill('deploy', {
        description: 'deploy the app',
        path: '/plugins/superpowers/deploy/SKILL.md',
        dir: '/plugins/superpowers/deploy',
        content: '# Deploy',
        metadata: {},
        source: 'extra',
        plugin: { id: 'superpowers' },
      }),
    );

    const result = await executeTool(
      makeTool(ix),
      toolContext({ skill: 'superpowers:deploy', args: 'prod' }),
    );

    expect(result).toMatchObject({
      output: 'Skill "deploy" loaded inline. Follow its instructions.',
    });
    expect(result.output).not.toContain('superpowers:deploy');
    expect(prompted).toHaveLength(0);
    expect(result.delivery?.kind).toBe('steer');
    expect(result.delivery?.message.origin).toMatchObject({
      kind: 'skill_activation',
      skillName: 'deploy',
      trigger: 'model-tool',
    });
    expect(result.delivery?.message.content[0]).toMatchObject({
      type: 'text',
      text: expect.stringContaining(
        '<kimi-skill-loaded name="deploy" trigger="model-tool" source="extra" dir="/plugins/superpowers/deploy" args="prod">',
      ),
    });
    expect(result.delivery?.message.content[0]).toMatchObject({
      type: 'text',
      text: expect.stringContaining('# Deploy'),
    });
  });

  it('injects the same content for a plugin-qualified call as for the bare-name call', async () => {
    skills.register(
      stubSkill('deploy', {
        description: 'deploy the app',
        content: '# Deploy',
        metadata: {},
        source: 'extra',
        plugin: { id: 'superpowers', instructions: 'Use the plugin instructions.' },
      }),
    );

    const bare = await executeTool(
      makeTool(ix),
      toolContext({ skill: 'deploy', args: 'prod' }),
    );
    const qualified = await executeTool(
      makeTool(ix),
      toolContext({ skill: 'superpowers:deploy', args: 'prod' }),
    );

    expect(qualified.output).toBe(bare.output);
    expect(qualified.delivery?.message.content).toEqual(bare.delivery?.message.content);
    expect(qualified.delivery?.message.origin).toMatchObject({
      skillName: 'deploy',
    });
  });

  it('returns the not-found error echoing the raw qualified reference when resolution fails', async () => {
    const result = await executeTool(
      makeTool(ix),
      toolContext({ skill: 'unknown-plugin:commit' }),
    );

    expect(result).toMatchObject({
      isError: true,
      output: 'Skill "unknown-plugin:commit" not found in the current skill listing.',
    });
  });

  it('reports the user-only gating error, not not-found, for a qualified skill that disables model invocation', async () => {
    skills.register(
      stubSkill('private', {
        metadata: { disableModelInvocation: true },
        source: 'extra',
        plugin: { id: 'superpowers' },
      }),
    );

    const result = await executeTool(
      makeTool(ix),
      toolContext({ skill: 'superpowers:private' }),
    );

    expect(result).toMatchObject({
      isError: true,
      output: 'Skill "superpowers:private" can only be triggered by the user (model invocation is disabled).',
    });
  });

  it('honors initialQueryDepth as an alias for queryDepth', async () => {
    const nested = await executeTool(
      makeTool(ix, 2),
      toolContext({ skill: 'commit' }),
    );
    const root = await executeTool(
      makeTool(ix, 0),
      toolContext({ skill: 'commit' }),
    );

    expect(prompted).toHaveLength(0);
    expect(nested.delivery?.message.origin).toMatchObject({
      kind: 'skill_activation',
      trigger: 'nested-skill',
    });
    expect(root.delivery?.message.origin).toMatchObject({
      kind: 'skill_activation',
      trigger: 'model-tool',
    });
  });

  it('throws a structured recursion error when nested skill invocation is too deep', async () => {
    await expect(
      executeTool(
        makeTool(ix, MAX_SKILL_QUERY_DEPTH),
        toolContext({ skill: 'commit' }),
      ),
    ).rejects.toBeInstanceOf(NestedSkillTooDeepError);
    expect(prompted).toHaveLength(0);
  });
});
