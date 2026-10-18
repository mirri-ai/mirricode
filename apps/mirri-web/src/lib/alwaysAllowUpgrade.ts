// apps/mirri-web/src/lib/alwaysAllowUpgrade.ts
// Pure TS — no Vue. The "always allow" one-click upgrade composition (B9 /
// P-012): answering the approval comes FIRST (the tool call must never be
// gated on the rule write), then the engine's approval-rule facet is
// persisted as a user-level `allow` rule. The rule write only happens for
// an APPROVED decision — a rejection with a stray `alwaysAllowRule` must
// never persist an allow rule. A rule-write failure is surfaced through
// `onRuleWriteFailure` without undoing the approval — the decision already
// took effect.

import type { ApprovalResponse } from '../api/types';

/**
 * Given a user decision on an approval, when the decision is approved and
 * carries `alwaysAllowRule`, then answer the approval first and persist the
 * facet as a user-level allow rule afterwards — surfacing (not throwing) a
 * rule-write failure so the approval itself is kept. Any other decision is
 * answered as-is with no rule write.
 */
export async function respondApprovalWithUpgrade(
  api: {
    respondApproval(
      sessionId: string,
      approvalId: string,
      response: ApprovalResponse,
    ): Promise<unknown>;
    addPermissionRule(rule: { decision: 'allow'; pattern: string }): Promise<unknown>;
  },
  sessionId: string,
  approvalId: string,
  response: ApprovalResponse,
  onRuleWriteFailure?: (error: unknown) => void,
): Promise<void> {
  const { alwaysAllowRule, ...approvalResponse } = response;
  // The approval goes first, without the web-internal upgrade field.
  await api.respondApproval(sessionId, approvalId, approvalResponse);
  // P-012 semantics: "always allow" = approve this ask + persist the rule.
  // Only an approved decision upgrades — a rejection never writes a rule.
  if (alwaysAllowRule === undefined || response.decision !== 'approved') return;
  try {
    await api.addPermissionRule({ decision: 'allow', pattern: alwaysAllowRule });
  } catch (error) {
    // The approval HAS taken effect — report the rule write separately.
    if (onRuleWriteFailure !== undefined) onRuleWriteFailure(error);
  }
}
