// apps/mirri-web/test/tool-detach.test.ts
// The B7 W6 badge pipeline: detaching a running foreground Bash tool call
// (task.detach by toolCallId) must stamp the card with the background task
// id, and the stamp must survive the tool result that lands afterwards —
// the honest "not faked as completed" rule (US12.2).

import { describe, expect, it } from 'vitest';

import { createInitialState, reduceAppEvent, type MirriClientState } from '../src/api/daemon/eventReducer';
import { messagesToTurns } from '../src/composables/messagesToTurns';
import type { AppMessage } from '../src/api/types';

const SID = 's-1';

function assistantWithRunningBash(): AppMessage {
  return {
    id: 'msg-a1',
    sessionId: SID,
    role: 'assistant',
    content: [
      { type: 'toolUse', toolCallId: 'toolu-1', toolName: 'Bash', input: { command: 'sleep 30' } },
    ],
    createdAt: '2026-11-01T00:00:00Z',
  };
}

function toolResultMessage(): AppMessage {
  return {
    id: 'msg-t1',
    sessionId: SID,
    role: 'tool',
    content: [{ type: 'toolResult', toolCallId: 'toolu-1', output: 'Task moved to background' }],
    createdAt: '2026-11-01T00:01:00Z',
  };
}

function turnsOf(state: MirriClientState) {
  return messagesToTurns(
    state.messagesBySession[SID] ?? [],
    [],
    undefined,
    true,
    {},
    state.backgroundedToolCalls[SID] ?? {},
  );
}

function bashTool(state: MirriClientState) {
  const tool = turnsOf(state)
    .flatMap((t) => t.tools ?? [])
    .find((tc) => tc.id === 'toolu-1');
  expect(tool).toBeDefined();
  return tool!;
}

describe('toolCallBackgrounded — the run-in-background badge (B7 W6)', () => {
  it('should mark the addressed tool call backgrounded and never let a later result flip it off', () => {
    // Given a session transcript whose last assistant message holds a
    // running Bash tool call
    const state = createInitialState();
    state.messagesBySession[SID] = [assistantWithRunningBash()];
    expect(bashTool(state).backgroundedTaskId).toBeUndefined();

    // When App.vue's detach handler acks task.detach and dispatches the
    // web-internal toolCallBackgrounded event
    const detached = reduceAppEvent(
      state,
      { type: 'toolCallBackgrounded', sessionId: SID, toolCallId: 'toolu-1', taskId: 'task-9' },
      { sessionId: SID, seq: 0 },
    );

    // Then the card is stamped with the background task id
    expect(bashTool(detached).backgroundedTaskId).toBe('task-9');

    // And when the moved-to-background tool result lands (the turn settles)
    const settled = reduceAppEvent(
      detached,
      { type: 'messageCreated', message: toolResultMessage() },
      { sessionId: SID, seq: 1 },
    );

    // Then the badge survives — the status settles but the stamp stays
    const tool = bashTool(settled);
    expect(tool.backgroundedTaskId).toBe('task-9');
    expect(tool.status).toBe('ok');
  });

  it('should keep the other sessions untouched when one tool call is detached', () => {
    // Given the same running Bash call in the addressed session
    const state = createInitialState();
    state.messagesBySession[SID] = [assistantWithRunningBash()];

    // When the detach lands
    const detached = reduceAppEvent(
      state,
      { type: 'toolCallBackgrounded', sessionId: SID, toolCallId: 'toolu-1', taskId: 'task-9' },
      { sessionId: SID, seq: 0 },
    );

    // Then only the addressed session's map is populated
    expect(detached.backgroundedToolCalls[SID]).toEqual({ 'toolu-1': 'task-9' });
    expect(Object.keys(detached.backgroundedToolCalls)).toEqual([SID]);
  });
});
