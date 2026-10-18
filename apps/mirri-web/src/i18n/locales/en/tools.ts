export default {
  label: {
    read: 'Read',
    bash: 'Run',
    edit: 'Edit',
    write: 'Write',
    grep: 'Search',
    glob: 'Find',
    ls: 'List',
    web_fetch: 'Fetch',
    search: 'Search',
    todo: 'Todo',
    task: 'Task',
    skill: 'Skill',
    swarm: 'Swarm',
    ask_user: 'Question',
  },
  swarm: {
    progress: '{done} / {total}',
    runningSub: '{count} in progress',
    doneSub: '{completed} completed · {failed} failed',
    members: 'Members',
    history: 'Work log',
    historyLoading: 'Loading work log…',
    taskLabel: 'Task',
    resultLabel: 'Result',
    phaseQueued: 'Queued',
    phaseWorking: 'Working',
    phaseSuspended: 'Suspended',
    phaseCompleted: 'Completed',
    phaseFailed: 'Failed',
    // 成员状态词汇分档（ADR-008 ①/③）：等待用户决策与用户取消的专属呈现。
    phaseAwaiting: 'Awaiting approval',
    phaseBackoff: 'Backing off',
    phaseAborted: 'Aborted',
    /** 等待审批中——等待用户（approval/question，事件驱动）成员的活动行与
     *  成员行徽章文案（ADR-008 ①），P1 推导已删（ADR-008 ④）。 */
    awaitingApproval: 'Awaiting approval',
    /** 等待作答中——问询等待与审批同构（SDR-008 ①）；P4 事件通道落地后
     *  由挂起原因填充，当前为保留词汇。 */
    awaitingAnswer: 'Awaiting your answer',
    /** 需要你处理——attention 聚合档的呈现文案（ADR-008 ②），兼作
     *  从成员聚焦视图直达审批卡的跳转按钮。 */
    attention: 'Needs your attention',
    waiting: 'Waiting for subagents…',
  },
  chip: {
    lines: '{count} lines',
    results: '{count} results',
    edited: 'edited',
    created: 'created',
    todos: '{count} items',
  },
  group: {
    title: '{count} tool call | {count} tool calls',
    running: 'running',
    error: 'failed',
    done: 'done',
  },
  ask: {
    dismissed: 'Dismissed',
    answer: '{count} answer',
    answers: '{count} answers',
    answered: 'Answered',
    more: '(+{count} more)',
  },
} as const;
