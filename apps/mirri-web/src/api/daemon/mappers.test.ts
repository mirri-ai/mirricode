// apps/mirri-web/src/api/daemon/mappers.test.ts
// Catalog wire (v2) mapping + provider write-payload builders. The web UI is
// v2-only (kap-server): catalog items arrive in the flat shape
// (capabilities[]/max_context_size/reasoning + import metadata), and provider
// creation requires an explicit id + models list.

import { describe, expect, it } from 'vitest';
import {
  toAppCatalogModel,
  toAppCatalogProvider,
  toAppEvent,
  toAppApprovalRequest,
  toAppTask,
  toWireAddProviderBody,
  toWireImportCatalogBody,
} from './mappers';
import { createAgentProjector } from './agentEventProjector';
import type { AppEvent } from '../types';
import { approvalRequestFromAskParams, routeRpcEventToAppEvents } from './ws-rpc-event-bridge';
import type { RpcEvent } from './json-rpc-client';
import type { WireBackgroundTask, WireCatalogModel, WireCatalogProvider, WireEvent } from './wire';

describe('catalog wire (v2) mapping', () => {
  it('maps a flat v2 catalog model to the app shape', () => {
    const wire: WireCatalogModel = {
      id: 'gpt-5.2',
      name: 'GPT-5.2',
      max_context_size: 400000,
      capabilities: ['image_in', 'thinking', 'tool_use'],
      reasoning: true,
    };
    expect(toAppCatalogModel(wire)).toEqual({
      id: 'gpt-5.2',
      name: 'GPT-5.2',
      maxContextSize: 400000,
      capabilities: ['image_in', 'thinking', 'tool_use'],
      reasoning: true,
    });
  });

  it('maps a v2 catalog provider with import metadata', () => {
    const wire: WireCatalogProvider = {
      id: 'azure',
      name: 'Azure',
      wire_type: 'openai',
      guessed: false,
      needs_base_url: true,
      rejected: false,
      reject_reason: null,
      env_key: 'AZURE_RESOURCE_NAME',
      models: [
        {
          id: 'phi-4-mini-reasoning',
          name: 'Phi-4-mini-reasoning',
          max_context_size: 128000,
          capabilities: ['thinking', 'tool_use'],
          reasoning: true,
        },
      ],
    };
    expect(toAppCatalogProvider(wire)).toEqual({
      id: 'azure',
      name: 'Azure',
      wireType: 'openai',
      guessed: false,
      needsBaseUrl: true,
      rejected: false,
      rejectReason: null,
      envKey: 'AZURE_RESOURCE_NAME',
      models: [
        {
          id: 'phi-4-mini-reasoning',
          name: 'Phi-4-mini-reasoning',
          maxContextSize: 128000,
          capabilities: ['thinking', 'tool_use'],
          reasoning: true,
        },
      ],
    });
  });

  it('keeps rejected providers visible with their reason', () => {
    const wire: WireCatalogProvider = {
      id: 'bedrock',
      name: 'Amazon Bedrock',
      wire_type: null,
      guessed: false,
      needs_base_url: false,
      rejected: true,
      reject_reason: 'proprietary-sdk',
      env_key: null,
      models: [],
    };
    const app = toAppCatalogProvider(wire);
    expect(app.rejected).toBe(true);
    expect(app.rejectReason).toBe('proprietary-sdk');
    expect(app.wireType).toBeNull();
  });
});

describe('provider write-payload builders (v2)', () => {
  it('builds the add-provider payload with snake_case models', () => {
    expect(
      toWireAddProviderBody({
        id: 'custom',
        type: 'openai',
        apiKey: 'sk-test',
        baseUrl: 'https://gateway.example.test/v1',
        models: [
          {
            model: 'gpt-5.2',
            maxContextSize: 400000,
            capabilities: ['thinking', 'tool_use'],
            supportEfforts: ['low', 'high'],
            adaptiveThinking: true,
          },
        ],
      }),
    ).toEqual({
      id: 'custom',
      type: 'openai',
      api_key: 'sk-test',
      base_url: 'https://gateway.example.test/v1',
      default_model: undefined,
      models: [
        {
          model: 'gpt-5.2',
          max_context_size: 400000,
          display_name: undefined,
          capabilities: ['thinking', 'tool_use'],
          max_output_size: undefined,
          support_efforts: ['low', 'high'],
          adaptive_thinking: true,
        },
      ],
    });
  });

  it('should send an empty models list when the add-provider payload carries zero models', () => {
    // Given a custom provider create with no model rows (the v2 schema
    // requires the key but allows the list to be empty)

    // When the wire body is built
    const body = toWireAddProviderBody({
      id: 'custom',
      type: 'openai',
      apiKey: 'sk-test',
      models: [],
    });

    // Then `models` is present and empty on the wire
    expect(body).toEqual({
      id: 'custom',
      type: 'openai',
      api_key: 'sk-test',
      base_url: undefined,
      default_model: undefined,
      models: [],
    });
  });

  it('builds the import-catalog payload', () => {
    expect(
      toWireImportCatalogBody({
        catalogId: 'azure',
        id: 'my-azure',
        apiKey: 'sk-test',
        baseUrl: 'https://azure.example.test',
      }),
    ).toEqual({
      catalog_id: 'azure',
      id: 'my-azure',
      api_key: 'sk-test',
      base_url: 'https://azure.example.test',
    });
    expect(toWireImportCatalogBody({ catalogId: 'azure' })).toEqual({
      catalog_id: 'azure',
      id: undefined,
      api_key: undefined,
      base_url: undefined,
    });
  });
});

describe('toAppTask (REST /tasks wire → app shape)', () => {
  it('carries the resolved subagent model onto the app task', () => {
    const wire: WireBackgroundTask = {
      id: 'task-1',
      session_id: 'session-1',
      kind: 'subagent',
      description: 'explore the codebase',
      status: 'running',
      created_at: '2026-01-01T00:00:00.000Z',
      subagent_type: 'coder',
      model: 'claude-sonnet',
    };
    const task = toAppTask(wire);
    expect(task.model).toBe('claude-sonnet');
    expect(task.subagentType).toBe('coder');
  });

  it('leaves model undefined when the wire task carries none', () => {
    const wire: WireBackgroundTask = {
      id: 'task-2',
      session_id: 'session-1',
      kind: 'bash',
      description: 'pnpm test',
      status: 'running',
      created_at: '2026-01-01T00:00:00.000Z',
    };
    expect(toAppTask(wire).model).toBeUndefined();
  });
});

describe('toAppEvent — event.session.work_changed', () => {
  it('should map the work-facts transition onto a sessionWorkChanged app event when the payload carries the tuple', () => {
    // The rust engine's durable work_changed frame (US4.4/SDR-013):
    // snake_case payload keys, the same vocabulary the TS v2 reference
    // emits — the sidebar's busy/mainTurnActive rows update without a
    // per-session subscription.
    const wire: WireEvent = {
      type: 'event.session.work_changed',
      session_id: 'session-1',
      seq: 11,
      timestamp: '2026-01-01T00:00:00.000Z',
      payload: {
        busy: true,
        main_turn_active: true,
        pending_interaction: 'none',
        last_turn_reason: 'completed',
      },
    };
    expect(toAppEvent(wire)).toEqual({
      type: 'sessionWorkChanged',
      sessionId: 'session-1',
      busy: true,
      mainTurnActive: true,
      pendingInteraction: 'none',
      lastTurnReason: 'completed',
      seq: 11,
    });
  });

  it('should keep the wire seq on the app event when the notification arrives through the RPC bridge', () => {
    const event: RpcEvent = {
      type: 'event.session.work_changed',
      sessionId: 'session-1',
      seq: 12,
      payload: {
        busy: false,
        main_turn_active: false,
        pending_interaction: 'none',
        last_turn_reason: 'cancelled',
      },
    };
    expect(routeRpcEventToAppEvents(createAgentProjector(), event)).toEqual([
      {
        type: 'sessionWorkChanged',
        sessionId: 'session-1',
        busy: false,
        mainTurnActive: false,
        pendingInteraction: 'none',
        lastTurnReason: 'cancelled',
        seq: 12,
      },
    ]);
  });
});

describe('toAppEvent — event.session.model_switched', () => {
  it('should map the engine model-switch notification onto a sessionModelSwitched app event when the payload names the previous and new model', () => {
    const wire: WireEvent = {
      type: 'event.session.model_switched',
      session_id: 'session-1',
      seq: 7,
      timestamp: '2026-01-01T00:00:00.000Z',
      payload: { from: 'model-a', to: 'model-b', timestamp: '2026-01-01T00:00:00.000Z' },
    };
    expect(toAppEvent(wire)).toEqual({
      type: 'sessionModelSwitched',
      sessionId: 'session-1',
      from: 'model-a',
      to: 'model-b',
      seq: 7,
    });
  });

  it('should keep the wire seq on the app event when the notification arrives through the RPC bridge', () => {
    const event: RpcEvent = {
      type: 'event.session.model_switched',
      sessionId: 'session-1',
      seq: 42,
      payload: { from: 'model-a', to: 'model-b', timestamp: '2026-01-01T00:00:00.000Z' },
    };
    expect(routeRpcEventToAppEvents(createAgentProjector(), event)).toEqual([
      {
        type: 'sessionModelSwitched',
        sessionId: 'session-1',
        from: 'model-a',
        to: 'model-b',
        seq: 42,
      },
    ]);
  });
});

describe('toAppEvent — event.mcp.server.status', () => {
  it('should map the rust core MCP pool notification onto an mcpServerStatus app event when the payload carries the server snapshot', () => {
    const wire: WireEvent = {
      type: 'event.mcp.server.status',
      session_id: 'session-1',
      seq: 9,
      timestamp: '2026-01-01T00:00:00.000Z',
      payload: {
        server: {
          name: 'github',
          transport: 'stdio',
          status: 'connected',
          toolCount: 12,
          pool: 'workspace',
          changeKind: 'status',
        },
      },
    };
    expect(toAppEvent(wire)).toEqual({
      type: 'mcpServerStatus',
      sessionId: 'session-1',
      server: {
        name: 'github',
        transport: 'stdio',
        status: 'connected',
        toolCount: 12,
        pool: 'workspace',
        changeKind: 'status',
      },
    });
  });

  it('should route through the RPC bridge instead of falling into the unhandled-event warning path', () => {
    const event: RpcEvent = {
      type: 'event.mcp.server.status',
      sessionId: 'session-1',
      seq: 10,
      payload: {
        server: { name: 'linear', transport: 'http', status: 'failed', toolCount: 0, pool: 'user', error: '401' },
      },
    };
    expect(routeRpcEventToAppEvents(createAgentProjector(), event)).toEqual([
      {
        type: 'mcpServerStatus',
        sessionId: 'session-1',
        server: { name: 'linear', transport: 'http', status: 'failed', toolCount: 0, pool: 'user', error: '401' },
      },
    ]);
  });
});

describe('approval request identity (supervision B1)', () => {
  it('should map the requesting member agent id when the wire row carries one, and leave it absent otherwise', () => {
    // Given an approval ask from a member agent and one from the main agent
    // (no agent_id) — the toAppApprovalRequest wire shape
    const wire = {
      approval_id: 'ap-9',
      session_id: 'session-1',
      tool_call_id: 'call-9',
      tool_name: 'Bash',
      action: 'execute Bash',
      tool_input_display: { kind: 'shell', command: 'ls' },
      created_at: '2026-09-14T00:00:00Z',
    };
    // When mapping both rows
    const member = toAppApprovalRequest({ ...wire, agent_id: 'agent-11111111-1111-4111-8111-111111111111' });
    const main = toAppApprovalRequest(wire);

    // Then the member's row carries the full agent id (the roster key) and
    // the main-agent row stays unattributed
    expect(member.agentId).toBe('agent-11111111-1111-4111-8111-111111111111');
    expect(main.agentId).toBeUndefined();
  });

  it('should map the member agent id from the approval.ask reverse-RPC params', () => {
    // Given an approval.ask payload the engine's permission hook built for a
    // member flow
    const params = {
      tool_name: 'Bash',
      tool_call_id: 'call-m',
      action: 'execute Bash',
      display: { kind: 'command', command: 'cargo test' },
      reason: 'approval required',
      request_id: 'req-1',
      agent_id: 'agent-22222222-2222-4222-8222-222222222222',
    };

    // When mapping the ask params
    const approval = approvalRequestFromAskParams(params, 'session-1');

    // Then the request carries the member's full agent id (the live badge
    // source); an ask without agent_id maps to an unattributed request
    expect(approval?.agentId).toBe('agent-22222222-2222-4222-8222-222222222222');
    expect(approval?.display).toEqual({ kind: 'command', command: 'cargo test' });
    expect(approvalRequestFromAskParams({ ...params, agent_id: undefined }, 'session-1')?.agentId)
      .toBeUndefined();
  });
});

describe('member tool event routing (supervision P2+1)', () => {
  it('should route a member tool call to its task row and never the main transcript when the frame carries the member agentId', () => {
    // Given a swarm member (agent-34d6376b…) starts a tool call while the
    // main agent's transcript is streaming (turn.step.started opens the
    // assistant message the projector tracks for the main transcript)
    const projector = createAgentProjector();
    const sessionId = 'session-1';
    projector.project('turn.started', { turnId: 'turn-1' }, sessionId);
    projector.project('turn.step.started', { turnId: 'turn-1' }, sessionId);
    projector.project('assistant.delta', { agentId: 'main', delta: 'thinking about the batch' }, sessionId);
    // (seed the member's task row the way the live stream does)
    projector.project('subagent.spawned', {
      subagentId: 'agent-34d6376b-c9ed-4555-9f99-2256e024348d',
      description: 'swarm #1 (coder)',
      parentToolCallId: 'call-swarm',
      swarmIndex: 0,
    }, sessionId);

    // When the member's tool call frame arrives tagged with its agentId
    const events = projector.project('tool.call.started', {
      agentId: 'agent-34d6376b-c9ed-4555-9f99-2256e024348d',
      toolCallId: 'call-read-1',
      name: 'Read',
      args: { file_path: 'note.txt' },
    }, sessionId);

    // Then the frame lands on the member's task row (taskProgress carrying
    // the synthesized "Calling Read" activity line) and the main transcript
    // gains NO tool slot (no messageUpdated with a toolUse content part)
    const progress = events.filter((e) => e.type === 'taskProgress');
    expect(progress).toHaveLength(1);
    expect(progress[0]!.taskId).toBe('agent-34d6376b-c9ed-4555-9f99-2256e024348d');
    const transcriptUpdates = events.filter(
      (e) => e.type === 'messageUpdated' || e.type === 'assistantDelta',
    );
    expect(transcriptUpdates).toHaveLength(0);
  });

  it('should keep an unattributed tool call in the main transcript when the frame carries no agentId', () => {
    // Given the main agent's transcript is streaming
    const projector = createAgentProjector();
    const sessionId = 'session-1';
    projector.project('turn.started', { turnId: 'turn-1' }, sessionId);
    projector.project('turn.step.started', { turnId: 'turn-1' }, sessionId);
    projector.project('assistant.delta', { agentId: 'main', delta: 'let me check' }, sessionId);

    // When a tool frame arrives without agentId (the main agent's dialect
    // before the engine stamp — kept working for older engines)
    const events = projector.project('tool.call.started', {
      toolCallId: 'call-main-1',
      name: 'Read',
      args: { file_path: 'plan.md' },
    }, sessionId);

    // Then the main transcript gains the tool slot
    expect(events.some((e) => e.type === 'messageUpdated')).toBe(true);
  });
});

describe('member compaction gating (CR5 agent attribution)', () => {
  const MEMBER_ID = 'agent-34d6376b-c9ed-4555-9f99-2256e024348d';

  it('should keep session compaction state untouched when a member agent compacts', () => {
    // Given a swarm member with a live task row while the session-level
    // compaction state is idle (the divider/banner the reducer owns)
    const projector = createAgentProjector();
    const sessionId = 'session-1';
    projector.project('subagent.spawned', {
      subagentId: MEMBER_ID,
      description: 'swarm #1 (coder)',
      parentToolCallId: 'call-swarm',
      swarmIndex: 0,
    }, sessionId);

    // When the member's auto-compaction finishes (frame stamped with its agentId)
    const events = projector.project('compaction.completed', {
      agentId: MEMBER_ID,
      result: { tokensBefore: 9000, tokensAfter: 1200, summary: 'member history summarized' },
    }, sessionId);

    // Then NO session-level compaction AppEvent fires (the member's compaction
    // is not the session's history being compacted — emitting it would clobber
    // the divider/banner state)
    const sessionCompaction = events.filter(
      (e) => e.type === 'compactionCompleted' || e.type === 'historyCompacted' || e.type === 'compactionStarted' || e.type === 'compactionCancelled',
    );
    expect(sessionCompaction).toHaveLength(0);

    // And the member row receives the progress attribution instead
    const progress = events.filter((e): e is Extract<AppEvent, { type: 'taskProgress' }> => e.type === 'taskProgress');
    expect(progress).toHaveLength(1);
    expect(progress[0]?.taskId).toBe(MEMBER_ID);
    expect(progress[0]?.outputChunk).toContain('compacted');
  });

  it('should emit session compaction state when the main agent compacts', () => {
    // Given the main agent's auto-compaction finished (frame stamped 'main')
    const projector = createAgentProjector();
    const events = projector.project('compaction.completed', {
      agentId: 'main',
      result: { tokensBefore: 9000, tokensAfter: 1200, summary: 'history summarized' },
    }, 'session-1');

    // Then the existing session-level AppEvents fire unchanged (zero regression)
    const completed = events.find((e) => e.type === 'compactionCompleted');
    expect(completed?.type === 'compactionCompleted' ? completed.tokensBefore : undefined).toBe(9000);
    expect(events.some((e) => e.type === 'historyCompacted')).toBe(true);
  });

  it('should emit session compaction state when the frame carries no agentId (legacy engines)', () => {
    // Given an older engine that never stamped agentId on compaction frames
    const projector = createAgentProjector();
    const events = projector.project('compaction.completed', {
      result: { tokensBefore: 9000, tokensAfter: 1200 },
    }, 'session-1');

    // Then the session-level AppEvents still fire — absent agentId is main
    expect(events.some((e) => e.type === 'compactionCompleted')).toBe(true);
    expect(events.some((e) => e.type === 'historyCompacted')).toBe(true);
  });
});

describe('structured agent status consumption (CR4)', () => {
  const MEMBER_ID = 'agent-0192-bbb-4c2f-8e5d-member000001';

  function spawnedMember(projector: ReturnType<typeof createAgentProjector>, sessionId: string) {
    projector.project('subagent.spawned', {
      subagentId: MEMBER_ID,
      description: 'swarm #1 (coder)',
      parentToolCallId: 'call-swarm',
      swarmIndex: 0,
    }, sessionId);
  }

  it('should record the member phase on its task row when an attributed status update arrives', () => {
    // Given a swarm member with a live task row
    const projector = createAgentProjector();
    const sessionId = 'session-1';
    spawnedMember(projector, sessionId);

    // When the member's structured status frame arrives (CR4: agentId + phase
    // plus the optional status bag)
    const events = projector.project('agent.status.updated', {
      agentId: MEMBER_ID,
      phase: 'tool_call',
      model: 'claude-sonnet-4.6',
      contextTokens: 1200,
      maxContextTokens: 200000,
      thinkingEffort: 'medium',
    }, sessionId);

    // Then the member task row carries the live phase — mapped onto the badge
    // vocabulary (`AppSubagentPhase` in types.ts is the display contract: a
    // wire `tool_call` is a working state on the badge)
    const created = events.find((e) => e.type === 'taskCreated');
    const task = created?.type === 'taskCreated' ? created.task : undefined;
    expect(task?.id).toBe(MEMBER_ID);
    expect(task?.subagentPhase).toBe('working');

    // And the member's status bag does NOT leak into the session-level usage —
    // the session's own watermark/model stays owned by the main agent's frame
    expect(events.some((e) => e.type === 'sessionUsageUpdated')).toBe(false);
  });

  it('should map the wire awaiting_approval phase onto the awaiting badge phase', () => {
    // Given a member that already started working
    const projector = createAgentProjector();
    const sessionId = 'session-1';
    spawnedMember(projector, sessionId);
    projector.project('subagent.started', { subagentId: MEMBER_ID }, sessionId);

    // When the member enters approval wait (CR4 wire phase)
    const events = projector.project('agent.status.updated', {
      agentId: MEMBER_ID,
      phase: 'awaiting_approval',
    }, sessionId);

    // Then the badge shows the awaiting-user phase (ADR-008 ①)
    const created = events.find((e) => e.type === 'taskCreated');
    expect(created?.type === 'taskCreated' ? created.task.subagentPhase : undefined).toBe('awaiting');
  });

  it('should ignore a member status frame without phase (v2 engines / older rust builds)', () => {
    // Given a queued member and a status frame that carries no `phase` — the
    // v2 TS engine's status bag and pre-CR4 rust builds omit it entirely
    const projector = createAgentProjector();
    const sessionId = 'session-1';
    spawnedMember(projector, sessionId);

    const events = projector.project('agent.status.updated', {
      agentId: MEMBER_ID,
      model: 'claude-sonnet-4.6',
      contextTokens: 10,
      maxContextTokens: 100,
    }, sessionId);

    // Then the frame is tolerated silently — no crash, no state churn, the
    // task row keeps its previous phase
    expect(events).toEqual([]);
    expect(projector.state(sessionId).subagentMeta.get(MEMBER_ID)?.subagentPhase).toBe('queued');
  });

  it('should ignore an unknown member phase value instead of corrupting the badge', () => {
    // Given a live member and a phase string the web has not learned (a future
    // engine vocabulary member)
    const projector = createAgentProjector();
    const sessionId = 'session-1';
    spawnedMember(projector, sessionId);

    const events = projector.project('agent.status.updated', {
      agentId: MEMBER_ID,
      phase: 'hovering',
    }, sessionId);

    // Then the foreign value never lands on AppTask.subagentPhase (the badge's
    // Record<AppSubagentPhase, number> counts must stay well-formed)
    expect(events).toEqual([]);
    expect(projector.state(sessionId).subagentMeta.get(MEMBER_ID)?.subagentPhase).toBe('queued');
  });

  it('should not resurrect a finished member row from a late status frame', () => {
    // Given a member whose lifecycle already decided the terminal state
    const projector = createAgentProjector();
    const sessionId = 'session-1';
    spawnedMember(projector, sessionId);
    projector.project('subagent.completed', { subagentId: MEMBER_ID, resultSummary: 'done' }, sessionId);

    // When a racing/late status frame arrives after the end (out-of-order WS
    // redelivery after a reconnect)
    const events = projector.project('agent.status.updated', {
      agentId: MEMBER_ID,
      phase: 'ended',
    }, sessionId);

    // Then the terminal state stays untouched — lifecycle events own the end
    expect(events).toEqual([]);
    expect(projector.state(sessionId).subagentMeta.get(MEMBER_ID)?.subagentPhase).toBe('completed');
  });

  it('should refresh session usage when the main agent status carries watermark', () => {
    // Given the main agent's structured status frame with the context
    // watermark (CR4 payload, main-attributed)
    const projector = createAgentProjector();
    const events = projector.project('agent.status.updated', {
      agentId: 'main',
      phase: 'running',
      contextTokens: 1,
      maxContextTokens: 2,
      usage: { inputOther: 1, output: 2, inputCacheRead: 3, inputCacheCreation: 4 },
    }, 'session-1');

    // Then the existing sessionUsageUpdated path fires with the watermark
    // applied (zero regression — the main arm reads the same keys as before)
    const usageEvent = events.find((e) => e.type === 'sessionUsageUpdated');
    expect(usageEvent?.type === 'sessionUsageUpdated' ? usageEvent.usage.contextTokens : undefined).toBe(1);
    expect(usageEvent?.type === 'sessionUsageUpdated' ? usageEvent.usage.contextLimit : undefined).toBe(2);
  });
});

describe('structured member suspension projection (P3b, ADR-011)', () => {
  const MEMBER_ID = 'agent-34d6376b-c9ed-4555-9f99-2256e024348d';

  function suspendedTask(reason: unknown) {
    const projector = createAgentProjector();
    const events = projector.project(
      'subagent.suspended',
      { subagentId: MEMBER_ID, reason },
      'session-1',
    );
    const created = events.find((e) => e.type === 'taskCreated');
    return created && created.type === 'taskCreated' ? created.task : null;
  }

  it('should project a structured approval suspension onto the awaiting phase when the raw reason carries kind approval', () => {
    // Given a raw subagent.suspended event whose reason is the ADR-011
    // structured object { kind: 'approval', request_id, tool_call_id }
    const task = suspendedTask({ kind: 'approval', request_id: 'r1', tool_call_id: 't1' });

    // Then the task lands on the event-driven awaiting phase and carries the
    // structured reason in camelCase with the gate ids intact
    expect(task?.subagentPhase).toBe('awaiting');
    expect(task?.suspendedReason).toEqual({ kind: 'approval', requestId: 'r1', toolCallId: 't1' });
  });

  it('should project a structured question suspension onto the awaiting phase when the raw reason carries kind question', () => {
    // Given a structured question suspension whose tool call id is absent
    const task = suspendedTask({ kind: 'question', request_id: 'q1' });

    // Then the task lands on awaiting with the question gate id and no
    // tool-call placeholder
    expect(task?.subagentPhase).toBe('awaiting');
    expect(task?.suspendedReason).toEqual({ kind: 'question', requestId: 'q1' });
  });

  it('should project a structured rate-limit suspension onto the backoff phase when the raw reason carries kind rate_limit', () => {
    // Given a structured rate-limit suspension (the swarm scheduler's requeue)
    const task = suspendedTask({ kind: 'rate_limit' });

    // Then the task lands on the backoff phase — distinguishable from waiting
    // on the user, per ADR-008 ①/ADR-011
    expect(task?.subagentPhase).toBe('backoff');
    expect(task?.suspendedReason).toEqual({ kind: 'rate_limit' });
  });

  it('should map a legacy string suspension reason onto the suspended phase when the raw reason is a string', () => {
    // Given the pre-ADR-011 free-form string (compat window — the brief's
    // ambiguity is pinned here: NO string sniffing, 'rate limit' text included)
    const task = suspendedTask('Provider rate limit; subagent requeued for retry.');

    // Then the legacy string maps to the plain suspended phase and passes
    // through verbatim
    expect(task?.subagentPhase).toBe('suspended');
    expect(task?.suspendedReason).toBe('Provider rate limit; subagent requeued for retry.');
  });

  it('should keep the member running while suspended so the lifecycle stays non-terminal', () => {
    // Given any suspension reason shape
    const task = suspendedTask('Provider rate limit; subagent requeued for retry.');

    // Then the task stays running — a later subagent.started resumption or a
    // terminal event decides the end
    expect(task?.status).toBe('running');
  });

  it('should project the same structured phase through the prefixed RPC event route', () => {
    // Given an event.subagent.suspended frame arriving via the RPC bridge (the
    // daemon's prefixed dialect — the classifier strips the prefix and projects)
    const event: RpcEvent = {
      type: 'event.subagent.suspended',
      sessionId: 'session-1',
      seq: 7,
      payload: { subagentId: MEMBER_ID, reason: { kind: 'approval', request_id: 'r1' } },
    };

    // When the bridge routes it through the projector
    const appEvents = routeRpcEventToAppEvents(createAgentProjector(), event);

    // Then the projected task carries the event-driven awaiting phase
    const created = appEvents.find((e) => e.type === 'taskCreated');
    expect(created?.type === 'taskCreated' ? created.task.subagentPhase : undefined).toBe('awaiting');
  });
});
