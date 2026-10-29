export default {
  noticeTitle: {
    graceful_exit: '上次退出时仍有 {count} 个任务在进行',
    service_exit: '上次应用异常退出，{count} 个任务被中断',
  },
  resumeOne: '继续',
  dismissOne: '忽略',
  resumeAll: '全部继续（{count}）',
  dismissAll: '全部忽略',
  openSession: '打开会话',
  closeNotice: '关闭',
  checkBackgroundHint: '该后台任务可能已完成，重新执行前请先检查结果',
  resumeAllFailed: '{count} 个任务续跑失败',
} as const;
