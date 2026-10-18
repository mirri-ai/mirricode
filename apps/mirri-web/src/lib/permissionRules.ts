// apps/mirri-web/src/lib/permissionRules.ts
// Pure TS — no Vue. Permission-rules panel view logic (B9 / D-34 / P-013):
// grouping, the add-form draft pre-check (the local facet mirror — the
// engine stays the authoritative validator), and the decision → badge
// variant mapping.

import type { AppPermissionRule, AppPermissionRuleInput } from '../api/types';
import type { RuleFacetView } from './ruleFacet';
import { parseRuleFacet } from './ruleFacet';

/** Every rule list grouped by decision, keyed in the panel's fixed order. */
export interface RulesByDecision {
  allow: AppPermissionRule[];
  deny: AppPermissionRule[];
  ask: AppPermissionRule[];
}

/** Group rules by decision (order within each group is the engine's list
 *  order — the rule chain's evaluation order). */
export function groupRulesByDecision(rules: ReadonlyArray<AppPermissionRule>): RulesByDecision {
  const groups: RulesByDecision = { allow: [], deny: [], ask: [] };
  for (const rule of rules) {
    if (rule.decision === 'allow') groups.allow.push(rule);
    else if (rule.decision === 'deny') groups.deny.push(rule);
    else groups.ask.push(rule);
  }
  return groups;
}

/** Whether an add-form draft is submittable: a non-blank pattern whose
 *  local facet pre-check parses. The engine re-validates authoritatively —
 *  a `false` here just spares the round trip. */
export function validateDraft(draft: AppPermissionRuleInput): boolean {
  if (draft.pattern.trim() === '') return false;
  return parseRuleFacet(draft.pattern) !== null;
}

/** The row/detail badge variant for a decision. */
export function decisionBadgeVariant(
  decision: AppPermissionRule['decision'],
): 'success' | 'danger' | 'warning' {
  if (decision === 'allow') return 'success';
  if (decision === 'deny') return 'danger';
  return 'warning';
}

/** The facet preview of one rule, for the detail pane's decomposition
 *  (tool / arg pattern / negation). Returns the raw pattern as the tool
 *  when the mirror cannot parse it (display-only — the engine accepted it). */
export function ruleFacetPreview(rule: AppPermissionRule): RuleFacetView {
  return parseRuleFacet(rule.pattern) ?? { tool: rule.pattern, negated: false };
}
