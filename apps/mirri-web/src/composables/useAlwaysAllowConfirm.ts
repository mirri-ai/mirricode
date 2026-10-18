// apps/mirri-web/src/composables/useAlwaysAllowConfirm.ts
// The approval card's "always allow" inline confirm-strip state machine
// (B9 / P-012): the button opens a confirmation strip that honestly shows
// the exact pattern to be persisted; the strip opens only when the card is
// not mid-decision (the same guard the action buttons apply).

import { ref } from 'vue';

export function useAlwaysAllowConfirm(opts: { isBusy: () => boolean }) {
  /** Whether the inline confirm strip renders. */
  const confirmOpen = ref(false);

  /** Open the strip — a no-op while a decision is in flight. */
  function open(): void {
    if (opts.isBusy()) return;
    confirmOpen.value = true;
  }

  /** Close the strip (cancel — nothing is decided, nothing is written). */
  function close(): void {
    confirmOpen.value = false;
  }

  return { confirmOpen, open, close };
}
