// apps/mirri-web/test/recovery-notice-card.test.ts
// Source-string assertions for RecoveryNoticeCard.vue (the ws-menu.test.ts
// precedent: no jsdom, so the template contract is pinned against the SFC
// source — per-row actions, the count-carrying resume-all, and C.6's rule
// that the close button binds no RPC at all).

import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';

const cardSource = readFileSync(
  join(import.meta.dirname, '../src/components/RecoveryNoticeCard.vue'),
  'utf8',
);

const template = (() => {
  const start = cardSource.indexOf('<template>');
  // The card uses nested <template #head/#foot> slots, so the block ends at
  // the LAST closing tag, not the first.
  const end = cardSource.lastIndexOf('</template>');
  if (start === -1 || end === -1) throw new Error('RecoveryNoticeCard.vue has no template block');
  return cardSource.slice(start, end);
})();

describe('RecoveryNoticeCard template contract', () => {
  it('should render per-row resume and dismiss buttons wired to the emitted actions when the card body is inspected', () => {
    expect(template).toContain('@click="emit(\'resume\', entry)"');
    expect(template).toContain('@click="emit(\'dismiss\', entry)"');
  });

  it('should make the row title open the session instead of resuming it when the row heading is inspected', () => {
    expect(template).toContain('@click="emit(\'open-session\', entry)"');
  });

  it('should render the footer resume-all with the count interpolation when the footer is inspected', () => {
    expect(template).toContain("t('recovery.resumeAll'");
    expect(template).toContain("{ count: interruptions.length }");
  });

  it('should render footer dismiss-all and header close buttons when the chrome is inspected', () => {
    expect(template).toContain("@click=\"emit('dismiss-all')\"");
    // C.6: closing the card only dismisses the UI — the close button must not
    // trigger any client/RPC action, so the emitted name is 'close'.
    expect(template).toContain("@click=\"emit('close')\"");
  });

  it('should bind no recovery RPC method name anywhere in the template when the whole card is scanned', () => {
    for (const forbidden of [
      'resumeInterrupted',
      'resumeAllInterrupted',
      'dismissInterrupted',
      'dismissAllInterrupted',
      'refreshRecovery',
      'client.',
    ]) {
      expect(template, `template must not reference "${forbidden}" (C.6/ADR-004: actions arrive via emits)`).not.toContain(forbidden);
    }
  });
});
