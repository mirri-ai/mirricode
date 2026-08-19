import { afterEach, beforeAll, beforeEach, describe, expect, it } from 'vitest';

import { DisposableStore } from '#/_base/di/lifecycle';
import { createServices, type TestInstantiationService } from '#/_base/di/test';
import { LifecycleScope, createAppScope } from '#/_base/di/scope';
import { Error2, ErrorCodes } from '#/errors';
import {
  IAgentCommandService,
  type AgentCommandInfo,
} from '#/agent/command/agentCommand';
import { AgentCommandService } from '#/agent/command/agentCommandService';
import type { CommandContribution } from '#/agent/command/commandContribution';
import {
  IAgentScopeContext,
  makeAgentScopeContext,
} from '#/agent/scopeContext/scopeContext';
import type { IEventBus } from '#/app/event/eventBus';
import type { IEventService } from '#/app/event/event';
import type { IPluginService } from '#/app/plugin/plugin';
import type { ITelemetryService } from '#/app/telemetry/telemetry';
import type { IAgentContextMemoryService } from '#/agent/contextMemory/contextMemory';
import type { IAgentContextSizeService } from '#/agent/contextSize/contextSize';
import type { IAgentFullCompactionService } from '#/agent/fullCompaction/fullCompaction';
import type { IAgentLoopService } from '#/agent/loop/loop';
import type { IAgentPermissionModeService } from '#/agent/permissionMode/permissionMode';
import type { IAgentPromptService } from '#/agent/prompt/prompt';
import type { IAgentSkillService } from '#/agent/skill/skill';
import type { IAgentToolPolicyService } from '#/agent/toolPolicy/toolPolicy';
import type { IAgentToolRegistryService } from '#/agent/toolRegistry/toolRegistry';
import type { IAgentConversationUndoService } from '#/agent/undo/undo';
import type { AgentRPCService } from '#/agent/rpc/rpcService';
import type { IAgentLifecycleService } from '#/session/agentLifecycle/agentLifecycle';
import type { ISessionContext } from '#/session/sessionContext/sessionContext';
import type { ISessionMetadata } from '#/session/sessionMetadata/sessionMetadata';

function stubService<T>(): T {
  // Only the command service dependency is exercised by these tests; every
  // other RPC dependency is a placeholder that is never invoked.
  return undefined as unknown as T;
}

function contribution(name: string, description: string | undefined, run: CommandContribution['run']): CommandContribution {
  return { name, description, run };
}

describe('AgentCommandService', () => {
  let disposables: DisposableStore;
  let ix: TestInstantiationService;
  let agentScopeContext: IAgentScopeContext;

  beforeEach(() => {
    disposables = new DisposableStore();
    agentScopeContext = makeAgentScopeContext({ agentId: 'main', agentScope: '' });
    ix = createServices(disposables, {
      additionalServices: (reg) => {
        // Lazy descriptor: `AgentCommandService` is constructed on first
        // `get` in this flattened harness (production binds it at Agent scope
        // creation, see the scoped-registration describe below).
        reg.define(IAgentCommandService, AgentCommandService);
        reg.defineInstance(IAgentScopeContext, agentScopeContext);
      },
    });
  });

  afterEach(() => disposables.dispose());

  it('should return the description and engine source when list is queried after registering', () => {
    const service = ix.get(IAgentCommandService);
    const handle = service.register(
      contribution('summarize', 'Summarizes the conversation', () => {}),
    );

    expect(service.list()).toEqual([
      { name: 'summarize', description: 'Summarizes the conversation', source: 'engine' },
    ] satisfies readonly AgentCommandInfo[]);

    handle.dispose();
  });

  it('should expose the given source when registering with a provider-specific source', () => {
    const service = ix.get(IAgentCommandService);
    const handle = service.register(
      contribution('deploy', 'Deploys the service', () => {}),
      'my-plugin',
    );

    expect(service.list()).toEqual([
      { name: 'deploy', description: 'Deploys the service', source: 'my-plugin' },
    ] satisfies readonly AgentCommandInfo[]);

    handle.dispose();
  });

  it('should list only the latest registration when the same name is registered twice', () => {
    const service = ix.get(IAgentCommandService);
    const first = service.register(contribution('retry', 'First description', () => {}));
    const second = service.register(contribution('retry', 'Second description', () => {}));

    expect(service.list()).toHaveLength(1);
    expect(service.list()[0]).toMatchObject({
      name: 'retry',
      description: 'Second description',
      source: 'engine',
    });

    first.dispose();
    second.dispose();
  });

  it('should remove the command from the list and ignore repeated dispose when the handle is disposed', () => {
    const service = ix.get(IAgentCommandService);
    const wrapper = service.register(contribution('stop', undefined, () => {}));

    wrapper.dispose();
    expect(service.list()).toEqual([]);

    expect(() => wrapper.dispose()).not.toThrow();
    expect(service.list()).toEqual([]);
  });

  it('should not remove a same-name re-registration when a stale old handle is disposed', () => {
    const service = ix.get(IAgentCommandService);
    const firstHandle = service.register(contribution('echo', 'First', () => {}));
    firstHandle.dispose();

    const secondHandle = service.register(contribution('echo', 'Second', () => {}));
    // The disposed `firstHandle` must not delete the newer `secondHandle` entry.
    firstHandle.dispose();

    expect(service.list()).toHaveLength(1);
    expect(service.list()[0]).toMatchObject({ name: 'echo', description: 'Second' });

    secondHandle.dispose();
  });

  it('should throw Error2 with REQUEST_INVALID when running an unknown command name', async () => {
    const service = ix.get(IAgentCommandService);

    await expect(service.run('does-not-exist')).rejects.toSatisfy((error: unknown) => {
      return error instanceof Error2 && error.code === ErrorCodes.REQUEST_INVALID;
    });
  });

  it('should pass the raw argument string to the contribution when running with args', async () => {
    const service = ix.get(IAgentCommandService);
    let receivedArgs: string | undefined;
    service.register(
      contribution('echo', undefined, (ctx) => {
        receivedArgs = ctx.args;
      }),
    );

    await service.run('echo', '--verbose -- file.txt');

    expect(receivedArgs).toBe('--verbose -- file.txt');
  });

  it('should await an async contribution completion before settling run', async () => {
    const service = ix.get(IAgentCommandService);
    let released = false;
    service.register(
      contribution('async-task', undefined, async () => {
        await Promise.resolve();
        await Promise.resolve();
        released = true;
      }),
    );

    await service.run('async-task');

    expect(released).toBe(true);
  });

  it('should fire the change event on register and once more on actual removal', () => {
    const service = ix.get(IAgentCommandService);
    let changeCount = 0;
    const changeSubscription = service.onDidChange(() => {
      changeCount += 1;
    });

    const handle = service.register(contribution('watch', undefined, () => {}));
    expect(changeCount).toBe(1);

    handle.dispose();
    expect(changeCount).toBe(2);

    handle.dispose();
    expect(changeCount).toBe(2);

    changeSubscription.dispose();
  });

  it('should hand the scope-resolvable services to the contribution through ctx.get', async () => {
    const service = ix.get(IAgentCommandService);
    let resolvedScope: IAgentScopeContext | undefined;
    service.register(
      contribution('whoami', undefined, (ctx) => {
        resolvedScope = ctx.get(IAgentScopeContext);
      }),
    );

    await service.run('whoami');

    expect(resolvedScope).toBe(agentScopeContext);
    expect(resolvedScope?.agentId).toBe('main');
  });

  it('should keep ctx.get usable after an await inside an async contribution', async () => {
    const service = ix.get(IAgentCommandService);
    let resolvedLate: IAgentScopeContext | undefined;
    service.register(
      contribution('late-get', undefined, async (ctx) => {
        await Promise.resolve();
        // `get` must not be bound to the run-time accessor's lifetime.
        resolvedLate = ctx.get(IAgentScopeContext);
      }),
    );

    await service.run('late-get');

    expect(resolvedLate).toBe(agentScopeContext);
  });
});

describe('AgentCommandService scoped registration', () => {
  it('should resolve the command service from an actual Agent scope', () => {
    const app = createAppScope();
    try {
      const session = app.createChild(LifecycleScope.Session, 'test-session');
      const agent = session.createChild(LifecycleScope.Agent, 'main');

      const service = agent.accessor.get(IAgentCommandService);
      expect(service).toBeInstanceOf(AgentCommandService);

      service.register(contribution('ping', 'Pong', () => {}));
      expect(service.list()).toEqual([
        { name: 'ping', description: 'Pong', source: 'engine' },
      ]);
    } finally {
      app.dispose();
    }
  });
});

describe('AgentRPCService command forwarding', () => {
  let disposables: DisposableStore;
  let ix: TestInstantiationService;
  let commands: IAgentCommandService;
  let rpc: AgentRPCService;
  let AgentRPCServiceClass: typeof import('#/agent/rpc/rpcService').AgentRPCService;

  beforeAll(async () => {
    // The RPC module registers an Agent-scoped `OnScopeCreated` service, so it
    // must not be imported before the scope-tree test above: that test creates
    // an Agent scope without the RPC dependency graph in place. Importing it
    // lazily keeps the earlier describe block's scope construction minimal.
    AgentRPCServiceClass = (await import('#/agent/rpc/rpcService')).AgentRPCService;
  });

  beforeEach(() => {
    disposables = new DisposableStore();
    ix = createServices(disposables, {
      additionalServices: (reg) => {
        reg.define(IAgentCommandService, AgentCommandService);
        reg.defineInstance(
          IAgentScopeContext,
          makeAgentScopeContext({ agentId: 'main', agentScope: '' }),
        );
      },
    });
    commands = ix.get(IAgentCommandService);
    rpc = new AgentRPCServiceClass(
      stubService<IAgentPromptService>(),
      stubService<IAgentConversationUndoService>(),
      stubService<IAgentLoopService>(),
      stubService<IAgentToolPolicyService>(),
      stubService<IAgentPermissionModeService>(),
      stubService<IAgentFullCompactionService>(),
      stubService<IAgentToolRegistryService>(),
      stubService<IAgentContextMemoryService>(),
      stubService<IAgentContextSizeService>(),
      stubService<IAgentSkillService>(),
      stubService<ITelemetryService>(),
      stubService<IEventBus>(),
      stubService<IEventService>(),
      stubService<IPluginService>(),
      commands,
      stubService<ISessionMetadata>(),
      stubService<ISessionContext>(),
      stubService<IAgentScopeContext>(),
      stubService<IAgentLifecycleService>(),
    );
  });

  afterEach(() => disposables.dispose());

  it('should forward listCommands to the registered command catalog', () => {
    const handle = commands.register(
      contribution('summarize', 'Summarizes the conversation', () => {}),
    );

    expect(rpc.listCommands({})).toContainEqual({
      name: 'summarize',
      description: 'Summarizes the conversation',
      source: 'engine',
    });

    handle.dispose();
  });

  it('should forward runCommand into the command service with the payload args', async () => {
    let receivedArgs: string | undefined;
    const handle = commands.register(
      contribution('greet', undefined, (ctx) => {
        receivedArgs = ctx.args;
      }),
    );

    await rpc.runCommand({ name: 'greet', args: 'hello world' });

    expect(receivedArgs).toBe('hello world');
    handle.dispose();
  });

  it('should propagate the unknown-command Error2 from runCommand', async () => {
    await expect(rpc.runCommand({ name: 'missing' })).rejects.toSatisfy(
      (error: unknown) => error instanceof Error2 && error.code === ErrorCodes.REQUEST_INVALID,
    );
  });
});