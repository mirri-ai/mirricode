/**
 * Standalone unit tests for `SessionCronServiceImpl`.
 *
 * Tests the session-level cron engine with manually-constructed fakes for all
 * DI dependencies — no F4 harness, no vitest decorator magic. Validates:
 *
 *   - Cron fires on schedule (manual tick + fake wall clock)
 *   - Recurring tasks advance cursor after fire
 *   - One-shot tasks auto-delete after firing
 *   - Stale recurring tasks auto-expire after 7 days
 *   - Session-level scoping via `sessionId` tag
 *   - Persistence across restart (loadFromStore rehydrates)
 *   - Session ownership: tasks belonging to other sessions are filtered out
 *   - Unclaimed tasks are adopted and tagged with the current sessionId
 *
 * Run: ../../node_modules/.bin/vitest run test/session/cron/sessionCronService.test.ts
 */
import { describe, expect, it, vi } from 'vitest';

import type { ServiceIdentifier } from '#/_base/di/instantiation';
import { StateRegistry } from '#/_base/state/stateRegistry';
import { Event, Emitter } from '#/_base/event';
import type { IAgentScopeHandle } from '#/_base/di/scope';
import type { CronTask, CronTaskInit } from '#/app/cron/cronTask';
import { CRON_SESSION_TAG } from '#/app/cron/cronTask';
import type { CronConfig } from '#/app/cron/configSection';
import type { ContextMessage } from '#/agent/contextMemory/types';
import type { DomainEvent } from '#/app/event/eventBus';

import { IWireService } from '#/wire/wire';
import { IAgentPromptService } from '#/agent/prompt/prompt';
import { IAgentLoopService } from '#/agent/loop/loop';
import { IAgentToolRegistryService } from '#/agent/toolRegistry/toolRegistry';
import { IEventBus } from '#/app/event/eventBus';
import { ISessionCronService } from '#/session/cron/sessionCronService';
import { SessionCronServiceImpl } from '#/session/cron/sessionCronServiceImpl';
import { cronAdd, cronDelete, cronCursor, CronModel } from '#/session/cron/cronOps';
import { ICronCreateTool } from '#/agent/tools/cron/cron-create/cron-create';
import { ICronListTool } from '#/agent/tools/cron/cron-list/cron-list';
import { ICronDeleteTool } from '#/agent/tools/cron/cron-delete/cron-delete';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const WALL_ANCHOR = new Date(2024, 5, 1, 12, 0, 0, 0).getTime();
const MS_PER_DAY = 24 * 60 * 60 * 1000;

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

/** Fake wall clock: starts at a given ms, advances manually. */
class FakeClock {
  private _now: number;
  constructor(start: number) {
    this._now = start;
  }
  wallNow(): number {
    return this._now;
  }
  monoNowMs(): number {
    return this._now;
  }
  advance(ms: number): void {
    this._now += ms;
  }
  set(ms: number): void {
    this._now = ms;
  }
}

/** In-memory ICronTaskPersistence for testing without filesystem. */
class InMemoryCronPersistence {
  private readonly tasks = new Map<string, CronTask>();
  readonly _serviceBrand = undefined;

  async get(_workspaceId: string, taskId: string): Promise<CronTask | undefined> {
    return this.tasks.get(taskId);
  }

  async list(_query: { readonly workspaceId: string }): Promise<readonly CronTask[]> {
    return Array.from(this.tasks.values());
  }

  async save(_workspaceId: string, task: CronTask): Promise<void> {
    this.tasks.set(task.id, task);
  }

  async delete(_workspaceId: string, taskId: string): Promise<void> {
    this.tasks.delete(taskId);
  }

  /** Test helper: count stored tasks. */
  get size(): number {
    return this.tasks.size;
  }

  /** Test helper: peek at stored task. */
  peek(taskId: string): CronTask | undefined {
    return this.tasks.get(taskId);
  }
}

/** Collected steering messages from the main agent. */
interface SteerCall {
  readonly message: ContextMessage;
}

/**
 * Build a `SessionCronServiceImpl` with all DI dependencies faked.
 *
 * Returns the service plus the fake collaborators so tests can drive
 * time, inspect persistence, and observe fires.
 */
function createCronHarness(options: {
  readonly sessionId?: string;
  readonly workspaceId?: string;
  readonly noJitter?: boolean;
  readonly noStale?: boolean;
  readonly now?: number;
  readonly manualTick?: boolean;
}): {
  readonly cron: SessionCronServiceImpl;
  readonly clock: FakeClock;
  readonly persist: InMemoryCronPersistence;
  readonly steerCalls: SteerCall[];
  readonly events: DomainEvent[];
  readonly wireOps: unknown[];
  readonly mutableCronConfig: {
    debug: boolean;
    noJitter: boolean;
    noStale: boolean;
    disabled: boolean;
    manualTick: boolean;
    clock?: string;
    pollIntervalMs?: number | null;
  };
} {
  const sessionId = options.sessionId ?? 'sess-test';
  const workspaceId = options.workspaceId ?? 'ws-test';
  const clock = new FakeClock(options.now ?? WALL_ANCHOR);
  const persist = new InMemoryCronPersistence();
  const steerCalls: SteerCall[] = [];
  const events: DomainEvent[] = [];
  const wireOps: unknown[] = [];

  // CronConfig — use a mutable object so tests can flip flags at runtime.
  // The CronConfig interface has readonly properties, but our mutable
  // wrapper is read through IConfigService.get('cron') which just returns
  // the current value — the readonly constraint is only compile-time.
  const mutableCronConfig: {
    debug: boolean;
    noJitter: boolean;
    noStale: boolean;
    disabled: boolean;
    manualTick: boolean;
    clock?: string;
    pollIntervalMs?: number | null;
  } = {
    debug: false,
    noJitter: options.noJitter ?? true,
    noStale: options.noStale ?? false,
    disabled: false,
    manualTick: options.manualTick ?? true,
    clock: undefined,
    pollIntervalMs: options.manualTick !== false ? null : undefined,
  };
  const config = { cron: mutableCronConfig as CronConfig };

  // ISessionStateService — use a real StateRegistry.
  // Do NOT register state keys here; SessionCronServiceImpl's constructor
  // registers them itself. Double registration throws BugIndicatingError.
  const states = new StateRegistry();

  // ISessionContext
  const sessionContext = {
    _serviceBrand: undefined,
    sessionId,
    workspaceId,
    sessionDir: '/tmp/test-session',
    metaScope: '/tmp/test-session/meta',
    cwd: '/tmp/test-session',
    scope: (subKey?: string) =>
      subKey ? `/tmp/test-session/${subKey}` : '/tmp/test-session',
  };

  // IAgentLifecycleService — stub that can simulate main agent binding.
  const onDidCreateMain = new Emitter<IAgentScopeHandle>();

  // -- Build all fakes, keyed by their ServiceIdentifier for the accessor map --

  // IAgentPromptService — captures steer calls.
  const promptService = {
    _serviceBrand: undefined,
    inject: vi.fn(async (message: ContextMessage) => {
      steerCalls.push({ message });
    }),
  };

  // IAgentLoopService — always idle by default.
  const loopService = {
    _serviceBrand: undefined,
    status: () => ({ state: 'idle' }),
    settled: () => Promise.resolve(),
  };

  // IAgentToolRegistryService — no-op.
  const toolRegistry = {
    _serviceBrand: undefined,
    register: () => ({ dispose: () => {} }),
  };

  // IWireService — captures dispatched ops.
  const wireModel = new Map<string, CronTask>();
  const wireService = {
    _serviceBrand: undefined,
    dispatch: vi.fn((op: unknown) => {
      wireOps.push(op);
    }),
    getModel: () => wireModel,
    hooks: {
      onDidRestore: {
        register: () => ({ dispose: () => {} }),
      },
    },
  };

  // IEventBus — captures published events.
  const eventBus = {
    _serviceBrand: undefined,
    publish: vi.fn((event: DomainEvent) => {
      events.push(event);
    }),
    subscribe: () => ({ dispose: () => {} }),
  };

  // ITelemetryService — no-op.
  const telemetry = {
    _serviceBrand: undefined,
    track: vi.fn(),
    track2: vi.fn(),
    withContext: () => telemetry,
    setContext: vi.fn(),
    addAppender: () => ({ dispose: () => {} }),
    removeAppender: vi.fn(),
    setAppender: vi.fn(),
    setEnabled: vi.fn(),
    flush: async () => {},
  };

  // IConfigService — returns our config object.
  const configService = {
    _serviceBrand: undefined,
    ready: Promise.resolve(),
    onDidChangeConfiguration: Event.None,
    onDidSectionChange: Event.None,
    get: (domain: string) => {
      if (domain === 'cron') return config.cron;
      return {};
    },
    inspect: () => ({}) as never,
    getAll: () => ({}) as never,
    set: async () => {},
    replace: async () => {},
    replaceSections: async () => {},
    reload: async () => {},
    diagnostics: () => [],
  };

  // Cron tool stubs — needed by registerCronTools.
  const cronCreateTool = { _serviceBrand: undefined };
  const cronListTool = { _serviceBrand: undefined };
  const cronDeleteTool = { _serviceBrand: undefined };

  // Service identifier → fake instance map for the accessor.
  const serviceMap = new Map<ServiceIdentifier<unknown>, unknown>();
  serviceMap.set(IWireService, wireService);
  serviceMap.set(IAgentPromptService, promptService);
  serviceMap.set(IAgentLoopService, loopService);
  serviceMap.set(IAgentToolRegistryService, toolRegistry);
  serviceMap.set(IEventBus, eventBus);
  serviceMap.set(ICronCreateTool, cronCreateTool);
  serviceMap.set(ICronListTool, cronListTool);
  serviceMap.set(ICronDeleteTool, cronDeleteTool);

  // Build the main agent handle that cron binds to.
  const mainHandle: IAgentScopeHandle = {
    id: 'main',
    kind: 1 as never, // LifecycleScope.Agent
    accessor: {
      get: <T,>(id: ServiceIdentifier<T>): T => {
        const fake = serviceMap.get(id);
        if (fake !== undefined) return fake as T;
        return undefined as T;
      },
    },
    dispose: () => {},
  };

  const agentLifecycle = {
    _serviceBrand: undefined,
    onDidCreate: onDidCreateMain.event,
    onDidDispose: Event.None,
    create: async () => mainHandle,
    fork: async () => mainHandle,
    get: (agentId: string) => (agentId === 'main' ? mainHandle : undefined),
    list: () => [mainHandle],
    broadcastPermissionMode: vi.fn(),
    remove: async () => {},
  };

  // Construct the real service with faked dependencies.
  const cron = new SessionCronServiceImpl(
    states as never,
    sessionContext as never,
    persist as never,
    agentLifecycle as never,
    telemetry as never,
    configService as never,
  );

  // Override clocks AFTER construction (the constructor doesn't call
  // resolveClocks until wire restore, which we simulate next).
  // We directly set the private `clocks` field.
  (cron as unknown as { clocks: FakeClock }).clocks = clock;

  // Simulate the main agent creation binding that production does.
  // This triggers `bindMainAgent` which registers cron tools and the wire
  // restore hook. Since the constructor already calls get('main') and
  // fires onDidCreate, the bindMainAgent has already run at this point.
  onDidCreateMain.fire(mainHandle);

  return {
    cron,
    clock,
    persist,
    steerCalls,
    events,
    wireOps,
    mutableCronConfig,
  };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe('SessionCronServiceImpl', () => {
  describe('addTask', () => {
    it('creates a task with sessionId tag and persists it', async () => {
      const { cron, persist } = createCronHarness({});

      const task = cron.addTask({
        cron: '*/5 * * * *',
        prompt: 'ping',
        recurring: true,
      });

      expect(task.id).toBeDefined();
      expect(task.cron).toBe('*/5 * * * *');
      expect(task.prompt).toBe('ping');
      expect(task.recurring).toBe(true);
      expect(task.tags?.[CRON_SESSION_TAG]).toBe('sess-test');

      // Wait for async persistence to settle.
      await cron.flushPersist();
      expect(persist.size).toBe(1);
      const stored = persist.peek(task.id);
      expect(stored).toBeDefined();
      expect(stored!.tags?.[CRON_SESSION_TAG]).toBe('sess-test');
    });

    it('dispatches a cron.add op on the wire', () => {
      const { cron, wireOps } = createCronHarness({});

      cron.addTask({ cron: '0 9 * * *', prompt: 'morning', recurring: true });

      expect(wireOps.length).toBe(1);
    });

    it('generates unique ids even when called rapidly', () => {
      const { cron } = createCronHarness({});

      const ids = new Set<string>();
      for (let i = 0; i < 20; i++) {
        const task = cron.addTask({ cron: '* * * * *', prompt: `task-${i}`, recurring: true });
        ids.add(task.id);
      }

      expect(ids.size).toBe(20);
    });
  });

  describe('removeTasks', () => {
    it('removes tasks and persists the deletion', async () => {
      const { cron, persist } = createCronHarness({});

      const task = cron.addTask({ cron: '*/5 * * * *', prompt: 'ping', recurring: true });
      await cron.flushPersist();
      expect(persist.size).toBe(1);

      const removed = cron.removeTasks([task.id]);
      expect(removed).toEqual([task.id]);

      await cron.flushPersist();
      expect(persist.size).toBe(0);
      expect(cron.list()).toHaveLength(0);
    });

    it('returns empty array for unknown ids', () => {
      const { cron } = createCronHarness({});

      const removed = cron.removeTasks(['nonexistent']);
      expect(removed).toEqual([]);
    });

    it('dispatches a cron.delete op on the wire', () => {
      const { cron, wireOps } = createCronHarness({});

      const task = cron.addTask({ cron: '*/5 * * * *', prompt: 'ping', recurring: true });
      wireOps.length = 0; // clear the add op

      cron.removeTasks([task.id]);
      expect(wireOps.length).toBe(1);
    });
  });

  describe('tick — cron fires on schedule', () => {
    it('should fire a recurring task when the wall clock passes the next fire time', async () => {
      const { cron, clock, steerCalls } = createCronHarness({ now: WALL_ANCHOR });

      // Schedule a */5 task at 12:00. First ideal fire is 12:05.
      cron.addTask({ cron: '*/5 * * * *', prompt: 'five-minute ping', recurring: true });

      // Advance 6 minutes — past the 12:05 fire time.
      clock.advance(6 * 60_000);
      await cron.tick();

      expect(steerCalls.length).toBe(1);
      const msg = steerCalls[0]!.message;
      const text = msg.content
        .filter((p): p is { type: 'text'; text: string } => p.type === 'text')
        .map((p) => p.text)
        .join('');
      expect(text).toContain('five-minute ping');
      expect(text).toContain('<cron-fire');
    });

    it('should coalesce missed fires into a single steer with coalescedCount', async () => {
      const { cron, clock, steerCalls } = createCronHarness({ now: WALL_ANCHOR });

      // */5 at 12:00. Ideal fires: 12:05, 12:10, 12:15.
      cron.addTask({ cron: '*/5 * * * *', prompt: 'coalesced', recurring: true });

      // Advance 16 minutes — past 3 ideal fires.
      clock.advance(16 * 60_000);
      await cron.tick();

      expect(steerCalls.length).toBe(1);
      const origin = steerCalls[0]!.message.origin as { coalescedCount: number };
      expect(origin.coalescedCount).toBeGreaterThanOrEqual(3);
    });

    it('should not fire when cron is disabled', async () => {
      const { cron, clock, steerCalls, mutableCronConfig } = createCronHarness({ now: WALL_ANCHOR });

      mutableCronConfig.disabled = true;
      cron.addTask({ cron: '*/5 * * * *', prompt: 'ping', recurring: true });
      clock.advance(6 * 60_000);
      await cron.tick();

      expect(steerCalls.length).toBe(0);

      // Reset for other tests.
      mutableCronConfig.disabled = false;
    });

    it('should not fire when there are no tasks', async () => {
      const { cron, clock, steerCalls } = createCronHarness({ now: WALL_ANCHOR });

      clock.advance(6 * 60_000);
      await cron.tick();

      expect(steerCalls.length).toBe(0);
    });
  });

  describe('one-shot tasks', () => {
    it('should auto-delete after firing', async () => {
      const { cron, clock, steerCalls, persist } = createCronHarness({ now: WALL_ANCHOR });

      // One-shot at 12:05.
      cron.addTask({ cron: '5 12 * * *', prompt: 'one-shot', recurring: false });

      // Advance past the fire time.
      clock.advance(6 * 60_000);
      await cron.tick();

      expect(steerCalls.length).toBe(1);
      // The task should have been removed.
      expect(cron.list()).toHaveLength(0);

      await cron.flushPersist();
      expect(persist.size).toBe(0);
    });
  });

  describe('stale recurring tasks', () => {
    it('should be flagged stale after 7 days and auto-removed on fire', async () => {
      const now = WALL_ANCHOR;
      const { cron, clock, steerCalls } = createCronHarness({ now });

      // Create a task that's already 8 days old.
      const task = cron.addTask({
        cron: '*/5 * * * *',
        prompt: 'stale-task',
        recurring: true,
      });
      // Manually backdate createdAt to simulate 8 days of age.
      const eightDaysAgo = now - 8 * MS_PER_DAY;
      const staleTask: CronTask = { ...task, createdAt: eightDaysAgo };
      // Replace in the state directly.
      const tasks = (cron as unknown as { tasks: Map<string, CronTask> }).tasks;
      tasks.set(staleTask.id, staleTask);

      await cron.flushPersist();

      // Advance a bit past the next fire time.
      clock.advance(6 * 60_000);
      await cron.tick();

      expect(steerCalls.length).toBe(1);
      const origin = steerCalls[0]!.message.origin as {
        stale: boolean;
        recurring: boolean;
      };
      expect(origin.stale).toBe(true);
      expect(origin.recurring).toBe(true);

      // Stale recurring tasks are auto-removed after their final fire.
      expect(cron.list()).toHaveLength(0);
    });

    it('should not mark one-shot tasks as stale', () => {
      const { cron } = createCronHarness({ now: WALL_ANCHOR });

      const task = cron.addTask({
        cron: '0 9 * * *',
        prompt: 'one-shot',
        recurring: false,
      });

      // Manually backdate createdAt and advance clock.
      const eightDaysAgo = WALL_ANCHOR - 8 * MS_PER_DAY;
      const tasks = (cron as unknown as { tasks: Map<string, CronTask> }).tasks;
      tasks.set(task.id, { ...task, createdAt: eightDaysAgo });
      (cron as unknown as { clocks: FakeClock }).clocks.set(WALL_ANCHOR + 8 * MS_PER_DAY);

      expect(cron.isStale(cron.getTask(task.id)!)).toBe(false);
    });
  });

  describe('session-level scoping', () => {
    it('should tag new tasks with the current sessionId', () => {
      const { cron } = createCronHarness({ sessionId: 'sess-alpha' });

      const task = cron.addTask({ cron: '*/5 * * * *', prompt: 'ping', recurring: true });

      expect(task.tags?.[CRON_SESSION_TAG]).toBe('sess-alpha');
    });
  });

  describe('persistence across restart (loadFromStore)', () => {
    it('should filter out tasks that belong to a different session', async () => {
      const persist = new InMemoryCronPersistence();
      const taskA: CronTask = {
        id: 'aaa00001',
        cron: '*/5 * * * *',
        prompt: 'session-a-task',
        createdAt: WALL_ANCHOR,
        recurring: true,
        tags: { [CRON_SESSION_TAG]: 'sess-a' },
      };
      const taskB: CronTask = {
        id: 'bbb00002',
        cron: '0 9 * * *',
        prompt: 'session-b-task',
        createdAt: WALL_ANCHOR,
        recurring: true,
        tags: { [CRON_SESSION_TAG]: 'sess-b' },
      };
      await persist.save('ws-test', taskA);
      await persist.save('ws-test', taskB);

      // Create a cron service for session B with this shared persistence.
      const { cron } = createCronHarness({ sessionId: 'sess-b' });
      // Override the persist to the shared one.
      (cron as unknown as { store: InMemoryCronPersistence }).store = persist;

      await cron.loadFromStore({ replace: true });

      // Session B should only see its own task.
      const tasks = cron.list();
      expect(tasks).toHaveLength(1);
      expect(tasks[0]!.id).toBe('bbb00002');
      expect(tasks[0]!.tags?.[CRON_SESSION_TAG]).toBe('sess-b');
    });

    it('should adopt unclaimed tasks and tag them with the current sessionId', async () => {
      const persist = new InMemoryCronPersistence();
      // A task with NO session tag (from pre-session-scoping or migration).
      const unclaimed: CronTask = {
        id: 'uncl0001',
        cron: '0 9 * * *',
        prompt: 'unclaimed',
        createdAt: WALL_ANCHOR,
        recurring: true,
        // No tags at all.
      };
      await persist.save('ws-test', unclaimed);

      const { cron } = createCronHarness({ sessionId: 'sess-new' });
      (cron as unknown as { store: InMemoryCronPersistence }).store = persist;

      await cron.loadFromStore({ replace: true });

      const tasks = cron.list();
      expect(tasks).toHaveLength(1);
      expect(tasks[0]!.tags?.[CRON_SESSION_TAG]).toBe('sess-new');

      // The adopted task should be persisted with the new tag.
      await cron.flushPersist();
      const stored = persist.peek('uncl0001');
      expect(stored?.tags?.[CRON_SESSION_TAG]).toBe('sess-new');
    });

    it('should rehydrate tasks that belong to this session', async () => {
      const persist = new InMemoryCronPersistence();
      const task: CronTask = {
        id: 'abcd1234',
        cron: '*/5 * * * *',
        prompt: 'survives-restart',
        createdAt: WALL_ANCHOR,
        recurring: true,
        tags: { [CRON_SESSION_TAG]: 'sess-restart' },
        lastFiredAt: WALL_ANCHOR + 300_000,
      };
      await persist.save('ws-test', task);

      const { cron } = createCronHarness({ sessionId: 'sess-restart' });
      (cron as unknown as { store: InMemoryCronPersistence }).store = persist;

      await cron.loadFromStore({ replace: true });

      const tasks = cron.list();
      expect(tasks).toHaveLength(1);
      expect(tasks[0]!.id).toBe('abcd1234');
      expect(tasks[0]!.cron).toBe('*/5 * * * *');
      expect(tasks[0]!.prompt).toBe('survives-restart');
      expect(tasks[0]!.lastFiredAt).toBe(WALL_ANCHOR + 300_000);
    });
  });

  describe('getNextFireTime', () => {
    it('should return null when there are no tasks', () => {
      const { cron } = createCronHarness({});

      expect(cron.getNextFireTime()).toBeNull();
    });

    it('should return the earliest next fire time across all tasks', () => {
      const { cron } = createCronHarness({ now: WALL_ANCHOR });

      // "every minute" fires sooner than "9am daily".
      cron.addTask({ cron: '0 9 * * *', prompt: 'daily', recurring: true });
      cron.addTask({ cron: '* * * * *', prompt: 'every-min', recurring: true });

      const next = cron.getNextFireTime();
      expect(next).not.toBeNull();
      // The every-minute task should fire within the next minute.
      expect(next!).toBeLessThanOrEqual(WALL_ANCHOR + 60_000 + 60_000);
    });
  });

  describe('getTask / list', () => {
    it('should return a task by id', () => {
      const { cron } = createCronHarness({});

      const task = cron.addTask({ cron: '*/5 * * * *', prompt: 'ping', recurring: true });

      expect(cron.getTask(task.id)).toEqual(task);
      expect(cron.getTask('nonexistent')).toBeUndefined();
    });

    it('should list all tasks', () => {
      const { cron } = createCronHarness({});

      cron.addTask({ cron: '*/5 * * * *', prompt: 'first', recurring: true });
      cron.addTask({ cron: '0 9 * * *', prompt: 'second', recurring: false });

      expect(cron.list()).toHaveLength(2);
    });
  });

  describe('start / stop', () => {
    it('should be idempotent', async () => {
      const { cron } = createCronHarness({});

      await cron.start();
      await cron.start(); // second call is no-op
      await cron.stop();
      await cron.stop(); // second call is no-op
      expect(cron).toBeDefined();
    });

    it('should clear in-flight and last-seen state on stop', async () => {
      const { cron, clock, steerCalls } = createCronHarness({ now: WALL_ANCHOR });

      cron.addTask({ cron: '*/5 * * * *', prompt: 'ping', recurring: true });
      clock.advance(6 * 60_000);
      await cron.tick();
      expect(steerCalls.length).toBe(1);

      await cron.stop();

      // After stop, in-flight and last-seen are cleared.
      // Verify by checking that the task fires again after restart.
      await cron.start();

      // Advance past the next fire.
      clock.advance(6 * 60_000);
      await cron.tick();
      // The task should fire again since it's recurring.
      expect(steerCalls.length).toBe(2);
    });
  });

  describe('isDisabled', () => {
    it('should reflect the config disabled state', () => {
      const { cron, mutableCronConfig } = createCronHarness({});

      expect(cron.isDisabled()).toBe(false);

      mutableCronConfig.disabled = true;
      expect(cron.isDisabled()).toBe(true);

      // Reset.
      mutableCronConfig.disabled = false;
    });
  });

  describe('now', () => {
    it('should return the current wall clock time', () => {
      const { cron, clock } = createCronHarness({ now: WALL_ANCHOR });

      expect(cron.now()).toBe(WALL_ANCHOR);
      clock.advance(5_000);
      expect(cron.now()).toBe(WALL_ANCHOR + 5_000);
    });
  });
});

describe('cronOps (wire model + ops)', () => {
  it('cronAdd should add a task to the model', () => {
    const state = new Map<string, CronTask>();
    const task: CronTask = {
      id: 'deadbeef',
      cron: '*/5 * * * *',
      prompt: 'ping',
      createdAt: WALL_ANCHOR,
      recurring: true,
    };

    const next = cronAdd.apply(state, { task });

    expect(next.size).toBe(1);
    expect(next.get('deadbeef')).toEqual(task);
    // Original state should be unchanged (immutable).
    expect(state.size).toBe(0);
  });

  it('cronDelete should remove tasks from the model', () => {
    const task: CronTask = {
      id: 'deadbeef',
      cron: '*/5 * * * *',
      prompt: 'ping',
      createdAt: WALL_ANCHOR,
      recurring: true,
    };
    const state = new Map<string, CronTask>([['deadbeef', task]]);

    const next = cronDelete.apply(state, { ids: ['deadbeef'] });

    expect(next.size).toBe(0);
  });

  it('cronDelete of absent ids should return the same reference', () => {
    const state = new Map<string, CronTask>();

    const next = cronDelete.apply(state, { ids: ['nonexistent'] });

    expect(next).toBe(state); // reference equality — no-op
  });

  it('cronCursor should update lastFiredAt on the matching task', () => {
    const task: CronTask = {
      id: 'deadbeef',
      cron: '*/5 * * * *',
      prompt: 'ping',
      createdAt: WALL_ANCHOR,
      recurring: true,
    };
    const state = new Map<string, CronTask>([['deadbeef', task]]);

    const next = cronCursor.apply(state, { id: 'deadbeef', lastFiredAt: WALL_ANCHOR + 300_000 });

    expect(next.get('deadbeef')!.lastFiredAt).toBe(WALL_ANCHOR + 300_000);
    // Original unchanged.
    expect(state.get('deadbeef')!.lastFiredAt).toBeUndefined();
  });

  it('cronCursor for unknown id should return the same reference', () => {
    const state = new Map<string, CronTask>();

    const next = cronCursor.apply(state, { id: 'ghost', lastFiredAt: 42 });

    expect(next).toBe(state);
  });

  it('CronModel initial state should be an empty map', () => {
    const initial = CronModel.initial();
    expect(initial.size).toBe(0);
  });
});
