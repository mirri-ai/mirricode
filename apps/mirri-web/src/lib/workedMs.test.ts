// apps/mirri-web/src/lib/workedMs.test.ts
// Unit tests for the run-header elapsed resolution (resolveWorkedMs) — the
// label contract behind "Working for {duration}" (streaming) and
// "Worked for {duration}" (completed).

import { describe, expect, it } from 'vitest';

import { resolveWorkedMs } from './workedMs';

describe('resolveWorkedMs (run-header elapsed resolution)', () => {
  it('should prefer the server durationMs over a running-turn anchor', () => {
    // Given a running turn that ALSO carries a server-measured duration
    // When the elapsed resolves
    const elapsed = resolveWorkedMs({
      durationMs: 1234,
      startedAtMs: 1000,
      nowMs: 5000,
      running: true,
    });

    // Then the authoritative duration wins — never the client estimate
    expect(elapsed).toBe(1234);
  });

  it('should compute now minus the anchor while a turn is running', () => {
    // Given a streaming turn anchored at t=1000 and a wall clock at t=5000
    // When the elapsed resolves
    const elapsed = resolveWorkedMs({ startedAtMs: 1000, nowMs: 5000, running: true });

    // Then the live elapsed is the anchored span
    expect(elapsed).toBe(4000);
  });

  it('should floor the live estimate at zero when the clock lags the anchor', () => {
    // Given a wall clock that has not caught up to the turn's start anchor
    // (e.g. a server stamp from a slightly-ahead clock)
    // When the elapsed resolves
    const elapsed = resolveWorkedMs({ startedAtMs: 5000, nowMs: 3000, running: true });

    // Then the header never shows a negative duration
    expect(elapsed).toBe(0);
  });

  it('should return null while running when no anchor exists', () => {
    // Given a streaming turn with no start anchor (older engine, side chat)
    // When the elapsed resolves
    const elapsed = resolveWorkedMs({ nowMs: 5000, running: true });

    // Then the header falls back to the bare "Working…" label
    expect(elapsed).toBeNull();
  });

  it('should return null when not running and no duration exists', () => {
    // Given a completed turn whose run-end turn carries no durationMs
    // When the elapsed resolves
    const elapsed = resolveWorkedMs({ nowMs: 5000, running: false });

    // Then the header falls back to the bare "Worked" label
    expect(elapsed).toBeNull();
  });

  it('should return the duration even when the turn is no longer running', () => {
    // Given a completed turn with a server-measured duration
    // When the elapsed resolves after the turn ended
    const elapsed = resolveWorkedMs({ durationMs: 9876, running: false });

    // Then the completed-turn path renders the authoritative "Worked for X"
    expect(elapsed).toBe(9876);
  });
});