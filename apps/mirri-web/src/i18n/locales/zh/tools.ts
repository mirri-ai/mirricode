export default {
  label: {
    read: '读取',
    bash: '运行',
    edit: '编辑',
    write: '写入',
    grep: '搜索',
    glob: '查找',
    ls: '列目录',
    web_fetch: '抓取',
    search: '搜索',
    todo: '待办',
    task: '任务',
    skill: '技能',
    swarm: 'Swarm',
    ask_user: '提问',
  },
  swarm: {
    progress: '{done} / {total}',
    runningSub: '{count} 个进行中',
    doneSub: '完成 {completed} · 失败 {failed}',
    members: '成员',
    history: '工作记录',
    historyLoading: '读取工作记录…',
    taskLabel: '任务',
    resultLabel: '结果',
    phaseQueued: '排队',
    phaseWorking: '运行中',
    phaseSuspended: '暂停',
    phaseCompleted: '完成',
    phaseFailed: '失败',
    // 成员状态词汇分档（ADR-008 ①/③）：等待用户决策与用户取消的专属呈现。
    phaseAwaiting: '等待审批中',
    phaseBackoff: '限流退避中',
    phaseAborted: '已中止',
    /** 等待审批中——等待用户（approval/question，事件驱动）成员的活动行与
     *  成员行徽章文案（ADR-008 ①），P1 推导已删（ADR-008 ④）。 */
    awaitingApproval: '等待审批中',
    /** 等待作答中——问询等待与审批同构（SDR-008 ①）；P4 事件通道落地后
     *  由挂起原因填充，当前为保留词汇。 */
    awaitingAnswer: '等待作答中',
    /** 需要你处理——attention 聚合档的呈现文案（ADR-008 ②），兼作
     *  从成员聚焦视图直达审批卡的跳转按钮。 */
    attention: '需要你处理',
    waiting: '等待子任务加入…',
  },
  chip: {
    lines: '{count} 行',
    results: '{count} 结果',
    edited: '已编辑',
    created: '已创建',
    todos: '{count} 项',
  },
  group: {
    title: '{count} 个工具调用',
    running: '运行中',
    error: '有失败',
    done: '已完成',
  },
  ask: {
    dismissed: '已忽略',
    answer: '{count} 个回答',
    answers: '{count} 个回答',
    answered: '已回答',
    more: '（还有 {count} 个）',
  },
} as const;
