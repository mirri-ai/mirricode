import { describe, expect, it } from 'vitest';

import type { Event } from '#/events';
import { translateDomainEvent, translateGlobalEvent } from '#/v2/event-mapper';

describe('translateDomainEvent', () => {
  it('should stamp sessionId and agentId onto a translated event', () => {
    const event = {
      type: 'turn.started',
      turnId: 3,
      prompt: 'hello',
    };

    const result = translateDomainEvent(event as never, 'sess-1', 'main');

    expect(result).toEqual({
      type: 'turn.started',
      turnId: 3,
      prompt: 'hello',
      sessionId: 'sess-1',
      agentId: 'main',
    });
  });

  it('should rename task lifecycle events to the legacy background.task spelling', () => {
    const started = translateDomainEvent({ type: 'task.started' } as never, 's', 'main');
    const terminated = translateDomainEvent({ type: 'task.terminated' } as never, 's', 'main');

    expect(started?.type).toBe('background.task.started');
    expect(terminated?.type).toBe('background.task.terminated');
  });

  it('should drop v2-internal events that have no v1 protocol counterpart', () => {
    for (const type of [
      'agent.activity.updated',
      'context.spliced',
      'task.notified',
      'plan.revision',
      'permission.approval.requested',
      'permission.approval.resolved',
      'prompt.completed',
    ]) {
      expect(translateDomainEvent({ type } as never, 's', 'main')).toBeUndefined();
    }
  });

  it('should pass through all other event payloads unchanged', () => {
    const result = translateDomainEvent(
      { type: 'assistant.delta', turnId: 1, delta: 'hello' } as never,
      's',
      'main',
    );
    const expected: Event = {
      type: 'assistant.delta',
      turnId: 1,
      delta: 'hello',
      sessionId: 's',
      agentId: 'main',
    };
    expect(result).toEqual(expected);
  });
});

describe('translateGlobalEvent', () => {
  it('should unwrap session.meta.updated from the envelope', () => {
    const result = translateGlobalEvent({
      type: 'session.meta.updated',
      payload: { sessionId: 's', title: 'New' },
    });

    expect(result).toEqual({ type: 'session.meta.updated', sessionId: 's', title: 'New' });
  });

  it('should drop everything other than session.meta.updated', () => {
    expect(
      translateGlobalEvent({ type: 'some.other.event', payload: { a: 1 } }),
    ).toBeUndefined();
    expect(translateGlobalEvent({ type: 'session.meta.updated', payload: 42 })).toBeUndefined();
  });
});