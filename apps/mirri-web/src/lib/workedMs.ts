// apps/mirri-web/src/lib/workedMs.ts
// Pure elapsed-duration resolution + the live-clock composable behind the
// streaming run header's "Working for X" label (Vue port of the lynx app's
// workedMs.ts — same resolution contract, component-agnostic).
//
// The server's authoritative `durationMs` (wall-clock from turn start to
// `turn.ended`) ALWAYS wins when present: the run-end turn carries it, so a
// completed turn's header shows the same "Worked for X" the server measured.
// While streaming there is no duration yet — the header falls back to a live
// client estimate anchored on the turn's real start time (`startedAtMs`):
// epoch-ms from the server snapshot (`current_turn_started_at`) for restored
// sessions, or the client's `turn.started` wall-clock for live-observed
// turns.

import { computed, onScopeDispose, ref, watch, type ComputedRef } from 'vue';

export interface WorkedMsInput {
  /** Server-measured duration of a completed turn (ms) — always wins. */
  durationMs?: number;
  /** Epoch-ms anchor of the running turn's real start (server snapshot or
   *  client turn.started stamp); absent → null while running. */
  startedAtMs?: number;
  /** Wall-clock "now" used to compute the live estimate — injectable for
   *  deterministic tests. */
  nowMs?: number;
  running: boolean;
}

/**
 * Resolve the elapsed duration shown in the run header.
 *
 * - `durationMs` present → it verbatim (the completed-turn path).
 * - running with an `startedAtMs` anchor → `now − anchor` (floored at 0).
 * - running without an anchor, or not running without a duration → null
 *   (the caller falls back to the bare "Working…" / "Worked" label — an
 *   unanchorable stream must never show a fabricated elapsed).
 */
export function resolveWorkedMs(input: WorkedMsInput): number | null {
  if (input.durationMs !== undefined) return input.durationMs;
  if (!input.running || input.startedAtMs === undefined) return null;
  return Math.max(0, (input.nowMs ?? Date.now()) - input.startedAtMs);
}

/**
 * Re-render-bound live clock for a streaming turn: returns the elapsed
 * milliseconds since `startedAtMs`, ticking every second while `running`.
 * The timer exists ONLY while streaming — a completed turn (which carries
 * the server's `durationMs`) never re-renders on a timer. The anchor is
 * read reactively, so a late snapshot / live turn.started stamp swaps in
 * without restarting the clock.
 */
export function useLiveElapsedMs(
  startedAtMs: ComputedRef<number | undefined>,
  running: ComputedRef<boolean>,
): ComputedRef<number | null> {
  const nowMs = ref(Date.now());
  let timer: ReturnType<typeof setInterval> | null = null;
  const stopTimer = (): void => {
    if (timer !== null) {
      clearInterval(timer);
      timer = null;
    }
  };

  watch(
    running,
    (isRunning) => {
      // Jump straight to the current wall clock on every activation so a
      // re-stream never renders the previous tick's stale second.
      nowMs.value = Date.now();
      stopTimer();
      if (isRunning) {
        timer = setInterval(() => {
          nowMs.value = Date.now();
        }, 1_000);
      }
    },
    { immediate: true },
  );
  onScopeDispose(stopTimer);

  return computed(() =>
    resolveWorkedMs({
      startedAtMs: startedAtMs.value,
      nowMs: nowMs.value,
      running: running.value,
    }),
  );
}