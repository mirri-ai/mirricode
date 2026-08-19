/**
 * steer-metadata — when steer() is called, the session title and lastPrompt
 * must be updated via applyPromptMetadataUpdate, just like prompt() does.
 *
 * Standalone unit test — no harness.
 * Run: `pnpm exec vitest run packages/agent-core-v2/test/agent/rpc/steer-metadata.test.ts`.
 */

import { describe, expect, it, vi } from 'vitest';

import { DisposableStore } from '#/_base/di/lifecycle';
import { createServices } from '#/_base/di/test';
import { Event } from '#/_base/event';
import { IAgentCommandService } from '#/agent/command/agentCommand';
import { IAgentContextMemoryService } from '#/agent/contextMemory/contextMemory';
import { IAgentContextSizeService } from '#/agent/contextSize/contextSize';
import { IAgentConversationUndoService } from '#/agent/undo/undo';
import { IAgentFullCompactionService } from '#/agent/fullCompaction/fullCompaction';
import { IAgentLoopService } from '#/agent/loop/loop';
import { IAgentPromptService } from '#/agent/prompt/prompt';
import { AgentPromptService } from '#/agent/prompt/promptService';
import { IAgentSystemReminderService } from '#/agent/systemReminder/systemReminder';
import { AgentSystemReminderService } from '#/agent/systemReminder/systemReminderService';
import { IAgentToolExecutorService } from '#/agent/toolExecutor/toolExecutor';
import { IAgentToolPolicyService } from '#/agent/toolPolicy/toolPolicy';
import { IAgentPermissionModeService } from '#/agent/permissionMode/permissionMode';
import { IAgentSkillService } from '#/agent/skill/skill';
import { IAgentToolRegistryService } from '#/agent/toolRegistry/toolRegistry';
import { IAgentScopeContext } from '#/agent/scopeContext/scopeContext';
import { IAgentLifecycleService } from '#/session/agentLifecycle/agentLifecycle';
import { ISessionMetadata, type SessionMeta, type SessionMetaPatch } from '#/session/sessionMetadata/sessionMetadata';
import { ISessionContext } from '#/session/sessionContext/sessionContext';
import { IEventBus } from '#/app/event/eventBus';
import { EventBusService } from '#/app/event/eventBusService';
import { IEventService, type DomainEvent } from '#/app/event/event';
import { IPluginService } from '#/app/plugin/plugin';
import { ITelemetryService } from '#/app/telemetry/telemetry';
import { IWireService } from '#/wire/wire';
import { createHooks } from '#/hooks';

import { AgentRPCService } from '#/agent/rpc/rpcService';
import { IAgentRPCService } from '#/agent/rpc/rpc';

import { stubContextMemory } from '../contextMemory/stubs';
import { stubLoopWithHooks, stubToolExecutor, stubWire } from '../loop/stubs';
import { registerStateServices } from '../../state/stubs';

function userMessage(text: string) {
  return { role: 'user' as const, content: [{ type: 'text' as const, text }], toolCalls: [] as never[], origin: { kind: 'user' as const } };
}

interface TestHarness {
  rpc: IAgentRPCService;
  prompt: IAgentPromptService;
  loop: ReturnType<typeof stubLoopWithHooks>;
  context: ReturnType<typeof stubContextMemory>;
  metadataPatchLog: SessionMetaPatch[];
  publishedEvents: DomainEvent[];
  disposables: DisposableStore;
}

function createHarness(metadataReadOverride?: () => Promise<SessionMeta>): TestHarness {
  const disposables = new DisposableStore();
  const context = stubContextMemory();
  const loop = stubLoopWithHooks({ pendingTurnResult: true });
  const fullCompaction = {
    _serviceBrand: undefined,
    compacting: null,
    begin: () => false,
    hooks: createHooks(['onWillCompact']),
    onDidFinishCompaction: Event.None,
  } as unknown as IAgentFullCompactionService;

  const metadataPatchLog: SessionMetaPatch[] = [];
  const publishedEvents: DomainEvent[] = [];

  const defaultMetadata: SessionMeta = {
    id: 'test-session',
    title: '',
    isCustomTitle: false,
    lastPrompt: '',
    createdAt: Date.now(),
    updatedAt: Date.now(),
    archived: false,
  };

  const sessionMetadata: ISessionMetadata = {
    _serviceBrand: undefined,
    ready: Promise.resolve(),
    onDidChangeMetadata: () => ({ dispose: () => {} }),
    read: metadataReadOverride ?? (async () => ({ ...defaultMetadata })),
    update: vi.fn(async (patch: SessionMetaPatch) => {
      metadataPatchLog.push(patch);
    }),
    setTitle: vi.fn(),
    setArchived: vi.fn(),
    registerAgent: vi.fn(),
  };

  const eventService: IEventService = {
    _serviceBrand: undefined,
    onDidPublish: Event.None as Event<DomainEvent>,
    publish: vi.fn((event: DomainEvent) => {
      publishedEvents.push(event);
    }),
    subscribe: vi.fn(() => ({ dispose: () => {} })),
  };

  const ix = createServices(disposables, {
    strict: true,
    additionalServices: (reg) => {
      registerStateServices(reg);
      reg.defineInstance(IAgentContextMemoryService, context);
      reg.defineInstance(IAgentLoopService, loop);
      reg.defineInstance(IWireService, stubWire());
      reg.defineInstance(IAgentToolExecutorService, stubToolExecutor());
      reg.defineInstance(IAgentFullCompactionService, fullCompaction);
      reg.define(IEventBus, EventBusService);
      reg.define(IAgentSystemReminderService, AgentSystemReminderService);
      reg.define(IAgentPromptService, AgentPromptService);
      reg.defineInstance(IAgentConversationUndoService, { _serviceBrand: undefined, undo: vi.fn() } as unknown as IAgentConversationUndoService);
      reg.defineInstance(IAgentToolPolicyService, { _serviceBrand: undefined, isToolActive: vi.fn(), setSessionDisabledTools: vi.fn() } as unknown as IAgentToolPolicyService);
      reg.defineInstance(IAgentPermissionModeService, { _serviceBrand: undefined, mode: 'manual' as const, setMode: vi.fn() } as unknown as IAgentPermissionModeService);
      reg.defineInstance(IAgentContextSizeService, { _serviceBrand: undefined, get: vi.fn(() => ({ measured: 0 })) } as unknown as IAgentContextSizeService);
      reg.defineInstance(IAgentSkillService, { _serviceBrand: undefined, activate: vi.fn() } as unknown as IAgentSkillService);
      reg.defineInstance(ITelemetryService, { _serviceBrand: undefined, track2: vi.fn() } as unknown as ITelemetryService);
      reg.defineInstance(IEventService, eventService);
      reg.defineInstance(IPluginService, { _serviceBrand: undefined, listPluginCommands: vi.fn(async () => []) } as unknown as IPluginService);
      reg.defineInstance(ISessionMetadata, sessionMetadata);
      reg.defineInstance(ISessionContext, { _serviceBrand: undefined, sessionId: 'test-session' } as unknown as ISessionContext);
      reg.defineInstance(IAgentScopeContext, { _serviceBrand: undefined, agentId: 'main' } as unknown as IAgentScopeContext);
      reg.defineInstance(IAgentLifecycleService, { _serviceBrand: undefined, broadcastPermissionMode: vi.fn() } as unknown as IAgentLifecycleService);
      reg.defineInstance(IAgentToolRegistryService, { _serviceBrand: undefined, list: vi.fn(() => []) } as unknown as IAgentToolRegistryService);
      reg.defineInstance(IAgentCommandService, {
        _serviceBrand: undefined,
        onDidChange: Event.None,
        list: vi.fn(() => []),
        run: vi.fn(async () => {}),
        register: vi.fn(() => ({ dispose: () => {} })),
      } as unknown as IAgentCommandService);
      reg.define(IAgentRPCService, AgentRPCService);
    },
  });

  return {
    rpc: ix.get(IAgentRPCService),
    prompt: ix.get(IAgentPromptService),
    loop,
    context,
    metadataPatchLog,
    publishedEvents,
    disposables,
  };
}

describe('steer() prompt metadata update', () => {
  it('should update lastPrompt and auto-title when steer() is called on an untitled session', async () => {
    const { rpc, prompt, loop, context, metadataPatchLog, publishedEvents, disposables } = createHarness();

    // Start an active turn so steer has something to steer into
    const activeHandle = await prompt.enqueue({ message: userMessage('initial prompt') });
    await activeHandle.launched;
    loop.drainNextBatch(context);

    // Clear metadata updates from the initial prompt
    metadataPatchLog.length = 0;
    publishedEvents.length = 0;

    // Enqueue a queued prompt then steer it via the RPC service
    await prompt.enqueue({ message: userMessage('steer me') });
    await rpc.steer({ input: [{ type: 'text', text: 'steer me' }] });

    // The session metadata should have been updated with the steer text
    expect(metadataPatchLog.length).toBeGreaterThanOrEqual(1);
    const lastPatch = metadataPatchLog.at(-1)!;
    expect(lastPatch.lastPrompt).toBe('steer me');

    // Since the session has no custom title and the title is untitled,
    // the title should be auto-generated from the steer text
    expect(lastPatch.title).toBe('steer me');
    expect(lastPatch.isCustomTitle).toBe(false);

    // A session.meta.updated event should have been published
    expect(publishedEvents.some((e) => e.type === 'session.meta.updated')).toBe(true);

    disposables.dispose();
  });

  it('should preserve a custom title when steer() updates lastPrompt', async () => {
    const customMetadataRead = async (): Promise<SessionMeta> => ({
      id: 'test-session',
      title: 'My Custom Title',
      isCustomTitle: true,
      lastPrompt: '',
      createdAt: Date.now(),
      updatedAt: Date.now(),
      archived: false,
    });

    const { rpc, prompt, loop, context, metadataPatchLog, disposables } = createHarness(customMetadataRead);

    // Start an active turn
    const activeHandle = await prompt.enqueue({ message: userMessage('active') });
    await activeHandle.launched;
    loop.drainNextBatch(context);

    // Clear patches from prompt()
    metadataPatchLog.length = 0;

    // Enqueue + steer
    await prompt.enqueue({ message: userMessage('new direction') });
    await rpc.steer({ input: [{ type: 'text', text: 'new direction' }] });

    // lastPrompt should be updated but title should NOT be overwritten
    expect(metadataPatchLog.length).toBeGreaterThanOrEqual(1);
    const lastPatch = metadataPatchLog.at(-1)!;
    expect(lastPatch.lastPrompt).toBe('new direction');
    expect(lastPatch.title).toBeUndefined();
    expect(lastPatch.isCustomTitle).toBeUndefined();

    disposables.dispose();
  });
});
