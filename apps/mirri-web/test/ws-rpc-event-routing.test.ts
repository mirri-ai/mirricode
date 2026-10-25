// apps/mirri-web/test/ws-rpc-event-routing.test.ts
// Given the rust engine's direct-form event frames (`event.<type>` method
// names on the unified RPC wire), the WsRpcApi event bridge must route every
// frame through classifyFrame: agent-core frames reach the projector with the
// prefix stripped, genuine protocol frames reach toAppEvent, control frames
// are dropped, and the `approval.ask` / `question.ask` reverse RPCs surface
// full-fidelity approval/question cards that the existing UI resolve paths
// can answer.

import { describe, expect, it, vi } from 'vitest';

import { createAgentProjector } from '../src/api/daemon/agentEventProjector';
import {
  approvalRequestFromAskParams,
  approvalResponseToEngineWire,
  questionRequestFromAskParams,
  questionResponseToEngineWire,
  routeRpcEventToAppEvents,
} from '../src/api/daemon/ws-rpc-event-bridge';
import { JsonRpcClient } from '../src/api/daemon/json-rpc-client';
import type { RpcEvent } from '../src/api/daemon/json-rpc-client';
import { WsRpcApi } from '../src/api/daemon/ws-rpc-api';
import type { MirriEventHandlers } from '../src/api/types';

function rpcEvent(type: string, payload: unknown, sessionId = 's1', seq = 1): RpcEvent {
  return { type, sessionId, seq, epoch: undefined, volatile: false, payload };
}

describe('routeRpcEventToAppEvents', () => {
  it('routes a prefixed agent-core event to the projector when the engine sends the direct form', () => {
    const projector = createAgentProjector();
    routeRpcEventToAppEvents(projector, rpcEvent('event.turn.started', { turnId: 1 }));
    routeRpcEventToAppEvents(projector, rpcEvent('event.turn.step.started', { turnId: 1, step: 1 }));
    const events = routeRpcEventToAppEvents(
      projector,
      rpcEvent('event.assistant.delta', { turnId: 1, delta: 'step one' }),
    );
    expect(events).toContainEqual(
      expect.objectContaining({ type: 'assistantDelta', delta: { text: 'step one' } }),
    );
  });

  it('keeps routing unprefixed agent-core events for backward compatibility', () => {
    const projector = createAgentProjector();
    routeRpcEventToAppEvents(projector, rpcEvent('turn.started', { turnId: 1 }));
    routeRpcEventToAppEvents(projector, rpcEvent('turn.step.started', { turnId: 1, step: 1 }));
    const events = routeRpcEventToAppEvents(projector, rpcEvent('assistant.delta', { turnId: 1, delta: 'legacy' }));
    expect(events).toContainEqual(
      expect.objectContaining({ type: 'assistantDelta', delta: { text: 'legacy' } }),
    );
  });

  it('routes a genuine protocol event through toAppEvent when the engine sends event.approval.requested', () => {
    const projector = createAgentProjector();
    const events = routeRpcEventToAppEvents(
      projector,
      rpcEvent('event.approval.requested', {
        agentId: 'main',
        requestId: 'req-1',
        toolName: 'bash',
      }),
    );
    expect(events).toEqual([
      expect.objectContaining({
        type: 'approvalRequested',
        sessionId: 's1',
        approval: expect.objectContaining({ approvalId: 'req-1', toolName: 'bash' }),
      }),
    ]);
  });

  it('drops control frames instead of feeding them to the projector', () => {
    const projector = createAgentProjector();
    expect(routeRpcEventToAppEvents(projector, rpcEvent('ping', {}))).toEqual([]);
    expect(routeRpcEventToAppEvents(projector, rpcEvent('pong', {}))).toEqual([]);
  });

  it('drops frames without a session scope unless they are workspace lifecycle events', () => {
    const projector = createAgentProjector();
    const event: RpcEvent = { type: 'event.assistant.delta', sessionId: undefined, seq: 1, epoch: undefined, volatile: false, payload: { delta: 'x' } };
    expect(routeRpcEventToAppEvents(projector, event)).toEqual([]);
  });

  it('routes a sessionless workspace created event through toAppEvent with the rust dialect', () => {
    // Given a server-scoped `event.workspace.created` frame with no session
    // id (the W2 synthesized empty-session envelope)
    const projector = createAgentProjector();
    const event: RpcEvent = {
      type: 'event.workspace.created',
      sessionId: '',
      seq: undefined,
      epoch: undefined,
      volatile: true,
      payload: {
        workspace: {
          id: 'wd_project_abc123def456',
          root: '/Users/dev/project',
          name: 'project',
        },
      },
    };

    // When it is routed through the event bridge
    const events = routeRpcEventToAppEvents(projector, event);

    // Then a workspaceCreated AppEvent reaches the applyWorkspaceEvent
    // consumer with the id/root/name and honest count/git defaults
    // (the rust camelCase dialect fills isGitRepo/sessionCount).
    expect(events).toEqual([
      {
        type: 'workspaceCreated',
        workspace: {
          id: 'wd_project_abc123def456',
          root: '/Users/dev/project',
          name: 'project',
          isGitRepo: false,
          sessionCount: 0,
          // W8: the event snapshot's honest trust default (the lifecycle
          // event carries no trust field — a listed/refreshed row is the
          // authoritative read).
          trusted: true,
        },
      },
    ]);
  });

  it('routes a sessionless workspace updated event with the renamed workspace', () => {
    // Given a server-scoped `event.workspace.updated` frame (rust dialect,
    // empty session id)
    const projector = createAgentProjector();
    const event: RpcEvent = {
      type: 'event.workspace.updated',
      sessionId: undefined,
      seq: undefined,
      epoch: undefined,
      volatile: true,
      payload: {
        workspace: { id: 'wd_project_abc123def456', root: '/Users/dev/project', name: 'renamed' },
      },
    };

    // When it is routed through the event bridge
    const events = routeRpcEventToAppEvents(projector, event);

    // Then a workspaceUpdated AppEvent reaches the consumer with the
    // renamed workspace
    expect(events).toEqual([
      {
        type: 'workspaceUpdated',
        workspace: {
          id: 'wd_project_abc123def456',
          root: '/Users/dev/project',
          name: 'renamed',
          isGitRepo: false,
          sessionCount: 0,
          trusted: true,
        },
      },
    ]);
  });

  it('routes a sessionless workspace deleted event with the camelCase id payload', () => {
    // Given a server-scoped `event.workspace.deleted` frame carrying the
    // rust engine's camelCase `workspaceId` (empty session id)
    const projector = createAgentProjector();
    const event: RpcEvent = {
      type: 'event.workspace.deleted',
      sessionId: '',
      seq: undefined,
      epoch: undefined,
      volatile: true,
      payload: { workspaceId: 'wd_project_abc123def456', root: '/Users/dev/project' },
    };

    // When it is routed through the event bridge
    const events = routeRpcEventToAppEvents(projector, event);

    // Then a workspaceDeleted AppEvent reaches the consumer with the id
    // and root the applyWorkspaceEvent hidden-root logic reads
    expect(events).toEqual([
      {
        type: 'workspaceDeleted',
        workspaceId: 'wd_project_abc123def456',
        root: '/Users/dev/project',
      },
    ]);
  });
});

describe('approval.ask bridge', () => {
  it('maps the engine ask params onto a full approval request', () => {
    const approval = approvalRequestFromAskParams(
      {
        tool_name: 'bash',
        tool_call_id: 'call-1',
        action: 'execute bash',
        display: { kind: 'command', command: 'pnpm test' },
        reason: 'approval required',
        request_id: 'req-9',
      },
      's1',
    );
    expect(approval).toEqual(
      expect.objectContaining({
        approvalId: 'req-9',
        sessionId: 's1',
        toolCallId: 'call-1',
        toolName: 'bash',
        action: 'execute bash',
      }),
    );
  });

  it('returns null when the ask params are malformed', () => {
    expect(approvalRequestFromAskParams({ tool_name: 'bash' }, 's1')).toBeNull();
    expect(approvalRequestFromAskParams('nope', 's1')).toBeNull();
  });

  it('maps the UI response onto the engine wire shape', () => {
    expect(approvalResponseToEngineWire({ decision: 'approved' })).toEqual({ decision: 'approved' });
    expect(
      approvalResponseToEngineWire({ decision: 'rejected', feedback: 'not allowed' }),
    ).toEqual({ decision: 'rejected', feedback: 'not allowed' });
  });

  it('drops the selected_label the rust engine does not read from the wire shape', () => {
    expect(
      approvalResponseToEngineWire({ decision: 'approved', selectedLabel: 'Always allow' }),
    ).toEqual({ decision: 'approved' });
  });
});

describe('question.ask bridge', () => {
  it('maps the engine ask params onto a question request with stable item ids', () => {
    const question = questionRequestFromAskParams(
      {
        request_id: 'req-q',
        questions: [
          {
            question: 'Which language?',
            header: 'Lang',
            options: [
              { label: 'Rust', description: 'safe and fast' },
              { label: 'TypeScript' },
            ],
            multi_select: false,
          },
        ],
      },
      's1',
    );
    expect(question).toEqual(
      expect.objectContaining({
        questionId: 'req-q',
        sessionId: 's1',
        questions: [
          expect.objectContaining({
            id: 'q0',
            question: 'Which language?',
            header: 'Lang',
            // The engine always advertises the free-text Other option —
            // matching the tool's model-facing description and the TS wire.
            allowOther: true,
            options: [
              expect.objectContaining({ label: 'Rust', description: 'safe and fast' }),
              expect.objectContaining({ label: 'TypeScript' }),
            ],
          }),
        ],
      }),
    );
  });

  it('returns null when the questions array is missing or empty', () => {
    expect(questionRequestFromAskParams({ request_id: 'req-q' }, 's1')).toBeNull();
    expect(questionRequestFromAskParams({ request_id: 'req-q', questions: [] }, 's1')).toBeNull();
  });

  it('maps the UI answers onto the engine wire shape the tool result renders', () => {
    expect(
      questionResponseToEngineWire({
        answers: { q0: { kind: 'single', optionId: 'opt-1' } },
      }),
    ).toEqual({ answers: { q0: 'opt-1' } });
  });
});

describe('event.question.requested routing', () => {
  it('should render the real card when the event carries full question items', () => {
    // Given a rust engine question event carrying the tool's full items
    const projector = createAgentProjector();
    const events = routeRpcEventToAppEvents(
      projector,
      rpcEvent('event.question.requested', {
        agentId: 'main',
        requestId: 'req-1',
        question: '这批本地变更要开成一个 PR，还是按主题拆分？',
        questions: [
          {
            id: 'q_0',
            header: 'PR 结构',
            question: '这批本地变更要开成一个 PR，还是按主题拆分？',
            options: [
              { id: '一个 PR，分组描述', label: '一个 PR，分组描述' },
              { id: '按主题拆 4-5 个 PR', label: '按主题拆 4-5 个 PR' },
            ],
          },
        ],
      }),
    );

    // Then the card keeps the options and the header instead of degrading to
    // a question text with an empty option list
    expect(events).toHaveLength(1);
    const event = events[0] as Extract<(typeof events)[number], { type: 'questionRequested' }>;
    expect(event.type).toBe('questionRequested');
    expect(event.question.questionId).toBe('req-1');
    expect(event.question.questions).toHaveLength(1);
    expect(event.question.questions[0]!.header).toBe('PR 结构');
    expect(event.question.questions[0]!.options.map((o) => o.label)).toEqual([
      '一个 PR，分组描述',
      '按主题拆 4-5 个 PR',
    ]);
  });

  it('should fall back to a single-question card when the event carries no items', () => {
    // Given an engine that predates the full payload
    const projector = createAgentProjector();
    const events = routeRpcEventToAppEvents(
      projector,
      rpcEvent('event.question.requested', {
        agentId: 'main',
        requestId: 'req-2',
        question: 'Continue?',
      }),
    );

    // Then the summary still surfaces so the user is not left with no card
    expect(events).toHaveLength(1);
    const event = events[0] as Extract<(typeof events)[number], { type: 'questionRequested' }>;
    expect(event.question.questions[0]!.question).toBe('Continue?');
    expect(event.question.questions[0]!.options).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// WsRpcApi end-to-end routing (mock client.call pattern)
// ---------------------------------------------------------------------------

/**
 * Build a WsRpcApi whose transport is a mocked client (the same pattern the
 * approval-flow suite uses), returning the installed catch-all handler.
 */
function createApiWithAnyHandler(): {
  api: WsRpcApi;
  handlers: MirriEventHandlers;
  anyHandler: (event: RpcEvent) => void;
  askHandler: (method: string) => (params: unknown) => Promise<unknown>;
} {
  const spy = vi.spyOn(JsonRpcClient.prototype, 'registerReverseRpc');
  const api = new WsRpcApi('ws://test.local/api/v1/rpc', 'http://test.local', undefined);
  const client = {
    call: vi.fn(),
    on: vi.fn(() => () => {}),
    onAnyEvent: vi.fn(() => () => {}),
    close: vi.fn(),
  };
  (api as unknown as { client: unknown }).client = client;
  const handlers: MirriEventHandlers = {
    onEvent: vi.fn(),
    onResync: vi.fn(),
    onError: vi.fn(),
    onConnectionChange: vi.fn(),
  };
  api.connectEvents(handlers);
  const registration = client.onAnyEvent.mock.calls[0];
  if (registration === undefined) throw new Error('connectEvents did not install onAnyEvent');
  const askHandler = (method: string) => {
    const askRegistration = spy.mock.calls.find(([registered]) => registered === method);
    if (askRegistration === undefined) throw new Error(`no reverse-RPC handler for ${method}`);
    return askRegistration[1];
  };
  return { api, handlers, anyHandler: registration[0], askHandler };
}

describe('WsRpcApi workspace event routing', () => {
  it('should deliver a workspace created event to the UI while keeping the last session pointer when the engine emits both on one connection', () => {
    // Given a WsRpcApi whose transport is a mocked client, with a
    // session-scoped event already received (the last-session pointer is
    // set) and an approval.ask parked against that session
    const { handlers, anyHandler, askHandler } = createApiWithAnyHandler();
    anyHandler(rpcEvent('event.turn.started', { turnId: 1 }, 'sess-1', 1));

    // When the engine emits a server-scoped workspace created event (the
    // W2 synthesized empty session id)…
    anyHandler({
      type: 'event.workspace.created',
      sessionId: '',
      seq: undefined,
      epoch: undefined,
      volatile: true,
      payload: { workspace: { id: 'wd_other_000000000001', root: '/Users/dev/other', name: 'other' } },
    });

    // …then the UI handler receives the mirrored workspaceCreated AppEvent
    const workspaceCalls = (handlers.onEvent as ReturnType<typeof vi.fn>).mock.calls.filter(
      ([appEvent]) => (appEvent as { type: string }).type === 'workspaceCreated',
    );
    expect(workspaceCalls).toHaveLength(1);
    expect(workspaceCalls[0][0]).toEqual({
      type: 'workspaceCreated',
      workspace: {
        id: 'wd_other_000000000001',
        root: '/Users/dev/other',
        name: 'other',
        isGitRepo: false,
        sessionCount: 0,
        trusted: true,
      },
    });
    // …and the meta carries the synthesized empty session id (the
    // applyWorkspaceEvent consumer ignores it, but the type demands a string)
    expect(workspaceCalls[0][1]).toEqual({ sessionId: '', seq: 0 });

    // And a later approval.ask still routes to the PREVIOUS session — the
    // workspace event must not clobber the last-session pointer
    const call = askHandler('approval.ask');
    void call({
      tool_name: 'bash',
      tool_call_id: 'call-1',
      action: 'execute bash',
      request_id: 'req-1',
    });
    const askCalls = (handlers.onEvent as ReturnType<typeof vi.fn>).mock.calls.filter(
      ([appEvent]) => (appEvent as { type: string }).type === 'approvalRequested',
    );
    expect(askCalls[0][1]).toEqual({ sessionId: 'sess-1', seq: 0 });
  });
});
