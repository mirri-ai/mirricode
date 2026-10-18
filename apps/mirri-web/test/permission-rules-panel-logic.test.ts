// apps/mirri-web/test/permission-rules-panel-logic.test.ts
// Given the Settings permission-rules panel (B9 / D-34 / P-013), its pure
// view logic must group the listed rules by decision, pre-check drafts with
// the local facet mirror before submitting (the engine stays the
// authoritative validator), and map decisions onto badge variants.

import { describe, expect, it } from 'vitest';

import {
  decisionBadgeVariant,
  groupRulesByDecision,
  validateDraft,
} from '../src/lib/permissionRules';

describe('groupRulesByDecision', () => {
  it('should group rows by decision when rules are listed', () => {
    const groups = groupRulesByDecision([
      { decision: 'deny', scope: 'user', pattern: 'Bash' },
      { decision: 'allow', scope: 'user', pattern: 'Read' },
      { decision: 'deny', scope: 'user', pattern: 'Write(/tmp/**)' },
      { decision: 'ask', scope: 'user', pattern: 'Fetch' },
    ]);
    expect(groups.deny.map((r) => r.pattern)).toEqual(['Bash', 'Write(/tmp/**)']);
    expect(groups.allow.map((r) => r.pattern)).toEqual(['Read']);
    expect(groups.ask.map((r) => r.pattern)).toEqual(['Fetch']);
  });

  it('should answer empty groups when no rule matches a decision', () => {
    const groups = groupRulesByDecision([{ decision: 'allow', scope: 'user', pattern: 'Read' }]);
    expect(groups.deny).toEqual([]);
    expect(groups.ask).toEqual([]);
  });
});

describe('validateDraft', () => {
  it('should block submit when the pattern is empty or blank', () => {
    expect(validateDraft({ decision: 'allow', pattern: '' })).toBe(false);
    expect(validateDraft({ decision: 'allow', pattern: '   ' })).toBe(false);
  });

  it('should block submit when the local facet pre-check fails', () => {
    expect(validateDraft({ decision: 'allow', pattern: 'Write(/unclosed' })).toBe(false);
    expect(validateDraft({ decision: 'allow', pattern: 'Write(/ok/**)' })).toBe(true);
    expect(validateDraft({ decision: 'deny', pattern: 'Bash' })).toBe(true);
  });
});

describe('decisionBadgeVariant', () => {
  it('should map decisions onto badge variants', () => {
    expect(decisionBadgeVariant('allow')).toBe('success');
    expect(decisionBadgeVariant('deny')).toBe('danger');
    expect(decisionBadgeVariant('ask')).toBe('warning');
  });
});
