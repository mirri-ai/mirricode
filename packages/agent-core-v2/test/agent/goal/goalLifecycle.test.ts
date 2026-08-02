/**
 * Scenario: standalone goal lifecycle, budget, pause/resume, and continuation tests.
 * Responsibilities: verify goal completion with criteria, budget halts, pause/resume,
 *   and continuation driver — all without the F4 harness.
 * Wiring: real goal/wire services; non-essential collaborators stubbed.
 * Run: `pnpm --filter @mirri-ai/agent-core-v2 exec vitest run test/agent/goal/goalLifecycle.test.ts`.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { SyncDescriptor } from '#/_base/di/descriptors';
import { DisposableStore, type IDisposable } from '#/_base/di/lifecycle';
import { TestInstantiationService } from '#/_base/di/test';
import { Event } from '#/_base/event';
import { IEventBus } from '#/app/event/eventBus';
import { EventBusService } from '#/app/event/eventBusService';
import { IConfigService } from '#/app/config/config';
import { IAgentContextInjectorService } from '#/agent/contextInjector/contextInjector';
import { IAgentContextMemoryService } from '#/agent/contextMemory/contextMemory';
import { IAgentGoalService } from '#/agent/goal/goal';
import { IGoalDeadlineScheduler } from '#/agent/goal/goalDeadlineScheduler';
import { AgentGoalService } from '#/agent/goal/goalService';

type GoalServiceTestManager = IAgentGoalService & AgentGoalService;
import { IAgentLoopService } from '#/agent/loop/loop';
import { IAgentScopeContext } from '#/agent/scopeContext/scopeContext';
import { IAgentStateService } from '#/agent/state/agentState';
import { AgentStateService } from '#/agent/state/agentStateService';
import { IAgentSystemReminderService } from '#/agent/systemReminder/systemReminder';
import { IAgentToolApprovalService } from '#/agent/toolApproval/toolApproval';
import { IAgentToolExecutorService } from '#/agent/toolExecutor/toolExecutor';
import { IAgentUsageService } from '#/agent/usage/usage';
import { ITelemetryService } from '#/app/telemetry/telemetry';
import { AppendLogStore } from '#/persistence/backends/node-fs/appendLogStore';
import { InMemoryStorageService } from '#/persistence/backends/memory/inMemoryStorageService';
import { IAppendLogStore } from '#/persistence/interface/appendLogStore';
import { IFileSystemStorageService } from '#/persistence/interface/storage';
import { IWireService } from '#/wire/wire';
import type { ContextMessage } from '#/agent/contextMemory/types';
import type { DomainEvent } from '#/app/event/eventBus';

import { registerTestAgentWire, restoreTestAgentWire, testWireScope } from '../../wire/stubs';

// ---------------------------------------------------------------------------
// Stub collaborators
// ---------------------------------------------------------------------------

function noopDisposable(): { dispose: () => void } {
  return { dispose: () => undefined };
}

function hookSlot() {
  return { register: () => noopDisposable() };
}

function createLoopStub(): IAgentLoopService {
  const enqueue = vi.fn(() => ({
    assigned: new Promise<never>(() => {}),
    abort: () => false,
  }));
  return {
    _serviceBrand: undefined,
    hooks: { onWillBeginStep: hookSlot(), onDidFinishStep: hookSlot() },
    enqueue,
    run: async () => ({ type: 'completed' as const, steps: 0, truncated: false }),
    status: () => ({ state: 'idle' as const, activeTurnId: undefined, pendingTurnIds: [], hasPendingRequests: false }),
    cancel: () => false,
    tryAcquireQuiescence: () => noopDisposable(),
    hasPendingRequests: () => false,
    registerLoopErrorHandler: () => noopDisposable(),
    settled: () => Promise.resolve(),
  } as unknown as IAgentLoopService;
}

function createContextStub(): IAgentContextMemoryService {
  return {
    _serviceBrand: undefined,
    get: () => [],
    splice: () => undefined,
  } as unknown as IAgentContextMemoryService;
}

function createInjectorStub(): IAgentContextInjectorService {
  return {
    _serviceBrand: undefined,
    register: () => noopDisposable(),
    injectAfterCompaction: async () => {},
  } as unknown as IAgentContextInjectorService;
}

function createRemindersStub(): IAgentSystemReminderService {
  return {
    _serviceBrand: undefined,
    appendSystemReminder: () => undefined,
  } as unknown as IAgentSystemReminderService;
}

function createTelemetryStub(): ITelemetryService {
  return {
    _serviceBrand: undefined,
    track: () => undefined,
    track2: () => undefined,
  } as unknown as ITelemetryService;
}

function createToolExecutorStub(): IAgentToolExecutorService {
  return {
    _serviceBrand: undefined,
    onBeforeExecuteTool: Event.None,
    onWillExecuteTool: Event.None,
    hooks: { onDidExecuteTool: hookSlot() },
  } as unknown as IAgentToolExecutorService;
}

function createToolApprovalStub(): IAgentToolApprovalService {
  return {
    _serviceBrand: undefined,
    resolvePermissionResolution: async () => undefined,
    requestToolApproval: async () => undefined,
    formatDenyMessage: (message: string) => message,
    formatApprovalRejectionMessage: () => 'rejected',
  } as unknown as IAgentToolApprovalService;
}

function createConfigStub(): IConfigService {
  return {
    _serviceBrand: undefined,
    get: () => undefined,
  } as unknown as IConfigService;
}

function createUsageStub(): IAgentUsageService {
  return {
    _serviceBrand: undefined,
    onDidRecord: Event.None,
  } as unknown as IAgentUsageService;
}

function createScopeContextStub(): IAgentScopeContext {
  return {
    _serviceBrand: undefined,
    agentId: 'main',
    scope: (subKey?: string) =>
      subKey === undefined || subKey === '' ? 'wire/agents/main' : `wire/agents/main/${subKey}`,
  };
}

// ---------------------------------------------------------------------------
// Manual deadline scheduler for wall-clock budget tests
// ---------------------------------------------------------------------------

interface ManualDeadline {
  readonly dueAt: number;
  readonly callback: () => void;
  cancelled: boolean;
}

class ManualGoalDeadlineScheduler implements IGoalDeadlineScheduler {
  declare readonly _serviceBrand: undefined;

  private currentTime = 0;
  private readonly deadlines = new Set<ManualDeadline>();

  now(): number {
    return this.currentTime;
  }

  schedule(delayMs: number, callback: () => void): IDisposable {
    const deadline: ManualDeadline = {
      dueAt: this.currentTime + Math.max(0, delayMs),
      callback,
      cancelled: false,
    };
    this.deadlines.add(deadline);
    return {
      dispose: () => {
        deadline.cancelled = true;
        this.deadlines.delete(deadline);
      },
    };
  }

  advanceBy(deltaMs: number): void {
    this.currentTime += deltaMs;
    while (true) {
      const due = [...this.deadlines]
        .filter((d) => !d.cancelled && d.dueAt <= this.currentTime)
        .toSorted((left, right) => left.dueAt - right.dueAt)[0];
      if (due === undefined) return;
      this.deadlines.delete(due);
      due.callback();
    }
  }
}

// ---------------------------------------------------------------------------
// Test fixture builder
// ---------------------------------------------------------------------------

type GoalUpdatedEvent = Extract<DomainEvent, { type: 'goal.updated' }>;

interface GoalTestHost {
  readonly goals: GoalServiceTestManager;
  readonly wire: IWireService;
  readonly eventBus: IEventBus;
  readonly clock: ManualGoalDeadlineScheduler;
  readonly events: GoalUpdatedEvent[];
}

function buildHost(): GoalTestHost {
  const disposables = new DisposableStore();
  const ix = disposables.add(new TestInstantiationService());

  ix.stub(IFileSystemStorageService, new InMemoryStorageService());
  ix.set(IAppendLogStore, new SyncDescriptor(AppendLogStore));
  ix.set(IEventBus, new SyncDescriptor(EventBusService));

  const clock = new ManualGoalDeadlineScheduler();
  ix.stub(IGoalDeadlineScheduler, clock);
  ix.stub(IAgentLoopService, createLoopStub());
  ix.stub(IAgentUsageService, createUsageStub());
  ix.stub(IAgentContextMemoryService, createContextStub());
  ix.stub(IAgentContextInjectorService, createInjectorStub());
  ix.stub(IAgentSystemReminderService, createRemindersStub());
  ix.stub(ITelemetryService, createTelemetryStub());
  ix.stub(IAgentToolExecutorService, createToolExecutorStub());
  ix.stub(IAgentToolApprovalService, createToolApprovalStub());
  ix.stub(IConfigService, createConfigStub());
  ix.set(IAgentStateService, new SyncDescriptor(AgentStateService));
  ix.set(IAgentGoalService, new SyncDescriptor(AgentGoalService));

  // Get the event bus and subscribe before creating the goal service,
  // so events from goal creation are captured.
  const eventBus = ix.get(IEventBus);
  const events: GoalUpdatedEvent[] = [];
  eventBus.subscribe((event) => {
    if (event.type === 'goal.updated') events.push(event as GoalUpdatedEvent);
  });

  const wire = registerTestAgentWire(ix, testWireScope('goal-lifecycle', 'test'), {
    log: ix.get(IAppendLogStore),
    eventBus,
  });
  // Override the scope context AFTER registerTestAgentWire, which sets agentId to 'test-agent';
  // the goal service requires agentId === 'main' to support goal operations.
  ix.stub(IAgentScopeContext, createScopeContextStub());

  const goals = ix.get(IAgentGoalService) as GoalServiceTestManager;

  return { goals, wire, eventBus, clock, events };
}

// ---------------------------------------------------------------------------
// Custom host builder for tests that need specific overrides
// ---------------------------------------------------------------------------

interface CustomHostOptions {
  readonly loopService?: IAgentLoopService;
  readonly injector?: IAgentContextInjectorService;
  readonly clock?: ManualGoalDeadlineScheduler;
  readonly scope?: string;
}

function buildCustomHost(options: CustomHostOptions = {}): {
  readonly goals: GoalServiceTestManager;
  readonly disposables: DisposableStore;
} {
  const disposables = new DisposableStore();
  const ix = disposables.add(new TestInstantiationService());
  const scope = options.scope ?? 'goal-lifecycle-custom';

  ix.stub(IFileSystemStorageService, new InMemoryStorageService());
  ix.set(IAppendLogStore, new SyncDescriptor(AppendLogStore));
  ix.set(IEventBus, new SyncDescriptor(EventBusService));

  ix.stub(IGoalDeadlineScheduler, options.clock ?? new ManualGoalDeadlineScheduler());
  ix.stub(IAgentLoopService, options.loopService ?? createLoopStub());
  ix.stub(IAgentUsageService, createUsageStub());
  ix.stub(IAgentContextMemoryService, createContextStub());
  ix.stub(IAgentContextInjectorService, options.injector ?? createInjectorStub());
  ix.stub(IAgentSystemReminderService, createRemindersStub());
  ix.stub(ITelemetryService, createTelemetryStub());
  ix.stub(IAgentToolExecutorService, createToolExecutorStub());
  ix.stub(IAgentToolApprovalService, createToolApprovalStub());
  ix.stub(IConfigService, createConfigStub());
  ix.set(IAgentStateService, new SyncDescriptor(AgentStateService));
  ix.set(IAgentGoalService, new SyncDescriptor(AgentGoalService));

  registerTestAgentWire(ix, testWireScope(scope, 'test'));
  // Override AFTER registerTestAgentWire, which sets agentId to 'test-agent'.
  // The goal service requires agentId === 'main' to support goal operations.
  ix.stub(IAgentScopeContext, createScopeContextStub());

  return { goals: ix.get(IAgentGoalService) as GoalServiceTestManager, disposables };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe('AgentGoalService lifecycle (standalone)', () => {
  let host: GoalTestHost;
  let goals: GoalServiceTestManager;

  beforeEach(() => {
    host = buildHost();
    goals = host.goals;
  });

  // --- Completion criterion and markComplete ---

  it('should complete a goal that has a completion criterion', async () => {
    const created = await goals.createGoal({
      objective: 'Ship feature X',
      completionCriterion: 'All tests pass',
    });

    expect(created.status).toBe('active');
    expect(created.completionCriterion).toBe('All tests pass');

    const completed = await goals.markComplete({ reason: 'All tests pass' }, 'model');

    expect(completed).toMatchObject({
      status: 'complete',
      objective: 'Ship feature X',
      completionCriterion: 'All tests pass',
    });
    // After completion, the goal is cleared (transient status)
    expect(goals.getGoal().goal).toBeNull();
  });

  it('should emit a completion change when the goal completes', async () => {
    await goals.createGoal({
      objective: 'Ship feature X',
      completionCriterion: 'All tests pass',
    });

    await goals.markComplete({ reason: 'done' }, 'model');

    const completionEvent = host.events.find((e) => e.change?.kind === 'completion');
    expect(completionEvent?.change).toMatchObject({
      kind: 'completion',
      status: 'complete',
      reason: 'done',
    });
    // The completion event carries the final snapshot (status 'complete').
    // A separate clear event follows with snapshot null.
    expect(completionEvent?.snapshot?.status).toBe('complete');
    // After completion, the goal is cleared
    expect(goals.getGoal().goal).toBeNull();
  });

  it('should return null when marking complete a non-active goal', async () => {
    await goals.createGoal({ objective: 'work' });
    await goals.pauseGoal();

    const result = await goals.markComplete({ reason: 'done' }, 'model');
    expect(result).toBeNull();
    expect(goals.getGoal().goal?.status).toBe('paused');
  });

  // --- Budget halts ---

  it('should block when a token budget is reached', async () => {
    await goals.createGoal({ objective: 'work' });
    await goals.setBudgetLimits({ budgetLimits: { tokenBudget: 50 } }, 'model');

    const snapshot = await goals.recordTokenUsage(50);

    expect(snapshot).toMatchObject({
      status: 'blocked',
      tokensUsed: 50,
      terminalReason: 'Blocked after goal budget reached: token budget 50',
    });
    expect(goals.getGoal().goal?.budget.overBudget).toBe(true);
    expect(goals.getGoal().goal?.budget.tokenBudgetReached).toBe(true);
  });

  it('should block when a turn budget is reached', async () => {
    await goals.createGoal({ objective: 'work' });
    await goals.setBudgetLimits({ budgetLimits: { turnBudget: 1 } }, 'model');

    // First turn should be active
    await goals.incrementTurn();
    expect(goals.getGoal().goal?.status).toBe('active');

    // Setting the budget to 1 when already at 1 turn should block
    const snapshot = await goals.setBudgetLimits({ budgetLimits: { turnBudget: 1 } }, 'model');
    expect(snapshot).toMatchObject({
      status: 'blocked',
      terminalReason: 'Blocked after goal budget reached: turn budget 1',
    });
  });

  it('should block when a wall-clock budget is reached', async () => {
    await goals.createGoal({ objective: 'work' });
    await goals.setBudgetLimits({ budgetLimits: { wallClockBudgetMs: 5000 } }, 'model');

    // Advance past the budget
    host.clock.advanceBy(6000);

    // The wall-clock deadline callback should have fired and blocked the goal
    expect(goals.getGoal().goal?.status).toBe('blocked');
    expect(goals.getGoal().goal?.terminalReason).toMatch(/Blocked after goal budget reached: wall-clock budget/);
    expect(goals.getGoal().goal?.budget.wallClockBudgetReached).toBe(true);
  });

  it('should report remaining budget correctly', async () => {
    await goals.createGoal({ objective: 'work' });
    await goals.setBudgetLimits(
      { budgetLimits: { tokenBudget: 100, turnBudget: 5, wallClockBudgetMs: 10000 } },
      'model',
    );

    const snapshot = goals.getGoal().goal!;
    expect(snapshot.budget.remainingTokens).toBe(100);
    expect(snapshot.budget.remainingTurns).toBe(5);
    expect(snapshot.budget.remainingWallClockMs).toBe(10000);
    expect(snapshot.budget.overBudget).toBe(false);

    await goals.recordTokenUsage(30);
    await goals.incrementTurn();

    const after = goals.getGoal().goal!;
    expect(after.budget.remainingTokens).toBe(70);
    expect(after.budget.remainingTurns).toBe(4);
    expect(after.budget.overBudget).toBe(false);
  });

  it('should block immediately when a newly set budget is already exhausted', async () => {
    await goals.createGoal({ objective: 'work' });
    await goals.incrementTurn();

    const snapshot = await goals.setBudgetLimits({ budgetLimits: { turnBudget: 1 } }, 'model');

    expect(snapshot.status).toBe('blocked');
    expect(snapshot.terminalReason).toBe('Blocked after goal budget reached: turn budget 1');
  });

  // --- Pause/resume ---

  it('should pause an active goal and resume it', async () => {
    await goals.createGoal({ objective: 'work' });
    expect(goals.getGoal().goal?.status).toBe('active');

    const paused = await goals.pauseGoal({ reason: 'Taking a break' });
    expect(paused.status).toBe('paused');
    expect(paused.terminalReason).toBe('Taking a break');

    const resumed = await goals.resumeGoal();
    expect(resumed.status).toBe('active');
    expect(resumed.terminalReason).toBeUndefined();
  });

  it('should emit lifecycle events for pause and resume', async () => {
    await goals.createGoal({ objective: 'work' });
    host.events.length = 0;

    await goals.pauseGoal();
    expect(host.events.at(-1)?.change).toMatchObject({
      kind: 'lifecycle',
      status: 'paused',
      actor: 'user',
    });

    await goals.resumeGoal();
    expect(host.events.at(-1)?.change).toMatchObject({
      kind: 'lifecycle',
      status: 'active',
      actor: 'user',
    });
  });

  it('should no-op pause when already paused', async () => {
    await goals.createGoal({ objective: 'work' });
    const first = await goals.pauseGoal();
    const second = await goals.pauseGoal();

    expect(first.status).toBe('paused');
    expect(second.status).toBe('paused');
  });

  it('should no-op resume when already active', async () => {
    await goals.createGoal({ objective: 'work' });
    const resumed = await goals.resumeGoal();

    expect(resumed.status).toBe('active');
  });

  it('should resume a blocked goal', async () => {
    await goals.createGoal({ objective: 'work' });
    const blocked = await goals.markBlocked({ reason: 'stuck' });
    expect(blocked?.status).toBe('blocked');

    const resumed = await goals.resumeGoal();
    expect(resumed.status).toBe('active');
    expect(resumed.terminalReason).toBeUndefined();
  });

  it('should reject resuming a complete goal (already cleared)', async () => {
    await goals.createGoal({ objective: 'work' });
    await goals.markComplete({ reason: 'done' }, 'model');
    expect(goals.getGoal().goal).toBeNull();

    await expect(goals.resumeGoal()).rejects.toMatchObject({
      code: 'goal.not_found',
    });
  });

  it('should cancel a goal and clear it', async () => {
    await goals.createGoal({ objective: 'work' });
    const removed = await goals.cancelGoal();

    expect(removed.status).toBe('active');
    expect(goals.getGoal().goal).toBeNull();
  });

  it('should pause on interrupt', async () => {
    await goals.createGoal({ objective: 'work' });
    const paused = await goals.pauseOnInterrupt({ reason: 'Paused after interruption' });

    expect(paused?.status).toBe('paused');
    expect(paused?.terminalReason).toBe('Paused after interruption');
  });

  it('should return null when pausing on interrupt a non-active goal', async () => {
    await goals.createGoal({ objective: 'work' });
    await goals.pauseGoal();

    const result = await goals.pauseOnInterrupt({ reason: 'again' });
    expect(result).toBeNull();
  });

  // --- Continuation driver ---

  it('should enqueue a continuation turn when a blocked goal is resumed with continueIfBlocked', async () => {
    const loopService = createLoopStub();
    const { goals: continuationGoals, disposables } = buildCustomHost({
      loopService,
      scope: 'goal-continuation',
    });

    await continuationGoals.createGoal({ objective: 'finish the task' });
    await continuationGoals.markBlocked({ reason: 'need credentials' });

    const resumed = await continuationGoals.resumeGoal({ continueIfBlocked: true });

    expect(resumed.status).toBe('active');
    expect(loopService.enqueue).toHaveBeenCalled();

    disposables.dispose();
  });

  it('should not enqueue a continuation when a paused goal is resumed without continueIfPaused', async () => {
    const loopService = createLoopStub();
    const { goals: pausedGoals, disposables } = buildCustomHost({
      loopService,
      scope: 'goal-no-continuation',
    });

    await pausedGoals.createGoal({ objective: 'finish the task' });
    await pausedGoals.pauseGoal();

    const resumed = await pausedGoals.resumeGoal();

    expect(resumed.status).toBe('active');
    expect(loopService.enqueue).not.toHaveBeenCalled();

    disposables.dispose();
  });

  it('should enqueue a continuation when a paused goal is resumed with continueIfPaused', async () => {
    const loopService = createLoopStub();
    const { goals: pausedGoals, disposables } = buildCustomHost({
      loopService,
      scope: 'goal-paused-continue',
    });

    await pausedGoals.createGoal({ objective: 'finish the task' });
    await pausedGoals.pauseGoal();

    const resumed = await pausedGoals.resumeGoal({ continueIfPaused: true });

    expect(resumed.status).toBe('active');
    expect(loopService.enqueue).toHaveBeenCalled();

    disposables.dispose();
  });

  // --- Goal injection (reminder registration) ---

  it('should register a goal reminder through the context injector', async () => {
    const registrations: { slot: string; factory: (ctx: { isNewTurn: boolean }) => string | undefined }[] = [];
    const injectorStub: IAgentContextInjectorService = {
      _serviceBrand: undefined,
      register(slot, factory) {
        registrations.push({ slot, factory: factory as never });
        return noopDisposable();
      },
      injectAfterCompaction: async () => {},
    };

    const { goals: injectGoals, disposables } = buildCustomHost({
      injector: injectorStub,
      scope: 'goal-injection',
    });

    // GoalInjection should have registered a 'goal' slot
    const goalSlot = registrations.find((r) => r.slot === 'goal');
    expect(goalSlot).toBeDefined();

    // No reminder when no goal
    expect(goalSlot!.factory({ isNewTurn: true })).toBeUndefined();

    // Active goal produces a reminder
    await injectGoals.createGoal({ objective: 'Ship feature X' });
    const reminder = goalSlot!.factory({ isNewTurn: true });
    expect(reminder).toBeDefined();
    expect(reminder).toContain('Ship feature X');

    // Paused goal produces a different reminder
    await injectGoals.pauseGoal();
    const pausedReminder = goalSlot!.factory({ isNewTurn: true });
    expect(pausedReminder).toBeDefined();
    expect(pausedReminder!).toContain('currently paused');

    // Blocked goal produces a different reminder
    await injectGoals.resumeGoal();
    await injectGoals.markBlocked({ reason: 'stuck' });
    const blockedReminder = goalSlot!.factory({ isNewTurn: true });
    expect(blockedReminder).toBeDefined();
    expect(blockedReminder!).toContain('currently blocked');

    disposables.dispose();
  });

  it('should not inject a goal reminder mid-turn (isNewTurn=false)', async () => {
    const registrations: { slot: string; factory: (ctx: { isNewTurn: boolean }) => string | undefined }[] = [];
    const injectorStub: IAgentContextInjectorService = {
      _serviceBrand: undefined,
      register(slot, factory) {
        registrations.push({ slot, factory: factory as never });
        return noopDisposable();
      },
      injectAfterCompaction: async () => {},
    };

    const { goals: injectGoals, disposables } = buildCustomHost({
      injector: injectorStub,
      scope: 'goal-no-mid-turn',
    });

    await injectGoals.createGoal({ objective: 'Ship feature X' });

    const goalSlot = registrations.find((r) => r.slot === 'goal');
    expect(goalSlot).toBeDefined();

    // Mid-turn should not inject
    expect(goalSlot!.factory({ isNewTurn: false })).toBeUndefined();

    disposables.dispose();
  });
});
