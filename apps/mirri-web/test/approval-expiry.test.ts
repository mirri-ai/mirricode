// apps/mirri-web/test/approval-expiry.test.ts
// Given the engine derives NO approval expiry (D-13: the rust core never
// sends `expires_at`), the web must stop fabricating one — the ask bridge
// omits `expiresAt` when the wire carries no expiry and passes a real one
// through untouched, and the countdown helper answers null (render nothing)
// for absent/empty/unparsable values, clamping the past at zero.

import { describe, expect, it } from 'vitest';

import { approvalRequestFromAskParams } from '../src/api/daemon/ws-rpc-event-bridge';
import { toAppApprovalRequest } from '../src/api/daemon/mappers';
import { toAppSnapshot } from '../src/api/daemon/ws-rpc-api';
import { expirySecondsLeft, formatExpiryCountdown } from '../src/lib/approvalExpiry';

describe('approval expiry honesty (D-13)', () => {
  it('should omit expiresAt when the engine ask carries no expiry', () => {
    const approval = approvalRequestFromAskParams({
      tool_name: 'Bash',
      tool_call_id: 'c1',
      action: 'execute Bash',
      display: {},
      reason: 'approval required',
      request_id: 'req-1',
    }, 's1');
    expect(approval).not.toBeNull();
    expect(approval!.expiresAt).toBeUndefined();
  });

  it('should pass through a real expiry when the engine ask carries one', () => {
    const approval = approvalRequestFromAskParams({
      tool_name: 'Bash',
      tool_call_id: 'c1',
      action: 'execute Bash',
      display: {},
      reason: 'approval required',
      request_id: 'req-2',
      expires_at: '2027-01-01T00:00:00.000Z',
    }, 's1');
    expect(approval).not.toBeNull();
    expect(approval!.expiresAt).toBe('2027-01-01T00:00:00.000Z');
  });

  it('should omit expiresAt on the event mapper when the wire carries no expiry', () => {
    // The `event.approval.requested` path (mappers.toAppApprovalRequest):
    // absent `expires_at` must stay absent — no empty-string default.
    const approval = toAppApprovalRequest({
      approval_id: 'req-3',
      session_id: 's1',
      tool_call_id: 'c3',
      tool_name: 'Bash',
      action: 'execute Bash',
      created_at: '2026-01-01T00:00:00Z',
    } as never);
    expect(approval.expiresAt).toBeUndefined();
  });

  it('should omit expiresAt on snapshot rows when the gate snapshot carries none', () => {
    // The `session.snapshot` hydration path (ws-rpc-api.toAppSnapshot):
    // the rust gate snapshot has no `expires_at` — the field must stay
    // absent instead of a fabricated empty string.
    const snapshot = toAppSnapshot({
      as_of_seq: 1,
      epoch: 'e',
      session: {} as never,
      messages: { items: [], has_more: false },
      pending_approvals: [{
        approval_id: 'req-4',
        session_id: 's1',
        tool_call_id: 'c4',
        tool_name: 'Bash',
        action: 'execute Bash',
        created_at: '2026-01-01T00:00:00Z',
      }],
    } as never, 's1');
    expect(snapshot.pendingApprovals[0].expiresAt).toBeUndefined();
  });
});

describe('snapshot approval facet (B9 R4)', () => {
  it('should carry approvalRule onto hydrated snapshot rows when the wire sends one', () => {
    const snapshot = toAppSnapshot({
      as_of_seq: 1,
      epoch: 'e',
      session: {} as never,
      messages: { items: [], has_more: false },
      pending_approvals: [{
        approval_id: 'req-5',
        session_id: 's1',
        tool_call_id: 'c5',
        tool_name: 'Bash',
        action: 'execute Bash',
        approvalRule: 'Bash',
        created_at: '2026-01-01T00:00:00Z',
      }],
    } as never, 's1');
    expect(snapshot.pendingApprovals[0].approvalRule).toBe('Bash');
  });

  it('should also read the snake_case approval_rule spelling on snapshot rows', () => {
    // Dual-spelling read: the row's other fields are snake_case, so a
    // future engine facet may arrive as `approval_rule` — both spellings
    // must hydrate (the same tolerance toAppApprovalRequest applies).
    const snapshot = toAppSnapshot({
      as_of_seq: 1,
      epoch: 'e',
      session: {} as never,
      messages: { items: [], has_more: false },
      pending_approvals: [{
        approval_id: 'req-7',
        session_id: 's1',
        tool_call_id: 'c7',
        tool_name: 'Bash',
        action: 'execute Bash',
        approval_rule: 'Bash',
        created_at: '2026-01-01T00:00:00Z',
      }],
    } as never, 's1');
    expect(snapshot.pendingApprovals[0].approvalRule).toBe('Bash');
  });

  it('should leave approvalRule absent on hydrated rows when the wire sends none', () => {
    const snapshot = toAppSnapshot({
      as_of_seq: 1,
      epoch: 'e',
      session: {} as never,
      messages: { items: [], has_more: false },
      pending_approvals: [{
        approval_id: 'req-6',
        session_id: 's1',
        tool_call_id: 'c6',
        tool_name: 'Bash',
        action: 'execute Bash',
        created_at: '2026-01-01T00:00:00Z',
      }],
    } as never, 's1');
    expect(snapshot.pendingApprovals[0].approvalRule).toBeUndefined();
  });
});

describe('expirySecondsLeft', () => {
  it('should answer null when expiresAt is absent, empty, or unparsable', () => {
    expect(expirySecondsLeft(undefined, 0)).toBeNull();
    expect(expirySecondsLeft(null, 0)).toBeNull();
    expect(expirySecondsLeft('', 0)).toBeNull();
    expect(expirySecondsLeft('not-a-date', 0)).toBeNull();
  });

  it('should answer remaining seconds when expiresAt is a real future instant', () => {
    const t = Date.parse('2027-01-01T00:00:00.000Z');
    expect(expirySecondsLeft('2027-01-01T00:00:00.000Z', t - 90_000)).toBe(90);
  });

  it('should clamp at zero when the instant is in the past', () => {
    const t = Date.parse('2027-01-01T00:00:00.000Z');
    expect(expirySecondsLeft('2027-01-01T00:00:00.000Z', t + 5_000)).toBe(0);
  });

  it('should round a partial second up so the chip never shows 0 early', () => {
    const t = Date.parse('2027-01-01T00:00:00.000Z');
    expect(expirySecondsLeft('2027-01-01T00:00:00.000Z', t - 90_500)).toBe(91);
  });
});

describe('formatExpiryCountdown', () => {
  it('should format whole minutes and sub-minute remainders as mm:ss', () => {
    expect(formatExpiryCountdown(90)).toBe('01:30');
    expect(formatExpiryCountdown(5)).toBe('00:05');
    expect(formatExpiryCountdown(3_725)).toBe('62:05');
  });
});
