/**
 * B0-L18: Verify mirri↔v2 wire-record bidirectional compatibility.
 *
 * Both v1 and v2 are at wire protocol 1.5. These tests verify:
 * 1. v2's wire/ + agent/blob/ read/write wire.jsonl in the same format as v1.
 * 2. A wire.jsonl created with v1's record types can be read/resumed by v2
 *    with no field loss.
 * 3. v2 registers ops for all mirri-specific record types.
 * 4. The 1.4→1.5 migration (wallClockResumedAt backfill on goal records) is
 *    present and correct in v2's wire/migration/.
 *
 * Run: `pnpm --filter @mirri-ai/agent-core-v2 exec vitest run
 *      test/wire/v1-v2-cross-resume.test.ts`.
 */

import { describe, expect, it } from 'vitest';

import { OP_REGISTRY } from '#/wire/op';
import { WIRE_PROTOCOL_VERSION } from '#/wire/migration/migration';
import { migrateV1_4ToV1_5 } from '#/wire/migration/migration';
import {
  opToWireRecord,
  wireRecordToPayload,
  type WireRecord,
} from '#/wire/record';

import { GoalModel, createGoal } from '#/agent/goal/goalOps';
import { PlanModel, planModeEnter } from '#/agent/plan/planOps';
import { SwarmModel, swarmEnter } from '#/agent/swarm/swarmOps';
import { PermissionModeModel, setMode } from '#/agent/permissionMode/permissionModeOps';
import { PermissionRulesModel } from '#/agent/permissionRules/permissionRulesOps';
import '#/session/cron/cronOps';

// ─── 1. Wire format compatibility (v1 writes, v2 reads) ─────────────────────

describe('v1→v2 wire.jsonl format compatibility', () => {
  it('should parse a v1 goal.create record without field loss', () => {
    // v1 writes: { type: 'goal.create', goalId, objective, completionCriterion?, time }
    const v1Record: WireRecord = {
      type: 'goal.create',
      goalId: 'goal-abc',
      objective: 'Ship the feature',
      completionCriterion: 'All tests pass',
      time: 1_700_000_000,
    };

    const descriptor = OP_REGISTRY.get('goal.create');
    expect(descriptor).toBeDefined();

    const payload = wireRecordToPayload(v1Record);
    const parsed = descriptor!.schema.safeParse(payload);
    expect(parsed.success).toBe(true);

    if (parsed.success) {
      expect(parsed.data).toMatchObject({
        goalId: 'goal-abc',
        objective: 'Ship the feature',
        completionCriterion: 'All tests pass',
      });
    }

    // Apply and verify the model state matches v1 semantics
    const state = descriptor!.apply(GoalModel.initial(), parsed.success ? parsed.data : {});
    expect(state).toMatchObject({
      goalId: 'goal-abc',
      objective: 'Ship the feature',
      completionCriterion: 'All tests pass',
      status: 'active',
      turnsUsed: 0,
      tokensUsed: 0,
      wallClockMs: 0,
    });
  });

  it('should parse a v1 goal.update record with all fields', () => {
    // v1 writes: { type: 'goal.update', status?, tokensUsed?, turnsUsed?,
    //             wallClockMs?, budgetLimits?, reason?, actor?, time }
    const v1Record: WireRecord = {
      type: 'goal.update',
      status: 'paused',
      turnsUsed: 3,
      tokensUsed: 1200,
      wallClockMs: 65_000,
      budgetLimits: { turnBudget: 10, tokenBudget: 50000, wallClockBudgetMs: 300_000 },
      reason: 'User paused',
      actor: 'user',
      time: 1_700_000_100,
    };

    const descriptor = OP_REGISTRY.get('goal.update');
    expect(descriptor).toBeDefined();

    const payload = wireRecordToPayload(v1Record);
    const parsed = descriptor!.schema.safeParse(payload);
    expect(parsed.success).toBe(true);

    if (parsed.success) {
      expect(parsed.data).toMatchObject({
        status: 'paused',
        turnsUsed: 3,
        tokensUsed: 1200,
        wallClockMs: 65_000,
        budgetLimits: {
          turnBudget: 10,
          tokenBudget: 50000,
          wallClockBudgetMs: 300_000,
        },
        reason: 'User paused',
        actor: 'user',
      });
    }
  });

  it('should parse a v1 goal.clear record', () => {
    const v1Record: WireRecord = {
      type: 'goal.clear',
      time: 1_700_000_200,
    };

    const descriptor = OP_REGISTRY.get('goal.clear');
    expect(descriptor).toBeDefined();

    const payload = wireRecordToPayload(v1Record);
    const parsed = descriptor!.schema.safeParse(payload);
    expect(parsed.success).toBe(true);
  });

  it('should parse v1 plan_mode.enter/cancel/exit records', () => {
    const enter: WireRecord = {
      type: 'plan_mode.enter',
      id: 'plan-123',
      time: 1_700_000_000,
    };
    const cancel: WireRecord = {
      type: 'plan_mode.cancel',
      id: 'plan-123',
      time: 1_700_000_100,
    };
    const exit: WireRecord = {
      type: 'plan_mode.exit',
      id: 'plan-123',
      time: 1_700_000_200,
    };

    for (const record of [enter, cancel, exit]) {
      const descriptor = OP_REGISTRY.get(record.type);
      expect(descriptor).toBeDefined();

      const payload = wireRecordToPayload(record);
      const parsed = descriptor!.schema.safeParse(payload);
      expect(parsed.success).toBe(true);
    }
  });

  it('should parse v1 swarm_mode.enter/exit records', () => {
    const enter: WireRecord = {
      type: 'swarm_mode.enter',
      trigger: 'tool',
      time: 1_700_000_000,
    };
    const exit: WireRecord = {
      type: 'swarm_mode.exit',
      time: 1_700_000_100,
    };

    const enterDescriptor = OP_REGISTRY.get('swarm_mode.enter');
    expect(enterDescriptor).toBeDefined();
    const enterPayload = wireRecordToPayload(enter);
    const enterParsed = enterDescriptor!.schema.safeParse(enterPayload);
    expect(enterParsed.success).toBe(true);

    const exitDescriptor = OP_REGISTRY.get('swarm_mode.exit');
    expect(exitDescriptor).toBeDefined();
    const exitPayload = wireRecordToPayload(exit);
    const exitParsed = exitDescriptor!.schema.safeParse(exitPayload);
    expect(exitParsed.success).toBe(true);
  });

  it('should parse v1 permission.set_mode record', () => {
    const record: WireRecord = {
      type: 'permission.set_mode',
      mode: 'yolo',
      time: 1_700_000_000,
    };

    const descriptor = OP_REGISTRY.get('permission.set_mode');
    expect(descriptor).toBeDefined();

    const payload = wireRecordToPayload(record);
    const parsed = descriptor!.schema.safeParse(payload);
    expect(parsed.success).toBe(true);

    if (parsed.success) {
      expect(parsed.data).toMatchObject({ mode: 'yolo' });
    }
  });

  it('should parse v1 permission.record_approval_result record', () => {
    const record: WireRecord = {
      type: 'permission.record_approval_result',
      result: { decision: 'approved', scope: 'session' },
      sessionApprovalRule: 'Bash:echo *',
      time: 1_700_000_000,
    };

    const descriptor = OP_REGISTRY.get('permission.record_approval_result');
    expect(descriptor).toBeDefined();

    const payload = wireRecordToPayload(record);
    const parsed = descriptor!.schema.safeParse(payload);
    expect(parsed.success).toBe(true);
  });
});

// ─── 2. Full cross-resume scenario: v1 journal → v2 restore ────────────────

describe('v1→v2 cross-resume full scenario', () => {
  it('should restore a v1 wire.jsonl containing all mirri-specific types without field loss', () => {
    // This is a representative v1 wire.jsonl at protocol 1.5 containing
    // all mirri-specific record types. v2 should be able to parse every
    // record and rebuild model state correctly.
    const v1Journal: WireRecord[] = [
      {
        type: 'metadata',
        protocol_version: '1.5',
        created_at: 1_700_000_000,
      },
      {
        type: 'goal.create',
        goalId: 'goal-1',
        objective: 'Ship the feature',
        completionCriterion: 'All tests pass',
        wallClockResumedAt: 1_700_000_000,
        time: 1_700_000_000,
      },
      {
        type: 'goal.update',
        status: 'paused',
        wallClockMs: 30_000,
        turnsUsed: 2,
        tokensUsed: 800,
        budgetLimits: { turnBudget: 10 },
        reason: 'User paused',
        actor: 'user',
        time: 1_700_000_100,
      },
      {
        type: 'goal.update',
        status: 'active',
        wallClockResumedAt: 1_700_000_200,
        actor: 'user',
        time: 1_700_000_200,
      },
      {
        type: 'permission.set_mode',
        mode: 'auto',
        time: 1_700_000_010,
      },
      {
        type: 'plan_mode.enter',
        id: 'plan-abc',
        time: 1_700_000_050,
      },
      {
        type: 'plan_mode.exit',
        id: 'plan-abc',
        time: 1_700_000_080,
      },
      {
        type: 'swarm_mode.enter',
        trigger: 'tool',
        time: 1_700_000_090,
      },
      {
        type: 'swarm_mode.exit',
        time: 1_700_000_095,
      },
      {
        type: 'goal.update',
        status: 'complete',
        turnsUsed: 5,
        tokensUsed: 2000,
        wallClockMs: 120_000,
        reason: 'All done',
        actor: 'model',
        time: 1_700_000_300,
      },
      {
        type: 'goal.clear',
        time: 1_700_000_400,
      },
    ];

    // Verify every non-metadata record type is known to v2's OP_REGISTRY
    for (let i = 0; i < v1Journal.length; i++) {
      const record = v1Journal[i]!;
      if (record.type === 'metadata') continue;
      const descriptor = OP_REGISTRY.get(record.type);
      expect(descriptor, `v2 OP_REGISTRY missing '${record.type}' at index ${i}`).toBeDefined();

      const payload = wireRecordToPayload(record);
      const parsed = descriptor!.schema.safeParse(payload);
      expect(
        parsed.success,
        `v2 schema parse failed for '${record.type}' at index ${i}: ${parsed.success === false ? parsed.error.message : ''}`,
      ).toBe(true);
    }

    // Now simulate the full restore flow: apply records sequentially
    let goalState = GoalModel.initial();
    let planState = PlanModel.initial();
    let swarmState = SwarmModel.initial();
    let permModeState = PermissionModeModel.initial();
    let permRulesState = PermissionRulesModel.initial();

    for (const record of v1Journal) {
      if (record.type === 'metadata') continue;

      const descriptor = OP_REGISTRY.get(record.type);
      if (descriptor === undefined) continue;

      const payload = wireRecordToPayload(record);
      const parsed = descriptor.schema.safeParse(payload);
      if (!parsed.success) continue;

      const data = parsed.data;

      switch (record.type) {
        case 'goal.create':
          goalState = descriptor.apply(goalState, data);
          break;
        case 'goal.update':
          goalState = descriptor.apply(goalState, data);
          break;
        case 'goal.clear':
          goalState = descriptor.apply(goalState, data);
          break;
        case 'plan_mode.enter':
          planState = descriptor.apply(planState, data);
          break;
        case 'plan_mode.exit':
          planState = descriptor.apply(planState, data);
          break;
        case 'swarm_mode.enter':
          swarmState = descriptor.apply(swarmState, data);
          break;
        case 'swarm_mode.exit':
          swarmState = descriptor.apply(swarmState, data);
          break;
        case 'permission.set_mode':
          permModeState = descriptor.apply(permModeState, data);
          break;
      }
    }

    // After processing all records, the goal was cleared
    expect(goalState).toBeNull();

    // Plan mode is exited
    expect(planState.current.active).toBe(false);

    // Swarm mode is exited
    expect(swarmState).toBeNull();

    // Permission mode was set to 'auto'
    expect(permModeState).toBe('auto');
  });

  it('should preserve goal lifecycle state through create→update→complete round-trip', () => {
    // Verify that the goal model state after processing a v1 journal matches
    // what v1's restoreAgentRecord would have produced.
    const v1Journal: WireRecord[] = [
      {
        type: 'goal.create',
        goalId: 'goal-rt',
        objective: 'Round-trip test',
        completionCriterion: 'Pass',
        wallClockResumedAt: 100,
        time: 100,
      },
      {
        type: 'goal.update',
        status: 'paused',
        wallClockMs: 5_000,
        turnsUsed: 1,
        tokensUsed: 500,
        reason: 'Mid-check',
        actor: 'user',
        time: 200,
      },
      {
        type: 'goal.update',
        status: 'active',
        wallClockResumedAt: 300,
        actor: 'user',
        time: 300,
      },
      {
        type: 'goal.update',
        turnsUsed: 3,
        tokensUsed: 1500,
        wallClockMs: 15_000,
        time: 400,
      },
    ];

    let goalState = GoalModel.initial();

    for (const record of v1Journal) {
      const descriptor = OP_REGISTRY.get(record.type);
      expect(descriptor).toBeDefined();

      const payload = wireRecordToPayload(record);
      const parsed = descriptor!.schema.safeParse(payload);
      expect(parsed.success).toBe(true);

      if (parsed.success) {
        goalState = descriptor!.apply(goalState, parsed.data);
      }
    }

    expect(goalState).toMatchObject({
      goalId: 'goal-rt',
      objective: 'Round-trip test',
      completionCriterion: 'Pass',
      status: 'active',
      turnsUsed: 3,
      tokensUsed: 1500,
      wallClockMs: 15_000,
      wallClockResumedAt: 300,
    });
  });
});

// ─── 3. v2→v1 wire format round-trip (v2 dispatches, v1 format expected) ───

describe('v2→v1 wire.jsonl round-trip format', () => {
  it('should produce { type, ...payload, time } records without nested payload key', () => {
    // When v2 dispatches an op, the resulting WireRecord must be flat:
    // { type: 'goal.create', goalId, objective, time } — NOT
    // { type: 'goal.create', payload: { goalId, objective }, time }.
    // This is the v1 format contract.

    const op = createGoal({
      goalId: 'goal-v2',
      objective: 'v2 dispatch test',
    });

    const record = opToWireRecord(op);

    expect(record.type).toBe('goal.create');
    expect(record['goalId']).toBe('goal-v2');
    expect(record['objective']).toBe('v2 dispatch test');
    expect(typeof record['time']).toBe('number');
    // Must NOT have a nested 'payload' key
    expect('payload' in record).toBe(false);
  });

  it('should produce permission.set_mode with flat mode field', () => {
    const op = setMode({ mode: 'manual' });
    const record = opToWireRecord(op);

    expect(record.type).toBe('permission.set_mode');
    expect(record['mode']).toBe('manual');
    expect('payload' in record).toBe(false);
  });

  it('should produce plan_mode.enter with flat id field', () => {
    const op = planModeEnter({ id: 'plan-v2' });
    const record = opToWireRecord(op);

    expect(record.type).toBe('plan_mode.enter');
    expect(record['id']).toBe('plan-v2');
    expect('payload' in record).toBe(false);
  });

  it('should produce swarm_mode.enter with flat trigger field', () => {
    const op = swarmEnter({ trigger: 'task' });
    const record = opToWireRecord(op);

    expect(record.type).toBe('swarm_mode.enter');
    expect(record['trigger']).toBe('task');
    expect('payload' in record).toBe(false);
  });
});

// ─── 4. OP_REGISTRY coverage for mirri-specific record types ────────────────

describe('v2 OP_REGISTRY coverage for mirri-specific record types', () => {
  const MIRRI_PERSISTED_OPS = [
    'goal.create',
    'goal.update',
    'goal.clear',
    'forked',
    'plan_mode.enter',
    'plan_mode.cancel',
    'plan_mode.exit',
    'plan.revision',
    'swarm_mode.enter',
    'swarm_mode.exit',
    'permission.set_mode',
    'permission.record_approval_result',
  ] as const;

  const MIRRI_TRANSIENT_OPS = [
    // cron.* are transient (not persisted to wire.jsonl) — same as v1
    'cron.add',
    'cron.delete',
    'cron.cursor',
    // permission.rules.add is transient — v1 does not persist rules either
    'permission.rules.add',
  ] as const;

  it('should register all mirri-specific persisted ops in OP_REGISTRY', () => {
    const missing: string[] = [];
    for (const opType of MIRRI_PERSISTED_OPS) {
      if (!OP_REGISTRY.has(opType)) {
        missing.push(opType);
      }
    }
    expect(missing).toEqual([]);
  });

  it('should register all mirri-specific transient ops in OP_REGISTRY', () => {
    const missing: string[] = [];
    for (const opType of MIRRI_TRANSIENT_OPS) {
      if (!OP_REGISTRY.has(opType)) {
        missing.push(opType);
      }
    }
    expect(missing).toEqual([]);
  });

  it('should mark cron.* ops as transient (persist: false)', () => {
    for (const opType of ['cron.add', 'cron.delete', 'cron.cursor'] as const) {
      const descriptor = OP_REGISTRY.get(opType);
      expect(descriptor, `Missing descriptor for ${opType}`).toBeDefined();
      expect(descriptor!.persist, `${opType} should be transient`).toBe(false);
    }
  });

  it('should mark permission.rules.add as transient (persist: false)', () => {
    const descriptor = OP_REGISTRY.get('permission.rules.add');
    expect(descriptor).toBeDefined();
    expect(descriptor!.persist).toBe(false);
  });

  it('should mark mirri persisted ops as persist: true (or default true)', () => {
    // Ops without an explicit `persist: false` default to true
    for (const opType of MIRRI_PERSISTED_OPS) {
      const descriptor = OP_REGISTRY.get(opType);
      expect(descriptor, `Missing descriptor for ${opType}`).toBeDefined();
      expect(
        descriptor!.persist !== false,
        `${opType} should be persisted (persist !== false)`,
      ).toBe(true);
    }
  });
});

// ─── 5. v1.4→v1.5 migration verification ───────────────────────────────────

describe('v2 1.4→1.5 migration (wallClockResumedAt backfill)', () => {
  it('should backfill wallClockResumedAt on goal.create from its time stamp', () => {
    const record: WireRecord = {
      type: 'goal.create',
      goalId: 'goal-m',
      objective: 'Migration test',
      time: 1_000,
    };

    const migrated = migrateV1_4ToV1_5.migrateRecord(record);
    expect(migrated['wallClockResumedAt']).toBe(1_000);
  });

  it('should backfill wallClockResumedAt on goal.update with status=active', () => {
    const record: WireRecord = {
      type: 'goal.update',
      status: 'active',
      time: 2_000,
    };

    const migrated = migrateV1_4ToV1_5.migrateRecord(record);
    expect(migrated['wallClockResumedAt']).toBe(2_000);
  });

  it('should backfill wallClockResumedAt on goal.update with wallClockMs (checkpoint)', () => {
    const record: WireRecord = {
      type: 'goal.update',
      wallClockMs: 5_000,
      time: 6_000,
    };

    const migrated = migrateV1_4ToV1_5.migrateRecord(record);
    expect(migrated['wallClockResumedAt']).toBe(6_000);
  });

  it('should preserve an existing wallClockResumedAt', () => {
    const record: WireRecord = {
      type: 'goal.create',
      goalId: 'goal-existing',
      objective: 'Existing anchor',
      wallClockResumedAt: 500,
      time: 1_000,
    };

    const migrated = migrateV1_4ToV1_5.migrateRecord(record);
    expect(migrated['wallClockResumedAt']).toBe(500);
  });

  it('should not touch non-goal records', () => {
    const record: WireRecord = {
      type: 'permission.set_mode',
      mode: 'auto',
      time: 1_000,
    };

    const migrated = migrateV1_4ToV1_5.migrateRecord(record);
    expect(migrated).toEqual(record);
  });

  it('should migrate a complete v1.4 journal to v1.5 with correct anchors', () => {
    // Simulates a v1.4 journal that v2 would read during restore
    const v1_4Journal: WireRecord[] = [
      { type: 'metadata', protocol_version: '1.4', created_at: 1 },
      { type: 'goal.create', goalId: 'goal-1', objective: 'Ship it', time: 100 },
      { type: 'goal.update', status: 'paused', wallClockMs: 50, time: 200 },
      { type: 'goal.update', status: 'active', time: 300 },
      { type: 'goal.update', wallClockMs: 80, time: 400 },
    ];

    const migrated = v1_4Journal.map((r) => migrateV1_4ToV1_5.migrateRecord(r));

    // goal.create gets wallClockResumedAt from its time
    expect(migrated[1]!['wallClockResumedAt']).toBe(100);
    // goal.update paused: no anchor (not an active interval start)
    expect(migrated[2]!['wallClockResumedAt']).toBeUndefined();
    // goal.update active: gets wallClockResumedAt from its time
    expect(migrated[3]!['wallClockResumedAt']).toBe(300);
    // goal.update checkpoint: gets wallClockResumedAt from its time
    expect(migrated[4]!['wallClockResumedAt']).toBe(400);
  });
});

// ─── 6. Wire protocol version parity ────────────────────────────────────────

describe('wire protocol version parity', () => {
  it('should report protocol version 1.5', () => {
    expect(WIRE_PROTOCOL_VERSION).toBe('1.5');
  });
});
