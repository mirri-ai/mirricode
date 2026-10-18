/**
 * Cross-engine cron wire-shape tests (F16, plans/07 T6).
 *
 * The rust kernel's `CronManager` serializes cron tasks into its session
 * wire log as the cross-language DTO (camelCase keys, epoch-millis
 * timestamps, the session id folded into `tags.sessionId`, no absolute
 * `nextFireAt`). These tests pin that shape against the TS side's own
 * declarations, so a drift on either side fails here rather than at a
 * cross-engine resume:
 *
 *   1. the `cron.add` / `cron.cursor` op lines the rust kernel writes
 *      parse cleanly through a zod mirror of the TS `CronTask` interface
 *      (`createdAt` is an epoch-millis `number`, never an RFC 3339
 *      string);
 *   2. the same lines fold through the real TS `CronModel` ops
 *      (`cronAdd` / `cronCursor`), producing a task map the TS cron
 *      engine can schedule from.
 *
 * The fixture lines below are verbatim outputs of the rust serializers
 * (`cron_task_wire` + `WireOp::CronAdd` / `WireOp::CronCursor` serde
 * forms) — see `mirricode/rust/crates/mirri-core/src/cron.rs`.
 *
 * Run: ../../node_modules/.bin/vitest run test/app/cron/rustWireCronTask.test.ts
 */
import { describe, expect, it } from 'vitest';

import { z } from 'zod';

import type { CronTask } from '#/app/cron/cronTask';
import { CRON_SESSION_TAG } from '#/app/cron/cronTask';
import { CronModel, cronAdd, cronCursor } from '#/session/cron/cronOps';

// ---------------------------------------------------------------------------
// Fixtures — verbatim rust wire-log lines
// ---------------------------------------------------------------------------

/**
 * A `cron.add` line exactly as the rust kernel writes it: the task is the
 * F16 wire DTO (camelCase, epoch-millis numbers, `tags.sessionId`). This
 * task has fired once (`lastFiredAt` present).
 */
const RUST_CRON_ADD_LINE =
  '{"type":"cron.add","task":{"createdAt":1788775200000,"cron":"*/5 * * * *","id":"cron-9f2c1a4b","lastFiredAt":1788775500000,"prompt":"heartbeat check","recurring":true,"tags":{"sessionId":"sess-42"}}}';

/**
 * A `cron.cursor` line exactly as the rust kernel writes it: the cursor is
 * an epoch-millis `number` under the wire DTO key `lastFiredAt`.
 */
const RUST_CRON_CURSOR_LINE =
  '{"type":"cron.cursor","id":"cron-9f2c1a4b","lastFiredAt":1788775800000}';

// ---------------------------------------------------------------------------
// Zod mirror of the TS `CronTask` interface (cronTask.ts)
// ---------------------------------------------------------------------------

const cronTaskShape = z.object({
  id: z.string(),
  cron: z.string(),
  prompt: z.string(),
  createdAt: z.number(),
  recurring: z.boolean().optional(),
  lastFiredAt: z.number().optional(),
  tags: z.record(z.string(), z.string()).optional(),
});

const cronAddOpShape = z.object({ type: z.literal('cron.add'), task: cronTaskShape });
const cronCursorOpShape = z.object({
  type: z.literal('cron.cursor'),
  id: z.string(),
  lastFiredAt: z.number(),
});

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe('rust cron.add wire DTO against the TS CronTask shape', () => {
  it('parses the rust op line through the zod mirror of the TS interface', () => {
    const op = cronAddOpShape.parse(JSON.parse(RUST_CRON_ADD_LINE));

    expect(op.task.id).toBe('cron-9f2c1a4b');
    expect(op.task.cron).toBe('*/5 * * * *');
    expect(op.task.prompt).toBe('heartbeat check');
    expect(op.task.recurring).toBe(true);
    // Epoch millis, not an RFC 3339 string — the legacy rust serde form
    // never crosses the wire.
    expect(op.task.createdAt).toBe(1_788_775_200_000);
    expect(op.task.lastFiredAt).toBe(1_788_775_500_000);
  });

  it('carries the session id in tags.sessionId (CRON_SESSION_TAG)', () => {
    const { task } = cronAddOpShape.parse(JSON.parse(RUST_CRON_ADD_LINE));

    expect(task.tags?.[CRON_SESSION_TAG]).toBe('sess-42');
  });

  it('does not persist an absolute nextFireAt (both engines recompute it)', () => {
    const { task } = cronAddOpShape.parse(JSON.parse(RUST_CRON_ADD_LINE)) as {
      task: Record<string, unknown>;
    };

    expect('nextFireAt' in task).toBe(false);
    expect('next_fire_at' in task).toBe(false);
  });

  it('has no extra keys beyond the TS CronTask fields', () => {
    const { task } = cronAddOpShape.parse(JSON.parse(RUST_CRON_ADD_LINE)) as {
      task: Record<string, unknown>;
    };

    expect(Object.keys(task).sort()).toEqual(
      ['createdAt', 'cron', 'id', 'lastFiredAt', 'prompt', 'recurring', 'tags'].sort(),
    );
  });
});

describe('rust cron.cursor wire shape against the TS cronCursor op', () => {
  it('parses with lastFiredAt as an epoch-millis number', () => {
    const op = cronCursorOpShape.parse(JSON.parse(RUST_CRON_CURSOR_LINE));

    expect(op.id).toBe('cron-9f2c1a4b');
    expect(op.lastFiredAt).toBe(1_788_775_800_000);
  });
});

describe('rust cron ops folding through the TS CronModel', () => {
  it('replays the rust wire lines into a schedulable task map', () => {
    // Given an empty CronModel state (what a TS-side replay starts from)
    let state = CronModel.initial();

    // When the rust-written lines fold through the real TS ops
    const addOp = cronAddOpShape.parse(JSON.parse(RUST_CRON_ADD_LINE));
    state = cronAdd.apply(state, { task: addOp.task as CronTask });
    const cursorOp = cronCursorOpShape.parse(JSON.parse(RUST_CRON_CURSOR_LINE));
    state = cronCursor.apply(state, cursorOp);

    // Then the task map holds the rust task with the cursor stamped —
    // the exact state the TS engine would have produced itself.
    expect(state.size).toBe(1);
    const task = state.get('cron-9f2c1a4b');
    expect(task).toBeDefined();
    expect(task!.createdAt).toBe(1_788_775_200_000);
    expect(task!.tags?.[CRON_SESSION_TAG]).toBe('sess-42');
    // The cursor op overwrote the add-time lastFiredAt.
    expect(task!.lastFiredAt).toBe(1_788_775_800_000);
    expect(task!.recurring).toBe(true);
  });
});
