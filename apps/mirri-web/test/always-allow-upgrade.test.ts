// apps/mirri-web/test/always-allow-upgrade.test.ts
// Given the "always allow" one-click upgrade (B9 / P-012) — approve this ask
// AND persist the engine's approval-rule facet as a user-level allow rule —
// the composition must answer the approval first (the tool call must not be
// gated on the rule write), strip the web-internal `alwaysAllowRule` field
// before it reaches the approval wire, and surface a rule-write failure
// without undoing the approval. The card-side confirm strip must open only
// when the card is not busy.

import { describe, expect, it, vi } from 'vitest';

import { respondApprovalWithUpgrade } from '../src/lib/alwaysAllowUpgrade';
import { useAlwaysAllowConfirm } from '../src/composables/useAlwaysAllowConfirm';
import type { ApprovalResponse } from '../src/api/types';

// ---------------------------------------------------------------------------
// The upgrade composition (lib/alwaysAllowUpgrade.ts)
// ---------------------------------------------------------------------------

describe('always-allow upgrade composition (P-012)', () => {
  it('should approve then persist a user rule when the response carries alwaysAllowRule', async () => {
    const api = {
      respondApproval: vi.fn().mockResolvedValue({ resolved: true, resolvedAt: '2026-01-01T00:00:00Z' }),
      addPermissionRule: vi.fn().mockResolvedValue([]),
    };
    await respondApprovalWith(api, 's1', 'req-1', {
      decision: 'approved', alwaysAllowRule: 'Bash',
    });
    // The approval goes first, WITHOUT the web-internal upgrade field.
    expect(api.respondApproval).toHaveBeenCalledWith('s1', 'req-1', { decision: 'approved' });
    expect(api.addPermissionRule).toHaveBeenCalledWith({ decision: 'allow', pattern: 'Bash' });
  });

  it('should approve before persisting the rule so the tool call is never gated on the write', async () => {
    const order: string[] = [];
    const api = {
      respondApproval: vi.fn().mockImplementation(async () => {
        order.push('respondApproval');
        return { resolved: true, resolvedAt: '2026-01-01T00:00:00Z' };
      }),
      addPermissionRule: vi.fn().mockImplementation(async () => {
        order.push('addPermissionRule');
        return [];
      }),
    };
    await respondApprovalWith(api, 's1', 'req-1', {
      decision: 'approved', alwaysAllowRule: 'Bash',
    });
    expect(order).toEqual(['respondApproval', 'addPermissionRule']);
  });

  it('should surface a failure notice but keep the approval when the rule write fails', async () => {
    const api = {
      respondApproval: vi.fn().mockResolvedValue({ resolved: true, resolvedAt: '2026-01-01T00:00:00Z' }),
      addPermissionRule: vi.fn().mockRejectedValue(new Error('conflict')),
    };
    const failures: unknown[] = [];
    await respondApprovalWith(api, 's1', 'req-1', {
      decision: 'approved', alwaysAllowRule: 'Bash',
    }, (e) => failures.push(e));
    expect(failures).toHaveLength(1);
  });

  it('should not write a rule when the response has no alwaysAllowRule', async () => {
    const api = {
      respondApproval: vi.fn().mockResolvedValue({ resolved: true, resolvedAt: '2026-01-01T00:00:00Z' }),
      addPermissionRule: vi.fn(),
    };
    await respondApprovalWith(api, 's1', 'req-1', { decision: 'approved', scope: 'session' });
    expect(api.addPermissionRule).not.toHaveBeenCalled();
  });

  it('should strip the upgrade field while keeping the rest of the response intact', async () => {
    const api = {
      respondApproval: vi.fn().mockResolvedValue({ resolved: true, resolvedAt: '2026-01-01T00:00:00Z' }),
      addPermissionRule: vi.fn().mockResolvedValue([]),
    };
    await respondApprovalWith(api, 's1', 'req-1', {
      decision: 'rejected', feedback: 'no', alwaysAllowRule: 'Bash',
    });
    expect(api.respondApproval).toHaveBeenCalledWith('s1', 'req-1', { decision: 'rejected', feedback: 'no' });
  });

  it('should never write an allow rule when the decision is not approved', async () => {
    const api = {
      respondApproval: vi.fn().mockResolvedValue({ resolved: true, resolvedAt: '2026-01-01T00:00:00Z' }),
      addPermissionRule: vi.fn(),
    };
    // A stray upgrade directive on a rejection must not persist a rule —
    // "always allow" is an approval affordance only (P-012).
    await respondApprovalWith(api, 's1', 'req-1', {
      decision: 'rejected', alwaysAllowRule: 'Bash',
    });
    await respondApprovalWith(api, 's1', 'req-2', {
      decision: 'cancelled', alwaysAllowRule: 'Bash',
    });
    expect(api.addPermissionRule).not.toHaveBeenCalled();
  });
});

/** The minimal API surface the composition needs — matches MirriWebApi's
 *  approval + rule-management methods. */
function respondApprovalWith(
  api: {
    respondApproval: (sessionId: string, approvalId: string, response: ApprovalResponse) => Promise<unknown>;
    addPermissionRule: (rule: { decision: 'allow'; pattern: string }) => Promise<unknown>;
  },
  sessionId: string,
  approvalId: string,
  response: { decision: ApprovalResponse['decision']; scope?: 'session'; feedback?: string; alwaysAllowRule?: string },
  onRuleWriteFailure?: (error: unknown) => void,
): Promise<void> {
  return respondApprovalWithUpgrade(api, sessionId, approvalId, response, onRuleWriteFailure);
}

// ---------------------------------------------------------------------------
// The card confirm strip state machine (composables/useAlwaysAllowConfirm.ts)
// ---------------------------------------------------------------------------

describe('useAlwaysAllowConfirm', () => {
  it('should show the confirm strip when always-allow is clicked and hide it on cancel', () => {
    const { confirmOpen, open, close } = useAlwaysAllowConfirm({ isBusy: () => false });
    expect(confirmOpen.value).toBe(false);
    open();
    expect(confirmOpen.value).toBe(true);
    close();
    expect(confirmOpen.value).toBe(false);
  });

  it('should not open the confirm strip while a decision is in flight', () => {
    const { confirmOpen, open } = useAlwaysAllowConfirm({ isBusy: () => true });
    open();
    expect(confirmOpen.value).toBe(false);
  });
});
