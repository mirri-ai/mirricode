/**
 * Standalone tests for the B3-L7 Hook Engine port:
 * - updatedInput / additionalContext in HookResult
 * - runner-level parsing and propagation
 * - collectAdditionalContext / injectHookAdditionalContext
 * - BeforeToolExecuteEventImpl.rewriteArgs()
 * - BeforeToolExecuteEmitter carrying rewrittenArgs
 * - blockDecision with updatedInput
 * - AgentExternalHooksService.runPreToolUse() integration
 */

import { describe, expect, it } from 'vitest';

import { SyncDescriptor } from '#/_base/di/descriptors';
import { DisposableStore } from '#/_base/di/lifecycle';
import {
  createServices,
  type TestInstantiationService,
} from '#/_base/di/test';
import { Emitter, Event } from '#/_base/event';
import { computeUndoCut } from '#/agent/contextMemory/contextOps';
import {
  IAgentContextMemoryService,
  type ContextCompactionInput,
  type ContextCompactionResult,
} from '#/agent/contextMemory/contextMemory';
import { buildContextCompactionShape } from '#/agent/contextMemory/compactionHandoff';
import type { ContextMessage } from '#/agent/contextMemory/types';
import { IAgentExternalHooksService } from '#/agent/externalHooks/externalHooks';
import { AgentExternalHooksService } from '#/agent/externalHooks/externalHooksService';
import { HOOKS_SECTION } from '#/agent/externalHooks/configSection';
import { IAgentFullCompactionService } from '#/agent/fullCompaction/fullCompaction';
import { IAgentLoopService, type AfterStepContext } from '#/agent/loop/loop';
import { IAgentPermissionGate } from '#/agent/permissionGate/permissionGate';
import { IAgentPromptService } from '#/agent/prompt/prompt';
import { IAgentTaskService } from '#/agent/task/task';
import { IAgentToolExecutorService } from '#/agent/toolExecutor/toolExecutor';
import type { BeforeToolExecuteEvent, ToolDidExecuteContext, WillExecuteToolEvent } from '#/agent/toolExecutor/toolHooks';
import { BeforeToolExecuteEventImpl, BeforeToolExecuteEmitter } from '#/agent/toolExecutor/beforeToolExecuteEvent';
import type { ResolvedToolExecutionHookContext } from '#/agent/toolExecutor/toolHooks';
import type { RunnableToolExecution } from '#/tool/toolContract';
import { runHook } from '#/agent/externalHooks/runner';
import { HostProcessService } from '#/os/backends/node-local/hostProcessService';
import {
  collectAdditionalContext,
  injectHookAdditionalContext,
} from '#/agent/externalHooks/user-prompt';
import type { HookResult } from '#/agent/externalHooks/types';
import { blockDecision } from '#/app/externalHooksRunner/runner';
import { IExternalHooksRunnerService } from '#/app/externalHooksRunner/externalHooksRunner';
import { IBootstrapService } from '#/app/bootstrap/bootstrap';
import { IConfigService } from '#/app/config/config';
import { IEventBus } from '#/app/event/eventBus';
import { EventBusService } from '#/app/event/eventBusService';
import { IFlagService } from '#/app/flag/flag';
import { FlagService } from '#/app/flag/flagService';
import { IPluginService } from '#/app/plugin/plugin';
import { IHostProcessService } from '#/os/interface/hostProcess';
import { ISessionContext } from '#/session/sessionContext/sessionContext';
import { IAgentStateService } from '#/agent/state/agentState';
import { IAppStateService } from '#/app/state/appState';
import { IWorkspaceStateService } from '#/workspace/state/workspaceState';
import { ISessionStateService } from '#/session/state/sessionState';
import { AgentStateService } from '#/agent/state/agentStateService';
import { AppStateService } from '#/app/state/appStateService';
import { WorkspaceStateService } from '#/workspace/state/workspaceStateService';
import { SessionStateService } from '#/session/state/sessionStateService';
import { HOOK_COMMAND_REWRITE_FLAG_ID } from '#/agent/hookCommandRewrite/flag';
import { emptyUsage } from '#/kosong/contract/usage';
import { createHooks } from '#/hooks';
import { OrderedHookSlot } from '#/hooks';
import { toDisposable } from '#/_base/di/lifecycle';
import { resolveHostArgs } from '#/app/bootstrap/bootstrap';

const hostProcess = new HostProcessService();

function nodeCommand(source: string): string {
  return `node -e ${JSON.stringify(source.replaceAll(/\s*\n\s*/g, ' '))}`;
}

// ---------------------------------------------------------------------------
// Runner: updatedInput / additionalContext parsing
// ---------------------------------------------------------------------------

describe('runHook: updatedInput and additionalContext parsing', () => {
  it('should parse updatedInput from hookSpecificOutput on allow result', async () => {
    const result = await runHook(
      hostProcess,
      nodeCommand(
        'process.stdout.write(JSON.stringify({ hookSpecificOutput: { updatedInput: { command: "rg" } } }));',
      ),
      { tool_name: 'Bash' },
      { timeout: 5 },
    );

    expect(result.action).toBe('allow');
    expect(result.structuredOutput).toBe(true);
    expect(result.updatedInput).toEqual({ command: 'rg' });
  });

  it('should parse additionalContext from hookSpecificOutput on allow result', async () => {
    const result = await runHook(
      hostProcess,
      nodeCommand(
        'process.stdout.write(JSON.stringify({ hookSpecificOutput: { additionalContext: "extra info" } }));',
      ),
      { tool_name: 'Bash' },
      { timeout: 5 },
    );

    expect(result.action).toBe('allow');
    expect(result.structuredOutput).toBe(true);
    expect(result.additionalContext).toBe('extra info');
  });

  it('should parse both updatedInput and additionalContext together', async () => {
    const result = await runHook(
      hostProcess,
      nodeCommand(
        'process.stdout.write(JSON.stringify({ hookSpecificOutput: { updatedInput: { command: "rg --pattern" }, additionalContext: "rewritten by rtk" } }));',
      ),
      { tool_name: 'Bash' },
      { timeout: 5 },
    );

    expect(result.action).toBe('allow');
    expect(result.updatedInput).toEqual({ command: 'rg --pattern' });
    expect(result.additionalContext).toBe('rewritten by rtk');
  });

  it('should discard updatedInput on deny-decision block but keep additionalContext', async () => {
    const result = await runHook(
      hostProcess,
      nodeCommand(
        'process.stdout.write(JSON.stringify({ hookSpecificOutput: { permissionDecision: "deny", permissionDecisionReason: "use rg instead", updatedInput: { command: "rg" }, additionalContext: "blocked context" } }));',
      ),
      { tool_name: 'Bash' },
      { timeout: 5 },
    );

    expect(result.action).toBe('block');
    expect(result.reason).toBe('use rg instead');
    expect(result.updatedInput).toBeUndefined();
    expect(result.additionalContext).toBe('blocked context');
  });

  it('should discard updatedInput on exit-code-2 block without additionalContext', async () => {
    const result = await runHook(
      hostProcess,
      nodeCommand('process.stderr.write("forbidden\\n"); process.exit(2);'),
      { tool_name: 'Bash' },
      { timeout: 5 },
    );

    expect(result.action).toBe('block');
    expect(result.updatedInput).toBeUndefined();
    expect(result.additionalContext).toBeUndefined();
  });

  it('should return undefined updatedInput when hookSpecificOutput has no updatedInput', async () => {
    const result = await runHook(
      hostProcess,
      nodeCommand('process.stdout.write("{}");'),
      {},
      { timeout: 5 },
    );

    expect(result.action).toBe('allow');
    expect(result.updatedInput).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// collectAdditionalContext
// ---------------------------------------------------------------------------

describe('collectAdditionalContext', () => {
  it('should return undefined when results are empty', () => {
    expect(collectAdditionalContext([])).toBeUndefined();
    expect(collectAdditionalContext(undefined)).toBeUndefined();
  });

  it('should return undefined when no results have additionalContext', () => {
    const results: HookResult[] = [
      { action: 'allow', structuredOutput: true },
    ];
    expect(collectAdditionalContext(results)).toBeUndefined();
  });

  it('should collect additionalContext from allowed, valid results', () => {
    const results: HookResult[] = [
      { action: 'allow', additionalContext: 'ctx1', exitCode: 0, structuredOutput: true },
      { action: 'allow', additionalContext: 'ctx2', exitCode: 0, structuredOutput: true },
    ];
    expect(collectAdditionalContext(results)).toBe('ctx1\nctx2');
  });

  it('should skip block results by default', () => {
    const results: HookResult[] = [
      { action: 'block', reason: 'no', additionalContext: 'blocked ctx' },
      { action: 'allow', additionalContext: 'allowed ctx', exitCode: 0 },
    ];
    expect(collectAdditionalContext(results)).toBe('allowed ctx');
  });

  it('should include block results when includeBlock is true', () => {
    const results: HookResult[] = [
      { action: 'block', reason: 'no', additionalContext: 'blocked ctx', exitCode: 0, structuredOutput: true },
      { action: 'allow', additionalContext: 'allowed ctx', exitCode: 0 },
    ];
    expect(collectAdditionalContext(results, { includeBlock: true })).toBe('blocked ctx\nallowed ctx');
  });

  it('should include block results with includeBlock but still skip invalid block results', () => {
    const results: HookResult[] = [
      { action: 'block', reason: 'no', additionalContext: 'valid blocked', exitCode: 0, structuredOutput: true },
      { action: 'block', reason: 'no', additionalContext: 'timed out blocked', timedOut: true },
    ];
    expect(collectAdditionalContext(results, { includeBlock: true })).toBe('valid blocked');
  });

  it('should skip timed-out results', () => {
    const results: HookResult[] = [
      { action: 'allow', additionalContext: 'timeout ctx', timedOut: true },
    ];
    expect(collectAdditionalContext(results)).toBeUndefined();
  });

  it('should skip non-zero exit code results', () => {
    const results: HookResult[] = [
      { action: 'allow', additionalContext: 'error ctx', exitCode: 1 },
    ];
    expect(collectAdditionalContext(results)).toBeUndefined();
  });

  it('should trim whitespace from additionalContext values', () => {
    const results: HookResult[] = [
      { action: 'allow', additionalContext: '  trimmed  ', exitCode: 0 },
    ];
    expect(collectAdditionalContext(results)).toBe('trimmed');
  });
});

// ---------------------------------------------------------------------------
// injectHookAdditionalContext
// ---------------------------------------------------------------------------

describe('injectHookAdditionalContext', () => {
  it('should be a no-op when no results have additionalContext', () => {
    const appended: unknown[] = [];
    const context = {
      append: (...messages: unknown[]) => { appended.push(...messages); },
    };
    injectHookAdditionalContext(context, [], 'PreToolUse');
    expect(appended).toHaveLength(0);
  });

  it('should inject a user message with hook_additional_context origin', () => {
    const appended: unknown[] = [];
    const context = {
      append: (...messages: unknown[]) => { appended.push(...messages); },
    };
    const results: HookResult[] = [
      { action: 'allow', additionalContext: 'extra info', exitCode: 0 },
    ];
    injectHookAdditionalContext(context, results, 'PreToolUse');
    expect(appended).toHaveLength(1);
    const msg = appended[0] as Record<string, unknown>;
    expect(msg['role']).toBe('user');
    expect(msg['origin']).toEqual({ kind: 'hook_additional_context', event: 'PreToolUse' });
  });

  it('should merge multiple additionalContext values with newline', () => {
    const appended: unknown[] = [];
    const context = {
      append: (...messages: unknown[]) => { appended.push(...messages); },
    };
    const results: HookResult[] = [
      { action: 'allow', additionalContext: 'line1', exitCode: 0 },
      { action: 'allow', additionalContext: 'line2', exitCode: 0 },
    ];
    injectHookAdditionalContext(context, results, 'PostToolUse');
    expect(appended).toHaveLength(1);
    const msg = appended[0] as Record<string, unknown>;
    const content = msg['content'] as Array<{ type: string; text: string }>;
    expect(content[0]!.text).toBe('line1\nline2');
  });

  it('should inject block additionalContext when includeBlock is true', () => {
    const appended: unknown[] = [];
    const context = {
      append: (...messages: unknown[]) => { appended.push(...messages); },
    };
    const results: HookResult[] = [
      { action: 'block', reason: 'no', additionalContext: 'blocked ctx', exitCode: 0, structuredOutput: true },
    ];
    injectHookAdditionalContext(context, results, 'PreToolUse', { includeBlock: true });
    expect(appended).toHaveLength(1);
    const msg = appended[0] as Record<string, unknown>;
    expect(msg['origin']).toEqual({ kind: 'hook_additional_context', event: 'PreToolUse' });
  });

  it('should not inject block additionalContext when includeBlock is not set', () => {
    const appended: unknown[] = [];
    const context = {
      append: (...messages: unknown[]) => { appended.push(...messages); },
    };
    const results: HookResult[] = [
      { action: 'block', reason: 'no', additionalContext: 'blocked ctx', exitCode: 0, structuredOutput: true },
    ];
    injectHookAdditionalContext(context, results, 'PreToolUse');
    expect(appended).toHaveLength(0);
  });
});

// ---------------------------------------------------------------------------
// BeforeToolExecuteEventImpl.rewriteArgs
// ---------------------------------------------------------------------------

function makeStubContext(args: unknown = {}): ResolvedToolExecutionHookContext {
  return {
    turnId: 0,
    signal: new AbortController().signal,
    toolCall: { id: 'tc_1', type: 'function', name: 'Bash', arguments: '{}' },
    toolCalls: [],
    tool: undefined,
    args,
    execution: { execute: async () => ({ output: 'ok' }) } as unknown as RunnableToolExecution,
  };
}

describe('BeforeToolExecuteEventImpl.rewriteArgs', () => {
  it('should record rewritten args when called', () => {
    const event = new BeforeToolExecuteEventImpl(makeStubContext({ command: 'ls' }));
    event.rewriteArgs({ command: 'rg' });
    expect(event.rewrittenArgs).toEqual({ command: 'rg' });
  });

  it('should keep the first rewriteArgs call (first hook wins)', () => {
    const event = new BeforeToolExecuteEventImpl(makeStubContext({ command: 'ls' }));
    event.rewriteArgs({ command: 'rg' });
    event.rewriteArgs({ command: 'grep' });
    expect(event.rewrittenArgs).toEqual({ command: 'rg' });
  });

  it('should throw when called after closeRegistration', () => {
    const event = new BeforeToolExecuteEventImpl(makeStubContext({ command: 'ls' }));
    event.closeRegistration();
    expect(() => event.rewriteArgs({ command: 'rg' })).toThrow('rewriteArgs');
  });

  it('should return undefined rewrittenArgs when not called', () => {
    const event = new BeforeToolExecuteEventImpl(makeStubContext({ command: 'ls' }));
    expect(event.rewrittenArgs).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// BeforeToolExecuteEmitter: rewrittenArgs in decision
// ---------------------------------------------------------------------------

describe('BeforeToolExecuteEmitter: rewrittenArgs in decision', () => {
  it('should return rewrittenArgs in the decision when a listener calls rewriteArgs', async () => {
    const emitter = new BeforeToolExecuteEmitter();
    emitter.event((event) => {
      event.rewriteArgs({ command: 'rg' });
    });

    const decision = await emitter.fireBeforeExecute(makeStubContext({ command: 'ls' }));
    expect(decision?.rewrittenArgs).toEqual({ command: 'rg' });
  });

  it('should return undefined decision when no listener rewrites or vetoes', async () => {
    const emitter = new BeforeToolExecuteEmitter();
    emitter.event(() => {});

    const decision = await emitter.fireBeforeExecute(makeStubContext({ command: 'ls' }));
    expect(decision).toBeUndefined();
  });

  it('should not return rewrittenArgs when vetoed', async () => {
    const emitter = new BeforeToolExecuteEmitter();
    emitter.event((event) => {
      event.rewriteArgs({ command: 'rg' });
      event.veto({ output: 'blocked', isError: true });
    });

    const decision = await emitter.fireBeforeExecute(makeStubContext({ command: 'ls' }));
    expect(decision?.veto).toBeDefined();
    expect(decision?.rewrittenArgs).toBeUndefined();
  });

  it('should return rewrittenArgs when allow is called after rewrite', async () => {
    const emitter = new BeforeToolExecuteEmitter();
    emitter.event((event) => {
      event.rewriteArgs({ command: 'rg' });
      event.allow();
    });

    const decision = await emitter.fireBeforeExecute(makeStubContext({ command: 'ls' }));
    expect(decision?.rewrittenArgs).toEqual({ command: 'rg' });
  });

  it('should carry rewrittenArgs from waitUntil factory', async () => {
    const emitter = new BeforeToolExecuteEmitter();
    emitter.event((event) => {
      event.waitUntil(async () => ({
        rewrittenArgs: { command: 'rg --hidden' },
      }));
    });

    const decision = await emitter.fireBeforeExecute(makeStubContext({ command: 'ls' }));
    expect(decision?.rewrittenArgs).toEqual({ command: 'rg --hidden' });
  });

  it('should combine executionMetadata and rewrittenArgs in decision', async () => {
    const emitter = new BeforeToolExecuteEmitter();
    emitter.event((event) => {
      event.rewriteArgs({ command: 'rg' });
      event.pass({ trace: 'hook-rewrite' });
    });

    const decision = await emitter.fireBeforeExecute(makeStubContext({ command: 'ls' }));
    expect(decision?.rewrittenArgs).toEqual({ command: 'rg' });
    expect(decision?.executionMetadata).toEqual({ trace: 'hook-rewrite' });
  });
});

// ---------------------------------------------------------------------------
// blockDecision: updatedInput handling
// ---------------------------------------------------------------------------

describe('blockDecision: updatedInput handling', () => {
  it('should return block decision when a result has action=block', () => {
    const results: HookResult[] = [
      { action: 'allow', updatedInput: { command: 'rg' }, exitCode: 0 },
      { action: 'block', reason: 'forbidden' },
    ];
    const decision = blockDecision('PreToolUse', results);
    expect(decision).toEqual({ block: true, reason: 'forbidden' });
  });

  it('should return undefined when all results are allow', () => {
    const results: HookResult[] = [
      { action: 'allow', updatedInput: { command: 'rg' }, exitCode: 0 },
    ];
    const decision = blockDecision('PreToolUse', results);
    expect(decision).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// AgentExternalHooksService.runPreToolUse() integration
// ---------------------------------------------------------------------------

/**
 * Stub context memory that records all appended messages.
 */
function stubContextMemory(): IAgentContextMemoryService & {
  readonly messages: readonly ContextMessage[];
} {
  const messages: ContextMessage[] = [];
  return {
    _serviceBrand: undefined,
    get: () => [...messages],
    append: (...inserted) => {
      messages.push(...inserted);
    },
    appendLoopEvent: () => {},
    clear: () => {
      messages.splice(0);
    },
    undo: (count) => {
      const cut = computeUndoCut(messages, count);
      if (cut.cutIndex >= 0 && cut.removedCount >= count) {
        messages.splice(cut.cutIndex);
      }
      return cut;
    },
    applyCompaction: (input: ContextCompactionInput): ContextCompactionResult => {
      const shape = buildContextCompactionShape(messages, input);
      messages.splice(0, messages.length, ...shape.messages);
      const { messages: _messages, ...result } = shape;
      void _messages;
      return result;
    },
    messages,
  };
}

function stubSessionContext(): ISessionContext {
  return {
    _serviceBrand: undefined,
    sessionId: 'session-1',
    workspaceId: 'workspace-1',
    sessionDir: '/tmp/session-1',
    metaScope: 'sessions/workspace-1/session-1',
    cwd: '/tmp',
    scope: (subKey?: string) =>
      subKey === undefined || subKey === ''
        ? 'sessions/workspace-1/session-1'
        : `sessions/workspace-1/session-1/${subKey}`,
  };
}

function stubBootstrap(): IBootstrapService {
  const homeDir = `/tmp/mirri-test-${process.pid}-${Date.now()}`;
  return {
    _serviceBrand: undefined,
    platform: 'linux',
    arch: 'x64',
    cwd: '/tmp',
    osHomeDir: '/home/test',
    homeDir,
    configPath: `${homeDir}/config.toml`,
    configKey: 'config.toml',
    clientIdentity: { productName: 'test', version: '0.0.0', platform: 'test' },
    args: resolveHostArgs({}),
    sessionsDir: `${homeDir}/sessions`,
    blobsDir: `${homeDir}/blobs`,
    storeDir: `${homeDir}/store`,
    cacheDir: `${homeDir}/cache`,
    logsDir: `${homeDir}/logs`,
    getEnv: () => undefined,
    scope: (name) => name,
  };
}

function stubHookRunner(partial: Partial<Pick<IExternalHooksRunnerService, 'trigger' | 'triggerBlock' | 'fireAndForgetTrigger'>>): IExternalHooksRunnerService {
  return {
    _serviceBrand: undefined,
    trigger: partial.trigger ?? (async () => []),
    triggerBlock: partial.triggerBlock ?? (async () => undefined),
    fireAndForgetTrigger: partial.fireAndForgetTrigger ?? (async () => []),
  };
}

/**
 * Stub tool executor with a real BeforeToolExecuteEmitter so listeners
 * registered by AgentExternalHooksService actually fire.
 */
function stubToolExecutorWithEmitter(): IAgentToolExecutorService & {
  readonly beforeEmitter: BeforeToolExecuteEmitter;
} {
  const beforeEmitter = new BeforeToolExecuteEmitter();
  return {
    _serviceBrand: undefined,
    execute: async function* () {},
    onBeforeExecuteTool: beforeEmitter.event,
    onWillExecuteTool: Event.None as Event<WillExecuteToolEvent>,
    hooks: { onDidExecuteTool: new OrderedHookSlot<ToolDidExecuteContext>() },
    recordDupType: () => {},
    registerToolCallGuard: () => ({ dispose() {} }),
    registerUnavailableToolDescriber: () => ({ dispose() {} }),
    registerMissingToolDescriber: () => ({ dispose() {} }),
    beforeEmitter,
  };
}

function stubLoopWithHooks(): IAgentLoopService {
  const hooks = createHooks(['onWillBeginStep', 'onDidFinishStep']) as IAgentLoopService['hooks'];
  return {
    _serviceBrand: undefined,
    hooks,
    enqueue: () => ({
      assigned: new Promise<never>(() => {}),
      abort: (_reason?: unknown) => false,
    }),
    run: async () => ({ type: 'completed', steps: 0, truncated: false }),
    status: () => ({ state: 'idle', activeTurnId: undefined, pendingTurnIds: [], hasPendingRequests: false }),
    cancel: (_turnId?: number, _reason?: unknown) => false,
    tryAcquireQuiescence: () => ({ dispose() {} }),
    hasPendingRequests: () => false,
    registerLoopErrorHandler: () => ({ dispose() {} }),
    settled: () => Promise.resolve(),
  };
}

/**
 * Create a FlagService stub where specific flags can be forced on/off.
 */
function stubFlagService(overrides: Record<string, boolean> = {}): IFlagService {
  return {
    _serviceBrand: undefined,
    registry: { _serviceBrand: undefined, register: () => toDisposable(() => {}), list: () => [], get: () => undefined },
    enabled: (id) => overrides[id] ?? false,
    snapshot: () => ({}),
    enabledIds: () => [],
    explain: () => undefined,
    explainAll: () => [],
    setConfigOverrides: () => {},
  };
}

/**
 * Create the AgentExternalHooksService with stubbed dependencies and return
 * the service plus the stubs for assertion.
 */
function createServiceWithStubs(options: {
  readonly hookRunner: IExternalHooksRunnerService;
  readonly flagOverrides?: Record<string, boolean>;
}): {
  service: AgentExternalHooksService;
  context: ReturnType<typeof stubContextMemory>;
  toolExecutor: ReturnType<typeof stubToolExecutorWithEmitter>;
  disposables: DisposableStore;
  ix: TestInstantiationService;
} {
  const disposables = new DisposableStore();
  const context = stubContextMemory();
  const toolExecutor = stubToolExecutorWithEmitter();
  const flags = stubFlagService(options.flagOverrides);

  const app = new AppStateService();
  const workspace = new WorkspaceStateService(app);
  const session = new SessionStateService(workspace);
  const agent = new AgentStateService(session);

  let ix: TestInstantiationService | undefined;
  try {
    ix = createServices(disposables, {
      strict: true,
      additionalServices: (reg) => {
        reg.defineInstance(IAppStateService, app);
        reg.defineInstance(IWorkspaceStateService, workspace);
        reg.defineInstance(ISessionStateService, session);
        reg.defineInstance(IAgentStateService, agent);
        reg.defineInstance(IBootstrapService, stubBootstrap());
        reg.defineInstance(ISessionContext, stubSessionContext());
        reg.definePartialInstance(IConfigService, {});
        reg.definePartialInstance(IPluginService, {});
        reg.defineInstance(IAgentContextMemoryService, context);
        reg.defineInstance(IAgentLoopService, stubLoopWithHooks());
        reg.define(IEventBus, EventBusService);
        reg.definePartialInstance(IAgentPromptService, {
          hooks: createHooks(['onBeforeSubmitPrompt']),
        });
        reg.defineInstance(IAgentToolExecutorService, toolExecutor);
        reg.definePartialInstance(IAgentPermissionGate, {});
        reg.definePartialInstance(IAgentFullCompactionService, {
          hooks: createHooks(['onWillCompact']),
        });
        reg.definePartialInstance(IAgentTaskService, {});
        reg.defineInstance(IFlagService, flags);
      },
    });
    ix.set(IExternalHooksRunnerService, options.hookRunner);
    ix.set(IAgentExternalHooksService, new SyncDescriptor(AgentExternalHooksService));
    const service = ix.get(IAgentExternalHooksService) as AgentExternalHooksService;
    return { service, context, toolExecutor, disposables, ix };
  } catch (error) {
    ix?.dispose();
    disposables.dispose();
    throw error;
  }
}

function makeStubToolContext(args: unknown = {}): ResolvedToolExecutionHookContext {
  return {
    turnId: 0,
    signal: new AbortController().signal,
    toolCall: { id: 'tc_1', type: 'function', name: 'Bash', arguments: '{}' },
    toolCalls: [],
    tool: undefined,
    args,
    execution: { execute: async () => ({ output: 'ok' }) } as unknown as RunnableToolExecution,
  };
}

describe('AgentExternalHooksService.runPreToolUse: updatedInput rewriting', () => {
  it('should rewrite tool args when hook returns updatedInput and flag is enabled', async () => {
    const hookRunner = stubHookRunner({
      trigger: async () => [
        { action: 'allow', updatedInput: { command: 'rg --hidden' }, exitCode: 0, structuredOutput: true },
      ],
    });

    const { toolExecutor, disposables, ix } = createServiceWithStubs({
      hookRunner,
      flagOverrides: { [HOOK_COMMAND_REWRITE_FLAG_ID]: true },
    });

    try {
      const decision = await toolExecutor.beforeEmitter.fireBeforeExecute(
        makeStubToolContext({ command: 'ls' }),
      );

      expect(decision?.rewrittenArgs).toEqual({ command: 'rg --hidden' });
    } finally {
      ix.dispose();
      disposables.dispose();
    }
  });

  it('should not rewrite tool args when hook returns updatedInput but flag is disabled', async () => {
    const hookRunner = stubHookRunner({
      trigger: async () => [
        { action: 'allow', updatedInput: { command: 'rg --hidden' }, exitCode: 0, structuredOutput: true },
      ],
    });

    const { toolExecutor, disposables, ix } = createServiceWithStubs({
      hookRunner,
      flagOverrides: { [HOOK_COMMAND_REWRITE_FLAG_ID]: false },
    });

    try {
      const decision = await toolExecutor.beforeEmitter.fireBeforeExecute(
        makeStubToolContext({ command: 'ls' }),
      );

      // No rewrite — the flag is off
      expect(decision?.rewrittenArgs).toBeUndefined();
    } finally {
      ix.dispose();
      disposables.dispose();
    }
  });

  it('should not rewrite tool args when no hook returns updatedInput', async () => {
    const hookRunner = stubHookRunner({
      trigger: async () => [
        { action: 'allow', exitCode: 0 },
      ],
    });

    const { toolExecutor, disposables, ix } = createServiceWithStubs({
      hookRunner,
      flagOverrides: { [HOOK_COMMAND_REWRITE_FLAG_ID]: true },
    });

    try {
      const decision = await toolExecutor.beforeEmitter.fireBeforeExecute(
        makeStubToolContext({ command: 'ls' }),
      );

      expect(decision?.rewrittenArgs).toBeUndefined();
    } finally {
      ix.dispose();
      disposables.dispose();
    }
  });

  it('should use the first hook result with updatedInput when multiple hooks return it', async () => {
    const hookRunner = stubHookRunner({
      trigger: async () => [
        { action: 'allow', updatedInput: { command: 'rg' }, exitCode: 0, structuredOutput: true },
        { action: 'allow', updatedInput: { command: 'grep' }, exitCode: 0, structuredOutput: true },
      ],
    });

    const { toolExecutor, disposables, ix } = createServiceWithStubs({
      hookRunner,
      flagOverrides: { [HOOK_COMMAND_REWRITE_FLAG_ID]: true },
    });

    try {
      const decision = await toolExecutor.beforeEmitter.fireBeforeExecute(
        makeStubToolContext({ command: 'ls' }),
      );

      // First hook with updatedInput wins
      expect(decision?.rewrittenArgs).toEqual({ command: 'rg' });
    } finally {
      ix.dispose();
      disposables.dispose();
    }
  });
});

describe('AgentExternalHooksService.runPreToolUse: block decision', () => {
  it('should veto tool execution when hook returns block decision', async () => {
    const hookRunner = stubHookRunner({
      trigger: async () => [
        { action: 'block', reason: 'dangerous command' },
      ],
    });

    const { toolExecutor, disposables, ix } = createServiceWithStubs({
      hookRunner,
    });

    try {
      const decision = await toolExecutor.beforeEmitter.fireBeforeExecute(
        makeStubToolContext({ command: 'rm -rf /' }),
      );

      expect(decision?.veto).toBeDefined();
      expect(decision?.veto?.isError).toBe(true);
    } finally {
      ix.dispose();
      disposables.dispose();
    }
  });

  it('should not veto tool execution when all hooks allow', async () => {
    const hookRunner = stubHookRunner({
      trigger: async () => [
        { action: 'allow', exitCode: 0 },
      ],
    });

    const { toolExecutor, disposables, ix } = createServiceWithStubs({
      hookRunner,
    });

    try {
      const decision = await toolExecutor.beforeEmitter.fireBeforeExecute(
        makeStubToolContext({ command: 'ls' }),
      );

      expect(decision?.veto).toBeUndefined();
    } finally {
      ix.dispose();
      disposables.dispose();
    }
  });
});

describe('AgentExternalHooksService.runPreToolUse: additionalContext injection', () => {
  it('should inject additionalContext into context memory when hook returns it', async () => {
    const hookRunner = stubHookRunner({
      trigger: async () => [
        { action: 'allow', additionalContext: 'use rg instead of grep', exitCode: 0 },
      ],
    });

    const { context, toolExecutor, disposables, ix } = createServiceWithStubs({
      hookRunner,
    });

    try {
      await toolExecutor.beforeEmitter.fireBeforeExecute(
        makeStubToolContext({ command: 'grep' }),
      );

      const contextMsg = context.messages.find(
        (msg) => msg.origin !== undefined,
      );
      expect(contextMsg).toBeDefined();
      expect(contextMsg!.role).toBe('user');
      expect(contextMsg!.origin).toEqual({ kind: 'hook_additional_context', event: 'PreToolUse' });
      const content = contextMsg!.content as Array<{ type: string; text: string }>;
      expect(content[0]!.text).toBe('use rg instead of grep');
    } finally {
      ix.dispose();
      disposables.dispose();
    }
  });

  it('should inject additionalContext even when hook blocks via deny decision', async () => {
    const hookRunner = stubHookRunner({
      trigger: async () => [
        {
          action: 'block',
          reason: 'forbidden',
          additionalContext: 'blocked but context still injected',
          exitCode: 0,
          structuredOutput: true,
        },
      ],
    });

    const { context, toolExecutor, disposables, ix } = createServiceWithStubs({
      hookRunner,
    });

    try {
      await toolExecutor.beforeEmitter.fireBeforeExecute(
        makeStubToolContext({ command: 'rm' }),
      );

      const contextMsg = context.messages.find(
        (msg) => msg.origin !== undefined,
      );
      // Block path also calls injectHookAdditionalContext with includeBlock
      expect(contextMsg).toBeDefined();
      expect(contextMsg!.origin).toEqual({ kind: 'hook_additional_context', event: 'PreToolUse' });
    } finally {
      ix.dispose();
      disposables.dispose();
    }
  });

  it('should not inject any context when no hook returns additionalContext', async () => {
    const hookRunner = stubHookRunner({
      trigger: async () => [
        { action: 'allow', exitCode: 0 },
      ],
    });

    const { context, toolExecutor, disposables, ix } = createServiceWithStubs({
      hookRunner,
    });

    try {
      await toolExecutor.beforeEmitter.fireBeforeExecute(
        makeStubToolContext({ command: 'ls' }),
      );

      expect(context.messages).toHaveLength(0);
    } finally {
      ix.dispose();
      disposables.dispose();
    }
  });
});

describe('AgentExternalHooksService.runPreToolUse: combined updatedInput + additionalContext + flag', () => {
  it('should rewrite args and inject context together when flag is enabled', async () => {
    const hookRunner = stubHookRunner({
      trigger: async () => [
        {
          action: 'allow',
          updatedInput: { command: 'rg --hidden' },
          additionalContext: 'rewritten for safety',
          exitCode: 0,
          structuredOutput: true,
        },
      ],
    });

    const { context, toolExecutor, disposables, ix } = createServiceWithStubs({
      hookRunner,
      flagOverrides: { [HOOK_COMMAND_REWRITE_FLAG_ID]: true },
    });

    try {
      const decision = await toolExecutor.beforeEmitter.fireBeforeExecute(
        makeStubToolContext({ command: 'grep' }),
      );

      // Args rewritten
      expect(decision?.rewrittenArgs).toEqual({ command: 'rg --hidden' });
      // Context injected
      const contextMsg = context.messages.find(
        (msg) => msg.origin !== undefined,
      );
      expect(contextMsg).toBeDefined();
      const content = contextMsg!.content as Array<{ type: string; text: string }>;
      expect(content[0]!.text).toBe('rewritten for safety');
    } finally {
      ix.dispose();
      disposables.dispose();
    }
  });

  it('should inject context but not rewrite args when flag is disabled', async () => {
    const hookRunner = stubHookRunner({
      trigger: async () => [
        {
          action: 'allow',
          updatedInput: { command: 'rg --hidden' },
          additionalContext: 'rewritten for safety',
          exitCode: 0,
          structuredOutput: true,
        },
      ],
    });

    const { context, toolExecutor, disposables, ix } = createServiceWithStubs({
      hookRunner,
      flagOverrides: { [HOOK_COMMAND_REWRITE_FLAG_ID]: false },
    });

    try {
      const decision = await toolExecutor.beforeEmitter.fireBeforeExecute(
        makeStubToolContext({ command: 'grep' }),
      );

      // No rewrite — flag off
      expect(decision?.rewrittenArgs).toBeUndefined();
      // Context still injected
      const contextMsg = context.messages.find(
        (msg) => msg.origin !== undefined,
      );
      expect(contextMsg).toBeDefined();
    } finally {
      ix.dispose();
      disposables.dispose();
    }
  });
});
