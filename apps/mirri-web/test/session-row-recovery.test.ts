// apps/mirri-web/test/session-row-recovery.test.ts
// Interrupted-task recovery (Task 6): the session-tree surface — badge guard,
// per-workspace interruption counts, and the source-string wiring of the
// in-place resume/dismiss quick actions (no jsdom, so component templates
// are asserted as source strings, the ws-menu.test.ts precedent).

import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';

import {
  interruptionCountsByWorkspace,
  interruptedBadgeVisible,
} from '../src/lib/sessionRowRecovery';

const sessionRowSource = readFileSync(
  join(import.meta.dirname, '../src/components/SessionRow.vue'),
  'utf8',
);
const workspaceGroupSource = readFileSync(
  join(import.meta.dirname, '../src/components/WorkspaceGroup.vue'),
  'utf8',
);
const appSource = readFileSync(
  join(import.meta.dirname, '../src/App.vue'),
  'utf8',
);

describe('interruptedBadgeVisible (badge guard, ADR-010)', () => {
  it('Given a merged interrupted last-turn reason on an idle row, When the guard runs, Then the badge is visible', () => {
    expect(
      interruptedBadgeVisible({ renaming: false, busy: false, lastTurnReason: 'interrupted' }),
    ).toBe(true);
  });

  it('Given an idle row without the interrupted reason, When the guard runs, Then the badge is hidden', () => {
    for (const lastTurnReason of ['completed', 'cancelled', 'failed', 'stalled', undefined] as const) {
      expect(
        interruptedBadgeVisible({ renaming: false, busy: false, lastTurnReason }),
      ).toBe(false);
    }
  });

  it('Given a row being renamed or still running, When the guard runs, Then the badge is hidden even with the interrupted reason', () => {
    expect(
      interruptedBadgeVisible({ renaming: true, busy: false, lastTurnReason: 'interrupted' }),
    ).toBe(false);
    expect(
      interruptedBadgeVisible({ renaming: false, busy: true, lastTurnReason: 'interrupted' }),
    ).toBe(false);
  });
});

describe('interruptionCountsByWorkspace (grouping)', () => {
  it('Given interruptions across workspaces, When grouped by workspace key, Then each key counts its own rows', () => {
    const counts = interruptionCountsByWorkspace([
      { sessionId: 's1', workspaceKey: 'wd_a_111111111111' },
      { sessionId: 's2', workspaceKey: 'wd_a_111111111111' },
      { sessionId: 's3', workspaceKey: 'wd_b_222222222222' },
    ] as never);
    expect(counts.get('wd_a_111111111111')).toBe(2);
    expect(counts.get('wd_b_222222222222')).toBe(1);
  });

  it('Given an empty interruption list, When grouped by workspace key, Then no workspace has a count', () => {
    expect(interruptionCountsByWorkspace([]).size).toBe(0);
  });
});

describe('session-tree source wiring', () => {
  it('Given the SessionRow template, When inspected, Then an interrupted badge branch renders the workspace.interrupted label from the extracted guard', () => {
    expect(sessionRowSource).toContain('interruptedBadgeVisible');
    expect(sessionRowSource).toContain("t('workspace.interrupted')");
  });

  it('Given the SessionRow template, When inspected, Then hover quick actions for resume and dismiss are present for interrupted rows', () => {
    expect(sessionRowSource).toContain("t('recovery.resumeOne')");
    expect(sessionRowSource).toContain("t('recovery.dismissOne')");
  });

  it('Given the WorkspaceGroup template, When inspected, Then the interrupted count renders next to the workspace name', () => {
    expect(workspaceGroupSource).toContain('recoveryCount');
    expect(workspaceGroupSource).toContain('gh-recovery-count');
  });

  it('Given App.vue, When inspected, Then the resume/dismiss helpers are provided for the session rows', () => {
    expect(appSource).toContain('provide(\'resumeInterruptedSession\'');
    expect(appSource).toContain('provide(\'dismissInterruptedSession\'');
  });

  it('Given App.vue, When inspected, Then both resume channels (session row and notice card) route through the shared background-hint gate', () => {
    // E.3/D.2: the notice card's @resume must not bypass the confirm gate
    // by calling client.resumeInterrupted directly — both channels share
    // the same single-item resume semantics.
    expect(appSource).toContain('resumeWithHintGate');
    expect(appSource).toContain('@resume="(entry) => void resumeWithHintGate(entry.sessionId)"');
    expect(appSource).not.toContain('@resume="(entry) => client.resumeInterrupted');
  });
});
