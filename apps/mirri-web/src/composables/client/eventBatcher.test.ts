// apps/mirri-web/src/composables/client/eventBatcher.test.ts
import { describe, expect, it } from 'vitest';
import { coalesceAppRenderEvents, isRenderEvent, shouldDropUsageFrame } from './eventBatcher';
import type { AppEvent, MirriEventMeta } from '../../api/types';

const meta: MirriEventMeta = { sessionId: 's1', seq: 0 };

describe('render-event classification (SDR-003a frame coalescing)', () => {
  it('should classify usage, work-changed and meta events as batchable', () => {
    const usage: AppEvent = { type: 'sessionUsageUpdated', sessionId: 's1', usage: {} as never };
    const work: AppEvent = { type: 'sessionWorkChanged', sessionId: 's1', busy: false, mainTurnActive: false, pendingInteraction: 'none', lastTurnReason: 'completed' };
    const meta_: AppEvent = { type: 'sessionMetaUpdated', sessionId: 's1', title: 't' };
    expect(isRenderEvent(usage)).toBe(true);
    expect(isRenderEvent(work)).toBe(true);
    expect(isRenderEvent(meta_)).toBe(true);
    expect(isRenderEvent({ type: 'sessionCreated', session: {} as never })).toBe(false);
  });

  it('should keep only the latest usage snapshot while inheriting absent optional fields from the previous frame', () => {
    // Frame 1 carries swarmMode (a phase frame with the flag); frame 2 is a
    // turn-end usage snapshot without it. Field-level merge must keep the
    // flag — a plain "last frame wins" would drop it.
    const first: AppEvent = { type: 'sessionUsageUpdated', sessionId: 's1', usage: { inputTokens: 1 } as never, swarmMode: true };
    const second: AppEvent = { type: 'sessionUsageUpdated', sessionId: 's1', usage: { inputTokens: 2 } as never };
    const merged = coalesceAppRenderEvents(
      { appEvent: first, meta },
      { appEvent: second, meta },
    );
    const mergedEvent = merged!.appEvent as { usage: { inputTokens: number }; swarmMode?: boolean };
    expect(mergedEvent.usage.inputTokens).toBe(2);
    expect(mergedEvent.swarmMode).toBe(true);
  });

  it('should let a later explicit value override the previous frame optional field', () => {
    const first: AppEvent = { type: 'sessionUsageUpdated', sessionId: 's1', usage: {} as never, planMode: true };
    const second: AppEvent = { type: 'sessionUsageUpdated', sessionId: 's1', usage: {} as never, planMode: false };
    const merged = coalesceAppRenderEvents({ appEvent: first, meta }, { appEvent: second, meta });
    expect((merged!.appEvent as { planMode?: boolean }).planMode).toBe(false);
  });

  it('should keep only the latest work-changed frame when consecutive frames coalesce', () => {
    const on: AppEvent = { type: 'sessionWorkChanged', sessionId: 's1', busy: true, mainTurnActive: true, pendingInteraction: 'none', lastTurnReason: undefined };
    const off: AppEvent = { type: 'sessionWorkChanged', sessionId: 's1', busy: false, mainTurnActive: false, pendingInteraction: 'none', lastTurnReason: 'completed' };
    const merged = coalesceAppRenderEvents({ appEvent: on, meta }, { appEvent: off, meta });
    expect((merged!.appEvent as { mainTurnActive: boolean }).mainTurnActive).toBe(false);
  });

  it('should not coalesce across sessions', () => {
    const a: AppEvent = { type: 'sessionUsageUpdated', sessionId: 's1', usage: { inputTokens: 1 } as never };
    const b: AppEvent = { type: 'sessionUsageUpdated', sessionId: 's2', usage: { inputTokens: 2 } as never };
    expect(coalesceAppRenderEvents({ appEvent: a, meta }, { appEvent: b, meta })).toBeUndefined();
  });
});

describe('background usage frame drop rule (SDR-003a: no commit without visual change)', () => {
  it('should drop a usage frame for a non-active session with no visible usage surface', () => {
    const frame: AppEvent = { type: 'sessionUsageUpdated', sessionId: 's-other', usage: { inputTokens: 5 } as never };
    expect(shouldDropUsageFrame(frame, 's-active')).toBe(true);
  });

  it('should keep a usage frame for the active session', () => {
    const frame: AppEvent = { type: 'sessionUsageUpdated', sessionId: 's-active', usage: { inputTokens: 5 } as never };
    expect(shouldDropUsageFrame(frame, 's-active')).toBe(false);
  });
});
