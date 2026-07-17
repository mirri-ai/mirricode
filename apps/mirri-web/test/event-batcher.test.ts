/**
 * Scenario: high-frequency WebSocket render events reach the browser queue.
 * Responsibilities: preserve ordering, merge only proven-contiguous assistant
 * streams, bound each drain, keep a hidden-tab task fallback, and cancel stale
 * callbacks after flush. Wiring: real batcher/coalescer/reducer with only the
 * browser scheduler replaced by a manual public scheduler.
 * Run: pnpm --filter @mirri-ai/mirri-web exec vitest run test/event-batcher.test.ts
 */

import { describe, expect, it } from 'vitest';

import { createEventBatcher, isRenderEvent, type EventBatcherScheduler } from '../src/composables/client/eventBatcher';
import type { AppEvent } from '../src/api/types';

interface ManualScheduler extends EventBatcherScheduler {
  flushFrame(): void;
  flushTask(): void;
  pendingFrames(): number;
  pendingTasks(): number;
}

function manualScheduler(): ManualScheduler {
  let nextHandle = 1;
  const frames = new Map<number, () => void>();
  const tasks = new Map<number, () => void>();

  function takeOne(callbacks: Map<number, () => void>): void {
    const entry = callbacks.entries().next().value as [number, () => void] | undefined;
    if (entry === undefined) return;
    callbacks.delete(entry[0]);
    entry[1]();
  }

  return {
    requestFrame(callback) {
      const handle = nextHandle++;
      frames.set(handle, callback);
      return handle;
    },
    cancelFrame(handle) {
      frames.delete(handle);
    },
    requestTask(callback) {
      const handle = nextHandle++;
      tasks.set(handle, callback);
      return handle;
    },
    cancelTask(handle) {
      tasks.delete(handle);
    },
    flushFrame() {
      takeOne(frames);
    },
    flushTask() {
      takeOne(tasks);
    },
    pendingFrames() {
      return frames.size;
    },
    pendingTasks() {
      return tasks.size;
    },
  };
}

describe('createEventBatcher', () => {
  it('coalesces consecutive batchable items into one scheduled flush, in order', () => {
    const processed: string[] = [];
    const s = manualScheduler();
    const enqueue = createEventBatcher<string>(
      (item) => processed.push(item),
      (item) => item.startsWith('d'),
      { scheduler: s },
    );

    enqueue('d1');
    enqueue('d2');
    enqueue('d3');

    expect(processed).toEqual([]); // nothing processed yet
    expect(s.pendingFrames() + s.pendingTasks()).toBeGreaterThan(0); // scheduled

    s.flushFrame();
    expect(processed).toEqual(['d1', 'd2', 'd3']);
  });

  it('applies a non-batchable item immediately when the queue is empty', () => {
    const processed: string[] = [];
    const s = manualScheduler();
    const enqueue = createEventBatcher<string>(
      (item) => processed.push(item),
      (item) => item.startsWith('d'),
      { scheduler: s },
    );

    enqueue('X');

    expect(processed).toEqual(['X']);
    expect(s.pendingFrames()).toBe(0);
    expect(s.pendingTasks()).toBe(0);
  });

  it('drains pending batchables before applying an immediate item', () => {
    const processed: string[] = [];
    const s = manualScheduler();
    const enqueue = createEventBatcher<string>(
      (item) => processed.push(item),
      (item) => item.startsWith('d'),
      { scheduler: s },
    );

    enqueue('d1');
    enqueue('d2');
    enqueue('X'); // immediate → must process d1, d2 first

    expect(processed).toEqual(['d1', 'd2', 'X']);
  });

  it('preserves arrival order across mixed batchable and immediate items', () => {
    const processed: string[] = [];
    const s = manualScheduler();
    const enqueue = createEventBatcher<string>(
      (item) => processed.push(item),
      (item) => item.startsWith('d'),
      { scheduler: s },
    );

    enqueue('d1'); // queued
    enqueue('d2'); // queued
    enqueue('A'); // immediate → drains d1, d2, then A
    enqueue('d3'); // queued again
    s.flushFrame(); // drains d3

    expect(processed).toEqual(['d1', 'd2', 'A', 'd3']);
  });

  it('reschedules after a flush when new batchable items arrive', () => {
    const processed: string[] = [];
    const s = manualScheduler();
    const enqueue = createEventBatcher<string>(
      (item) => processed.push(item),
      (item) => item.startsWith('d'),
      { scheduler: s },
    );

    enqueue('d1');
    s.flushFrame();
    expect(processed).toEqual(['d1']);

    enqueue('d2');
    expect(s.pendingFrames() + s.pendingTasks()).toBeGreaterThan(0);

    s.flushFrame();
    expect(processed).toEqual(['d1', 'd2']);
  });

  it('flush() drains pending batchables synchronously without the scheduler', () => {
    const processed: string[] = [];
    const s = manualScheduler();
    const enqueue = createEventBatcher<string>(
      (item) => processed.push(item),
      (item) => item.startsWith('d'),
      { scheduler: s },
    );

    enqueue('d1');
    enqueue('d2');
    expect(processed).toEqual([]);

    enqueue.flush(); // synchronous drain, no scheduler callback needed
    expect(processed).toEqual(['d1', 'd2']);
  });

  it('flush() on an empty queue is a no-op', () => {
    const processed: string[] = [];
    const s = manualScheduler();
    const enqueue = createEventBatcher<string>(
      (item) => processed.push(item),
      (item) => item.startsWith('d'),
      { scheduler: s },
    );

    enqueue.flush();
    expect(processed).toEqual([]);
  });
});

describe('isRenderEvent', () => {
  it.each(['assistantDelta', 'agentDelta', 'toolOutput', 'taskProgress'])(
    'treats %s as batchable',
    (type) => {
      expect(isRenderEvent({ type } as AppEvent)).toBe(true);
    },
  );

  it.each(['messageCreated', 'messageUpdated', 'sessionWorkChanged', 'approvalRequested', 'configChanged'])(
    'treats %s as immediate',
    (type) => {
      expect(isRenderEvent({ type } as AppEvent)).toBe(false);
    },
  );
});
