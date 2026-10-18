import { describe, expect, it, vi } from 'vitest';

import { SessionEventHandler } from '#/tui/controllers/session-event-handler';
import { getBuiltInPalette } from '#/tui/theme';
import { INITIAL_SESSION_USAGE } from '#/tui/types';

// The footer usage-stats source (usage-visibility Task 8): every
// `turn.step.completed` event carries the step's token usage in the klient
// dialect (inputOther / output / inputCacheRead / inputCacheCreation), and the
// handler accumulates the four buckets into `AppState.sessionUsage` so the
// footer can render session-level cache hit + in/out totals.
describe('SessionEventHandler step usage accumulation', () => {
  function makeHost() {
    const host = {
      state: {
        appState: {
          sessionId: 's1',
          streamingPhase: 'idle',
          model: 'model-a',
          permissionMode: 'auto',
          sessionUsage: { ...INITIAL_SESSION_USAGE },
        },
        queuedMessages: [],
        queuedMessageDispatchPending: false,
        theme: { palette: getBuiltInPalette('dark') },
        toolOutputExpanded: false,
        todoPanel: { getTodos: vi.fn(() => []) },
        transcriptContainer: { addChild: vi.fn() },
        ui: { requestRender: vi.fn() },
      },
      session: undefined,
      aborted: false,
      sessionEventUnsubscribe: undefined,
      streamingUI: {
        setTurnId: vi.fn(),
        flushNow: vi.fn(),
        resetToolUi: vi.fn(),
        finalizeTurn: vi.fn(),
        hasThinkingDraft: vi.fn(() => false),
        flushThinkingToTranscript: vi.fn(),
        appendAssistantDelta: vi.fn(),
        scheduleFlush: vi.fn(),
        getTurnContext: vi.fn(() => ({ turnId: 'turn-1', step: 0 })),
        markStepTruncated: vi.fn(() => 0),
      },
      requireSession: vi.fn(),
      setAppState: vi.fn((patch: Record<string, unknown>) => {
        Object.assign(host.state.appState, patch);
      }),
      patchLivePane: vi.fn(),
      resetLivePane: vi.fn(),
      showError: vi.fn(),
      showStatus: vi.fn(),
      showNotice: vi.fn(),
      track: vi.fn(),
      mountEditorReplacement: vi.fn(),
      restoreEditor: vi.fn(),
      restoreInputText: vi.fn(),
      appendTranscriptEntry: vi.fn(),
      updateActivityPane: vi.fn(),
      updateTerminalTitle: vi.fn(),
      sendNormalUserInput: vi.fn(),
      sendQueuedMessage: vi.fn(),
      shiftQueuedMessage: vi.fn(),
      handleShellOutput: vi.fn(),
      handleShellStarted: vi.fn(),
      btwPanelController: { routeEvent: vi.fn(() => false) },
      tasksBrowserController: {},
    };
    return { host: host as any };
  }

  function stepCompletedEvent(
    step: number,
    usage?: {
      inputOther: number;
      output: number;
      inputCacheRead: number;
      inputCacheCreation: number;
    },
    agentId = 'main',
  ) {
    return {
      type: 'turn.step.completed',
      sessionId: 's1',
      agentId,
      turnId: 7,
      step,
      finishReason: 'stop',
      ...(usage === undefined ? {} : { usage }),
    } as const;
  }

  it('accumulates the four buckets across steps into sessionUsage', () => {
    const { host } = makeHost();
    const handler = new SessionEventHandler(host);

    handler.handleEvent(
      stepCompletedEvent(1, {
        inputOther: 1_000,
        output: 500,
        inputCacheRead: 20_000,
        inputCacheCreation: 3_000,
      }),
      vi.fn(),
    );
    handler.handleEvent(
      stepCompletedEvent(2, {
        inputOther: 2_048,
        output: 1_024,
        inputCacheRead: 40_960,
        inputCacheCreation: 1_024,
      }),
      vi.fn(),
    );

    expect(host.state.appState.sessionUsage).toEqual({
      inputTokens: 3_048,
      outputTokens: 1_524,
      cacheReadTokens: 60_960,
      cacheCreationTokens: 4_024,
    });
  });

  it('ignores steps without a usage payload (state untouched)', () => {
    const { host } = makeHost();
    const handler = new SessionEventHandler(host);

    handler.handleEvent(stepCompletedEvent(1), vi.fn());

    const usagePatches = host.setAppState.mock.calls.filter(
      ([patch]: [Record<string, unknown>]) => 'sessionUsage' in patch,
    );
    expect(usagePatches).toEqual([]);
    expect(host.state.appState.sessionUsage).toEqual({ ...INITIAL_SESSION_USAGE });
  });

  it('still accumulates when the step was provider-filtered (tokens were consumed)', () => {
    const { host } = makeHost();
    const handler = new SessionEventHandler(host);

    handler.handleEvent(
      {
        ...stepCompletedEvent(1, {
          inputOther: 10,
          output: 0,
          inputCacheRead: 90,
          inputCacheCreation: 0,
        }),
        providerFinishReason: 'filtered',
      },
      vi.fn(),
    );

    expect(host.state.appState.sessionUsage).toEqual({
      inputTokens: 10,
      outputTokens: 0,
      cacheReadTokens: 90,
      cacheCreationTokens: 0,
    });
    // The filtered notice still fires.
    expect(host.showNotice).toHaveBeenCalled();
  });

  it('counts a subagent step even though routeChildAgentEvent swallows it (billing scope = main + subagents)', () => {
    const { host } = makeHost();
    const handler = new SessionEventHandler(host);

    // The child-agent event never reaches the switch (the router returns
    // true for an unknown subagent id), but its tokens still land in the
    // session total.
    handler.handleEvent(
      stepCompletedEvent(1, {
        inputOther: 512,
        output: 256,
        inputCacheRead: 8_192,
        inputCacheCreation: 1_024,
      }, 'agent-child-1'),
      vi.fn(),
    );

    expect(host.state.appState.sessionUsage).toEqual({
      inputTokens: 512,
      outputTokens: 256,
      cacheReadTokens: 8_192,
      cacheCreationTokens: 1_024,
    });
  });

  it('counts a main-agent step exactly once (pre-routing accumulation must not double-count)', () => {
    const { host } = makeHost();
    const handler = new SessionEventHandler(host);

    handler.handleEvent(
      stepCompletedEvent(1, {
        inputOther: 1_000,
        output: 500,
        inputCacheRead: 20_000,
        inputCacheCreation: 3_000,
      }),
      vi.fn(),
    );

    // Exactly one accumulation patch, and the totals are the single-step
    // values — not doubled.
    const usagePatches = host.setAppState.mock.calls.filter(
      ([patch]: [Record<string, unknown>]) => 'sessionUsage' in patch,
    );
    expect(usagePatches).toHaveLength(1);
    expect(host.state.appState.sessionUsage).toEqual({
      inputTokens: 1_000,
      outputTokens: 500,
      cacheReadTokens: 20_000,
      cacheCreationTokens: 3_000,
    });
  });
});
