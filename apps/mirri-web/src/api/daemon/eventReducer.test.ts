import { describe, expect, it } from 'vitest';

import {
  type EventMeta,
  type MirriClientState,
  createInitialState,
  reduceAppEvent,
} from './eventReducer';
import type { AppEvent, AppMessage } from '../types';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const SID = 'sess_1';
const META: EventMeta = { sessionId: SID, seq: 1 };

function makeUserMessage(overrides: Partial<AppMessage> & { id: string }): AppMessage {
  return {
    sessionId: SID,
    role: 'user',
    content: [{ type: 'text', text: 'hello' }],
    createdAt: '2025-01-01T00:00:00.000Z',
    ...overrides,
  };
}

function stateWithMessages(messages: AppMessage[]): MirriClientState {
  const state = createInitialState();
  state.messagesBySession[SID] = messages;
  return state;
}

function messageCreatedEvent(message: AppMessage): AppEvent {
  return { type: 'messageCreated', message };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe('messageCreated dual-echo dedup', () => {
  it('should skip second user messageCreated when promptId already exists in transcript', () => {
    // Given: a user message already merged (NOT optimistic anymore) with promptId "pr_abc"
    const existingUserMessage = makeUserMessage({
      id: 'msg_first_echo',
      promptId: 'pr_abc',
    });
    const state = stateWithMessages([existingUserMessage]);

    // When: a messageCreated event arrives for a user message with the same promptId but different id
    const secondEcho = makeUserMessage({
      id: 'msg_second_echo',
      promptId: 'pr_abc',
    });
    const result = reduceAppEvent(state, messageCreatedEvent(secondEcho), { ...META, seq: 2 });

    // Then: the message list has exactly one user message (the second was deduped)
    const messages = result.messagesBySession[SID]!;
    const userMessages = messages.filter((m) => m.role === 'user');
    expect(userMessages).toHaveLength(1);
    expect(userMessages[0]!.id).toBe('msg_first_echo');
  });

  it('should not dedup user messages with different promptIds', () => {
    // Given: a user message with promptId "pr_abc"
    const firstMessage = makeUserMessage({
      id: 'msg_1',
      promptId: 'pr_abc',
    });
    const state = stateWithMessages([firstMessage]);

    // When: a messageCreated event arrives for a user message with a different promptId "pr_xyz"
    const secondMessage = makeUserMessage({
      id: 'msg_2',
      promptId: 'pr_xyz',
    });
    const result = reduceAppEvent(state, messageCreatedEvent(secondMessage), { ...META, seq: 2 });

    // Then: both messages exist
    const messages = result.messagesBySession[SID]!;
    const userMessages = messages.filter((m) => m.role === 'user');
    expect(userMessages).toHaveLength(2);
  });

  it('should not dedup user messages when promptId is undefined', () => {
    // Given: a user message without promptId
    const firstMessage = makeUserMessage({
      id: 'msg_1',
      // promptId deliberately omitted
    });
    const state = stateWithMessages([firstMessage]);

    // When: a messageCreated event arrives for a user message also without promptId
    const secondMessage = makeUserMessage({
      id: 'msg_2',
      // promptId deliberately omitted
    });
    const result = reduceAppEvent(state, messageCreatedEvent(secondMessage), { ...META, seq: 2 });

    // Then: existing behavior preserved (append, no false dedup)
    const messages = result.messagesBySession[SID]!;
    const userMessages = messages.filter((m) => m.role === 'user');
    expect(userMessages).toHaveLength(2);
  });

  it('should dedup second echo after optimistic message is merged by first echo', () => {
    // Given: an optimistic user message (as the client creates on submit)
    const optimisticMsg = makeUserMessage({
      id: 'msg_opt_1',
      promptId: undefined, // not yet stamped by submit response
      metadata: { 'mirriWeb.optimisticUserMessage': true },
    });
    const state = stateWithMessages([optimisticMsg]);

    // When: first echo arrives (prompt.submitted path) — merges into optimistic
    const firstEcho = makeUserMessage({
      id: 'msg_server_1',
      promptId: 'pr_abc',
      content: [{ type: 'text', text: 'hello' }],
    });
    const afterFirst = reduceAppEvent(state, messageCreatedEvent(firstEcho), { ...META, seq: 2 });

    // And: second echo arrives (event.message.created path) — same promptId, different id
    const secondEcho = makeUserMessage({
      id: 'msg_server_2',
      promptId: 'pr_abc',
      content: [{ type: 'text', text: 'hello' }],
    });
    const afterSecond = reduceAppEvent(afterFirst, messageCreatedEvent(secondEcho), { ...META, seq: 3 });

    // Then: exactly one user message remains (optimistic merged by first echo, second deduped)
    const messages = afterSecond.messagesBySession[SID]!;
    const userMessages = messages.filter((m) => m.role === 'user');
    expect(userMessages).toHaveLength(1);
    // The merged message keeps the optimistic id (reducer merges into existing slot)
    expect(userMessages[0]!.id).toBe('msg_opt_1');
  });
});
