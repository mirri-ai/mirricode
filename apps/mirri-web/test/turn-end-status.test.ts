import { describe, expect, it } from 'vitest';

import { turnEndStatusFor } from '../src/composables/useMirriWebClient';

// ADR-004 (turn-scheduling O8-d): the turn-end reason folds into the
// session's quiet status. Only a real completion is idle; every other
// reason — including unknown values from newer engines — must land in the
// non-completed family so a termination is never swallowed.
describe('turnEndStatusFor', () => {
  it('returns idle when the turn completed normally', () => {
    expect(turnEndStatusFor('completed')).toBe('idle');
  });

  it('returns aborted for the known non-completed outcomes', () => {
    for (const reason of ['cancelled', 'failed', 'filtered', 'stalled']) {
      expect(turnEndStatusFor(reason)).toBe('aborted');
    }
  });

  it('returns aborted for unknown reasons from newer engines', () => {
    expect(turnEndStatusFor('some-future-reason')).toBe('aborted');
  });
});
