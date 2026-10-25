// apps/mirri-web/src/composables/client/useTaskPoller.ts
// Background task output polling and the 1-second task clock used to keep
// running-task elapsed timers live in the UI.
//
// S5 (the task.list polling amplifier): the poll cycle must never re-list
// the whole session every second. A heavy session holds hundreds of
// terminal tasks (one per Bash call), and hauling their full rows once per
// second grew multi-MB responses that starved the server and froze the
// renderer. The poll therefore lists ONLY the running rows
// (`activeOnly`), fetches their output tails through the single-task
// `task.output` face, and leaves terminal rows to the event stream
// (`event.task.completed` / `background.task.terminated` flip the status)
// plus a one-shot terminal backfill bounded to a few concurrent requests.

import { computed, ref, watch, type ComputedRef, type Ref } from 'vue';
import { getMirriWebApi } from '../../api';
import type { AppMessage, AppTask } from '../../api/types';
import { keepLiveSubagents, type KeepLiveSubagentsOpts } from '../../lib/taskMerge';
import { parseSwarmResult, type SwarmResult } from '../../lib/parseSwarmResult';
import type { ExtendedState } from '../useMirriWebClient';

const TASK_OUTPUT_POLL_INTERVAL_MS = 1000;
const TASK_OUTPUT_POLL_BYTES = 4096;
const TASK_OUTPUT_FINAL_BYTES = 32 * 1024;
/** How many terminal-output backfills may fly at once (the one-shot fetch
 * after a session opens; the rest queue behind these slots). */
const TERMINAL_BACKFILL_CONCURRENCY = 4;

/**
 * ADR-010 ④ keep-live 对账依赖的会话事实（transcript / 父调用定局 / 蜂群结果），
 * 由 useTaskPoller 的两处 keepLiveSubagents 调用点共用（实现在 P5-3 统一收口）。
 *
 * 数据源是 rawState.messagesBySession[sid]：
 * - transcriptLoaded：该会话消息列表已由快照/resync 填充（hasOwnProperty 语义，
 *   与 useMirriWebClient.hasLoadedMessages 一致——快照安装时无条件落数组，可能为空）。
 * - parentCallSettled：转录中存在 toolCallId 匹配的 toolResult content（父
 *   AgentSwarm/Agent 调用已定局）。
 * - swarmOutcomeFor：从父调用的 toolResult output 解析 `<agent_swarm_result>`，
 *   按 toolCallId 缓存解析结果（N-7），再按 agentId 匹配成员行取逐成员终态。
 */
function buildReconcileOpts(rawState: ExtendedState, sid: string): KeepLiveSubagentsOpts {
  // N-7：蜂群结果按 toolCallId 缓存 parseSwarmResult 解析结果。
  const swarmResultCache = new Map<string, SwarmResult | null>();

  function messagesForSession(): AppMessage[] | undefined {
    return Object.prototype.hasOwnProperty.call(rawState.messagesBySession, sid)
      ? (rawState.messagesBySession[sid] ?? [])
      : undefined;
  }

  /** 返回匹配的 toolResult content 本身（而非其 output——output 可为 undefined，
   * 以 output 判定会把已定局但无输出的父调用误判为未定局）。 */
  function findToolResultPart(toolCallId: string): { type: 'toolResult'; toolCallId: string; output: unknown; isError?: boolean } | undefined {
    const messages = messagesForSession() ?? [];
    for (const message of messages) {
      for (const part of message.content) {
        if (part.type === 'toolResult' && part.toolCallId === toolCallId) return part;
      }
    }
    return undefined;
  }

  function toolResultOutput(toolCallId: string): string | string[] | undefined {
    const part = findToolResultPart(toolCallId);
    if (part === undefined) return undefined;
    // The AgentSwarm result arrives as a plain string (or string[]) in the
    // toolResult's output; anything else carries no swarm XML.
    return typeof part.output === 'string' || Array.isArray(part.output) ? part.output : undefined;
  }

  return {
    transcriptLoaded: messagesForSession() !== undefined,
    parentCallSettled: (toolCallId) => {
      if (messagesForSession() === undefined) return false;
      return findToolResultPart(toolCallId) !== undefined;
    },
    swarmOutcomeFor: (toolCallId, agentId) => {
      const output = toolResultOutput(toolCallId);
      if (output === undefined) return undefined;
      let swarm = swarmResultCache.get(toolCallId);
      if (swarm === undefined) {
        swarm = parseSwarmResult(output);
        swarmResultCache.set(toolCallId, swarm);
      }
      return swarm?.subagents.find((sub) => sub.agentId === agentId);
    },
  };
}

export interface UseTaskPoller {
  /** 1-second clock that ticks while an active app task is running. */
  taskClock: Readonly<Ref<number>>;
  /** One-off load of the task list for a session, plus terminal-output backfill. */
  loadTasksForSession: (sessionId: string) => Promise<void>;
}

export function useTaskPoller(
  rawState: ExtendedState,
  activeAppTasks: ComputedRef<AppTask[]>,
): UseTaskPoller {
  let taskOutputPollTimer: ReturnType<typeof setInterval> | null = null;
  let lastPolledSessionId: string | undefined;
  const fetchedTerminalTaskOutputIds = new Set<string>();

  async function loadTasksForSession(sessionId: string): Promise<void> {
    try {
      const api = getMirriWebApi();
      const taskList = await api.listTasks(sessionId);
      rawState.tasksBySession = {
        ...rawState.tasksBySession,
        // Fold REST task rows into their WS-owned swarm rows (see keepLiveSubagents).
        [sessionId]: keepLiveSubagents(
          taskList,
          rawState.tasksBySession[sessionId] ?? [],
          buildReconcileOpts(rawState, sessionId),
        ),
      };
      // Completed tasks may have real terminal output that never streamed over
      // WS. Fetch it once now so the rows are expandable when the session opens.
      await fetchTerminalTaskOutputs(sessionId, taskList);
    } catch {
      // Tasks are side data; old/stale sessions may fail without blocking messages.
    }
  }

  /**
   * Fetch the final output snapshot for terminal tasks that lack real streamed
   * output. Called once after loading the task list so already-completed
   * tasks are clickable immediately. Bounded to a few concurrent requests so
   * opening a 1000+-task session does not fire 1000+ simultaneous calls.
   */
  async function fetchTerminalTaskOutputs(
    sessionId: string,
    taskList?: AppTask[],
  ): Promise<void> {
    if (rawState.activeSessionId !== sessionId) return;

    const tasks = taskList ?? rawState.tasksBySession[sessionId] ?? [];
    const api = getMirriWebApi();
    const outputByTaskId = new Map<string, { preview: string; bytes?: number }>();

    const pending = tasks.filter(
      (task) =>
        (task.status === 'completed' ||
          task.status === 'failed' ||
          task.status === 'cancelled') &&
        !fetchedTerminalTaskOutputIds.has(task.id) &&
        (task.outputLines?.length ?? 0) === 0,
    );

    for (let i = 0; i < pending.length; i += TERMINAL_BACKFILL_CONCURRENCY) {
      const batch = pending.slice(i, i + TERMINAL_BACKFILL_CONCURRENCY);
      await Promise.all(
        batch.map(async (task) => {
          try {
            const preview = await api.getTaskOutput(
              sessionId,
              task.id,
              TASK_OUTPUT_FINAL_BYTES,
            );
            if (preview.length > 0) {
              outputByTaskId.set(task.id, { preview });
            }
            // Only a definitive response marks the task as fetched — a transient
            // failure must leave it eligible for a later backfill.
            fetchedTerminalTaskOutputIds.add(task.id);
          } catch {
            // Task may have vanished between list and output; ignore.
          }
        }),
      );
    }

    if (outputByTaskId.size === 0) return;

    const existing = rawState.tasksBySession[sessionId] ?? [];
    rawState.tasksBySession = {
      ...rawState.tasksBySession,
      [sessionId]: existing.map((t) => {
        // Output was fetched by REST task id; a background subagent row folded
        // into its WS agent-id row (keepLiveSubagents) is matched via
        // backgroundTaskId, otherwise its final output would be dropped here
        // and never refetched (the REST id is already marked as fetched).
        const polled =
          outputByTaskId.get(t.id) ??
          (t.backgroundTaskId !== undefined
            ? outputByTaskId.get(t.backgroundTaskId)
            : undefined);
        if (!polled) return t;
        return { ...t, outputPreview: polled.preview, outputBytes: polled.bytes };
      }),
    };
  }

  /**
   * Poll the output of RUNNING background tasks for a session. The poll
   * lists only the running rows (`activeOnly`) and fetches each one's tail
   * through the single-task output face — the running-task count bounds the
   * request count, never the session's total task count (S5). Terminal
   * transitions arrive on the event stream; a terminal task that slipped
   * past the events is backfilled once through the same single-task face.
   */
  async function pollTaskOutputForSession(sessionId: string): Promise<void> {
    if (rawState.activeSessionId !== sessionId) return;

    const api = getMirriWebApi();
    let runningTasks: AppTask[];
    try {
      runningTasks = await api.listTasks(sessionId, undefined, { activeOnly: true });
    } catch {
      return;
    }

    const outputByTaskId = new Map<string, { preview: string; bytes?: number }>();

    await Promise.all(
      runningTasks.map(async (task) => {
        const isRunning = task.status === 'running';
        if (!isRunning) return;

        try {
          const preview = await api.getTaskOutput(sessionId, task.id, TASK_OUTPUT_POLL_BYTES);
          if (preview.length > 0) {
            outputByTaskId.set(task.id, { preview });
          }
        } catch {
          // Task may have finished between listTasks and the output call; ignore.
        }
      }),
    );

    const existing = rawState.tasksBySession[sessionId] ?? [];
    const existingById = new Map(existing.map((t) => [t.id, t] as const));

    const refreshed: AppTask[] = runningTasks.map((fresh) => {
      const old = existingById.get(fresh.id);
      const polled = outputByTaskId.get(fresh.id);
      return {
        ...fresh,
        // Preserve any WS-driven outputLines / streamed text (future taskProgress events).
        outputLines: old?.outputLines,
        text: old?.text,
        outputPreview: polled?.preview ?? old?.outputPreview ?? fresh.outputPreview,
        outputBytes: polled?.bytes ?? old?.outputBytes ?? fresh.outputBytes,
      };
    });

    // Terminal rows the event stream may have missed: they were running in
    // our store but are absent from the poll's running list. Subagent rows
    // are WS-owned (keyed by agent id, not the REST task id) — the event
    // stream and keepLiveSubagents' fold own their lifecycle, so they never
    // land here (their REST row folds by backgroundTaskId/agentId instead).
    const justFinished = existing.filter(
      (t) =>
        t.kind !== 'subagent' &&
        t.status === 'running' &&
        !runningTasks.some((fresh) => fresh.id === t.id) &&
        !fetchedTerminalTaskOutputIds.has(t.id),
    );
    const settledRows: AppTask[] = [];
    if (justFinished.length > 0) {
      try {
        // One-shot full list (low frequency — only fires when a running row
        // just left the set): the REAL terminal statuses, not a guess. The
        // per-second poll stays running-only; this is the settle net.
        const fullList = await api.listTasks(sessionId);
        const fullById = new Map(fullList.map((t) => [t.id, t] as const));
        await Promise.all(
          justFinished.map(async (task) => {
            const settled = fullById.get(task.id);
            fetchedTerminalTaskOutputIds.add(task.id);
            if (settled === undefined) return; // vanished; keep the row as-is
            let preview = settled.outputPreview;
            try {
              const tail = await api.getTaskOutput(sessionId, task.id, TASK_OUTPUT_FINAL_BYTES);
              if (tail.length > 0) preview = tail;
            } catch {
              // The summary preview stays; the task may have been cleaned up.
            }
            settledRows.push({ ...settled, outputPreview: preview });
          }),
        );
      } catch {
        // Transient failure: leave the rows running; the next poll retries.
      }
    }

    // The poll only owns the running (and just-settled) rows — keep the
    // session's terminal history intact instead of replacing it with the
    // running-only list (the Tasks dock shows completed tasks too).
    const refreshedIds = new Set([...refreshed, ...settledRows].map((t) => t.id));
    const terminalHistory = existing.filter(
      (t) => !refreshedIds.has(t.id) && t.status !== 'running',
    );

    // A session switch mid-poll must not land a stale write over the newly
    // opened session's rows (the awaits above may have straddled the switch).
    if (rawState.activeSessionId !== sessionId) return;

    rawState.tasksBySession = {
      ...rawState.tasksBySession,
      // Fold REST task rows into their WS-owned swarm rows (see keepLiveSubagents).
      [sessionId]: keepLiveSubagents(
        [...refreshed, ...settledRows, ...terminalHistory],
        existing,
        buildReconcileOpts(rawState, sessionId),
      ),
    };
  }

  function startTaskOutputPolling(sessionId: string): void {
    if (taskOutputPollTimer !== null && lastPolledSessionId === sessionId) {
      return;
    }
    stopTaskOutputPolling();
    lastPolledSessionId = sessionId;
    void pollTaskOutputForSession(sessionId);
    taskOutputPollTimer = setInterval(() => {
      if (typeof document !== 'undefined' && document.visibilityState === 'hidden') {
        return;
      }
      if (rawState.activeSessionId === sessionId) {
        void pollTaskOutputForSession(sessionId);
      } else {
        stopTaskOutputPolling();
      }
    }, TASK_OUTPUT_POLL_INTERVAL_MS);
  }

  function stopTaskOutputPolling(): void {
    if (taskOutputPollTimer !== null) {
      clearInterval(taskOutputPollTimer);
      taskOutputPollTimer = null;
    }
    lastPolledSessionId = undefined;
    fetchedTerminalTaskOutputIds.clear();
  }

  // A 1-second clock that only ticks while a task is running, so a running task's
  // elapsed-time label keeps counting up. UI task mappers read Date.now() once per
  // evaluation; without this the `tasks` computed only re-ran when tasksBySession
  // changed, freezing the timer at whatever it read on the first render.
  const taskClock = ref(0);
  let taskClockTimer: ReturnType<typeof setInterval> | null = null;
  watch(
    () => activeAppTasks.value.some((tk) => tk.status === 'running'),
    (hasRunning) => {
      if (hasRunning && taskClockTimer === null) {
        taskClockTimer = setInterval(() => {
          taskClock.value = (taskClock.value + 1) % Number.MAX_SAFE_INTEGER;
        }, 1000);
      } else if (!hasRunning && taskClockTimer !== null) {
        clearInterval(taskClockTimer);
        taskClockTimer = null;
      }
    },
    { immediate: true },
  );

  // Start/stop task output polling based on whether the active session has
  // running background tasks. This mirrors the TUI's 1-second refresh.
  watch(
    () => {
      const sid = rawState.activeSessionId;
      if (!sid) return { sid: undefined as string | undefined, hasRunning: false };
      const tasks = rawState.tasksBySession[sid] ?? [];
      return { sid, hasRunning: tasks.some((t) => t.status === 'running') };
    },
    ({ sid, hasRunning }, _prev, onCleanup) => {
      let cleanupTimer: ReturnType<typeof setTimeout> | undefined;
      if (hasRunning && sid !== undefined) {
        startTaskOutputPolling(sid);
      } else if (sid !== undefined) {
        // All tasks finished — wait a beat to catch final output, then stop.
        cleanupTimer = setTimeout(() => {
          const tasks = rawState.tasksBySession[sid] ?? [];
          if (!tasks.some((t) => t.status === 'running')) {
            stopTaskOutputPolling();
          }
        }, 1500);
      } else {
        stopTaskOutputPolling();
      }
      onCleanup(() => {
        if (cleanupTimer !== undefined) clearTimeout(cleanupTimer);
      });
    },
    { deep: true, immediate: true },
  );

  return {
    taskClock: computed(() => taskClock.value),
    loadTasksForSession,
  };
}
