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
    /** 折叠态成员行时间线的思考元素标签（ADR-012 ②）：推理段只以 kind
     *  信号进入时间线，标签由组件层本地化（纯函数层不掺 i18n）。 */
    thinkingLabel: '思考中',
  },
  chip: {
    lines: '{count} 行',
    results: '{count} 结果',
    edited: '已编辑',
    created: '已创建',
    todos: '{count} 项',
  },
  /** Bash 行的工作目录徽标：该 `cd` 指向的就是当前工作区，命令语义上等于
   *  直接在此目录执行，因此折叠成徽标而不隐藏——悬停可见完整路径。 */
  workspacePrefixHint: '当前工作区（命令开头省略了 cd 到此目录）',
  /** 非当前工作区的目录前缀：只保留末段路径，其余压缩为 …。 */
  otherDirPrefixHint: '切换到此目录后执行',
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
