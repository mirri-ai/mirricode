import { describe, expect, it, vi } from 'vitest';

import { SessionEventHandler } from '#/tui/controllers/session-event-handler';
import { getBuiltInPalette } from '#/tui/theme';

/**
 * The model-switch divider's live half (spec 11 §2.5 A2): the rust engine's
 * durable `event.session.model_switched {from, to}` arrives through the
 * node-sdk translation as a `session.meta.updated` event whose free-form
 * patch carries the `modelSwitched` member (the v1 `Event` union has no
 * first-class variant — see translate.ts), and the handler must render the
 * divider and follow the session's new model.
 */
describe('SessionEventHandler model switch divider', () => {
  function makeHost() {
    const host = {
      state: {
        appState: {
          sessionId: 's1',
          streamingPhase: 'idle',
          model: 'model-a',
          permissionMode: 'auto',
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
      },
      requireSession: vi.fn(),
      setAppState: vi.fn(),
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

  function translatedSwitchEvent(from: string, to: string) {
    return {
      type: 'session.meta.updated',
      sessionId: 's1',
      agentId: 'main',
      patch: { modelSwitched: { from, to } },
    } as const;
  }

  it('appends the divider entry and follows the new model when the translated switch event arrives', () => {
    // Given a session streaming on model-a when the engine switches to model-b
    const { host } = makeHost();
    const handler = new SessionEventHandler(host);

    // When the translated model_switched event arrives (riding the meta patch)
    handler.handleEvent(translatedSwitchEvent('model-a', 'model-b'), vi.fn());

    // Then a model_switch divider entry is appended to the transcript
    expect(host.appendTranscriptEntry).toHaveBeenCalledWith(
      expect.objectContaining({
        kind: 'model_switch',
        renderMode: 'plain',
        modelSwitchData: { from: 'model-a', to: 'model-b' },
      }),
    );
    // And the app state's model follows the switch (the footer stays in sync)
    expect(host.setAppState).toHaveBeenCalledWith({ model: 'model-b' });
  });

  it('still handles title updates riding the same event', () => {
    // Given a switch event that also renames the session
    const { host } = makeHost();
    const handler = new SessionEventHandler(host);

    // When the event carries both a title and the modelSwitched patch member
    handler.handleEvent(
      {
        type: 'session.meta.updated',
        sessionId: 's1',
        agentId: 'main',
        title: 'renamed',
        patch: { modelSwitched: { from: 'model-a', to: 'model-b' } },
      },
      vi.fn(),
    );

    // Then both the title and the divider are handled
    expect(host.setAppState).toHaveBeenCalledWith({ sessionTitle: 'renamed' });
    expect(host.appendTranscriptEntry).toHaveBeenCalledWith(
      expect.objectContaining({ kind: 'model_switch' }),
    );
  });

  it('ignores meta updates without a model switch', () => {
    // Given a plain session rename
    const { host } = makeHost();
    const handler = new SessionEventHandler(host);

    // When the meta update carries no modelSwitched patch member
    handler.handleEvent(
      {
        type: 'session.meta.updated',
        sessionId: 's1',
        agentId: 'main',
        title: 'renamed',
        patch: { title: 'renamed' },
      },
      vi.fn(),
    );

    // Then no divider entry is appended and the model state is untouched
    expect(host.appendTranscriptEntry).not.toHaveBeenCalled();
    const modelPatches = host.setAppState.mock.calls.filter(
      ([patch]: [Record<string, unknown>]) => 'model' in patch,
    );
    expect(modelPatches).toEqual([]);
  });

  it('ignores a malformed modelSwitched patch member', () => {
    // Given a corrupted patch member
    const { host } = makeHost();
    const handler = new SessionEventHandler(host);

    // When the event arrives
    handler.handleEvent(
      {
        type: 'session.meta.updated',
        sessionId: 's1',
        agentId: 'main',
        patch: { modelSwitched: { from: 'model-a' } },
      },
      vi.fn(),
    );

    // Then no divider renders and the model state is untouched
    expect(host.appendTranscriptEntry).not.toHaveBeenCalled();
    expect(host.setAppState).not.toHaveBeenCalled();
  });
});
