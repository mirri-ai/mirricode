// apps/mirri-web/test/select-session-touch.test.ts
// SDR-019: opening a session is NOT "use". Selecting a session from the
// sidebar (or anywhere else) must never bump the recency key `lastUsedAt` —
// only a user send does (the engine's message-submit handler). The row's
// relative-time label and sort position therefore stay put on a pure open.

import { describe, expect, it, vi } from 'vitest';
import { useMirriWebClient } from '../src/composables/useMirriWebClient';

// Every API method the open path may touch resolves to null; real engines
// answer the snapshot/prompt calls. The `touchSession` spy is the one this
// suite exists to watch — it must stay silent on a plain open.
const apiMock = vi.hoisted(() => {
  const handlers: Record<string, ReturnType<typeof vi.fn>> = {
    // selectSession re-populates the parked-prompt queue from GET /prompts.
    listPrompts: vi.fn().mockResolvedValue({ queued: [] }),
    // The open path refreshes sidecars (status/usage/goal). Missing endpoints
    // reject and the refreshers return early — that mirrors an old daemon.
    getSessionStatus: vi.fn().mockRejectedValue(new Error('no status endpoint')),
    getGoal: vi.fn().mockResolvedValue(null),
  };
  return new Proxy(handlers, {
    get(target, prop) {
      if (typeof prop !== 'string') return undefined;
      if (!(prop in target)) target[prop] = vi.fn().mockResolvedValue(null);
      return target[prop];
    },
  });
});

vi.mock('../src/api', () => ({
  getMirriWebApi: () => apiMock,
}));

describe('selectSession — opening is not use (SDR-019)', () => {
  it('should not bump the recency key when the user only opens a session', async () => {
    const client = useMirriWebClient();

    await client.selectSession('sess_1');

    expect(apiMock.touchSession).not.toHaveBeenCalled();
  });
});
