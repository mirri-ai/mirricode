import { describe, expect, it } from 'vitest';
import { effect, reactive } from 'vue';

import { writeIfChanged, writeMapIfChanged } from './client/writeBack';
import type { MirriClientState } from '../api/daemon/eventReducer';

describe('writeMapIfChanged (SDR-003b nested-key write-back)', () => {
  it('should not touch the map object when references are identical', () => {
    const target: Record<string, number[]> = { a: [1] };
    const same = target;
    expect(writeMapIfChanged(target, same)).toBe(false);
  });

  it('should write only the changed session key, keeping other entries untouched by reference', () => {
    const a1 = [1];
    const b1 = [2];
    const target: Record<string, number[]> = { a: a1, b: b1 };
    // Reducer cloned the map and replaced only 'a' with a new array.
    const next: Record<string, number[]> = { a: [1, 2], b: b1 };
    const changed = writeMapIfChanged(target, next);
    expect(changed).toBe(true);
    expect(target.a).toEqual([1, 2]);
    expect(target.b).toBe(b1); // same reference — b consumers never invalidate
  });

  it('should delete keys absent from next', () => {
    const target: Record<string, number[]> = { a: [1], b: [2] };
    const next: Record<string, number[]> = { a: [1] };
    writeMapIfChanged(target, next);
    expect('b' in target).toBe(false);
  });
});

describe('writeIfChanged (reference-compare field write-back)', () => {
  it('should skip assignment when the reference is unchanged', () => {
    const arr = [1];
    const state = { sessions: arr } as never as MirriClientState;
    const next = { sessions: arr } as never as MirriClientState;
    writeIfChanged(state, next, 'sessions');
    expect(state.sessions).toBe(arr);
  });

  it('should assign when the reference differs', () => {
    const arr1 = [1];
    const arr2 = [2];
    const state = { sessions: arr1 } as never as MirriClientState;
    const next = { sessions: arr2 } as never as MirriClientState;
    writeIfChanged(state, next, 'sessions');
    expect(state.sessions).toBe(arr2);
  });
});

describe('writeMapIfChanged on a reactive map (SDR-003b isolation acceptance)', () => {
  it('should not re-run effects reading other sessions when only one session entry is written', () => {
    const aArr = [1];
    const bArr = [2];
    const proxy = reactive<Record<string, number[]>>({ a: aArr, b: bArr });

    let bReads = 0;
    effect(() => {
      void proxy.b; // a consumer of session b only (e.g. b's turns computed)
      bReads += 1;
    });
    expect(bReads).toBe(1);

    // The write-back: reducer produced a map where only 'a' changed.
    const next: Record<string, number[]> = { a: [1, 2], b: bArr };
    const changed = writeMapIfChanged(proxy, next);

    expect(changed).toBe(true);
    expect(proxy.a).toEqual([1, 2]);
    expect(bReads).toBe(1); // b's effect did NOT re-run — the isolation win
  });

  it('should re-run the session-a effect when its entry changes', () => {
    const proxy = reactive<Record<string, number[]>>({ a: [1] });
    let aReads = 0;
    effect(() => {
      void proxy.a;
      aReads += 1;
    });
    expect(aReads).toBe(1);
    writeMapIfChanged(proxy, { a: [1, 2] });
    expect(aReads).toBe(2); // the target session's consumers DO update
  });

  it('should not run any effect when the reducer kept the same reference (no-op event)', () => {
    const proxy = reactive<Record<string, number[]>>({ a: [1] });
    let aReads = 0;
    effect(() => {
      void proxy.a;
      aReads += 1;
    });
    writeMapIfChanged(proxy, proxy); // same reference — no writes at all
    expect(aReads).toBe(1);
  });
});
