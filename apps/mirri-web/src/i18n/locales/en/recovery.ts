export default {
  noticeTitle: {
    graceful_exit: 'Tasks still running when you exited ({count})',
    service_exit: '{count} tasks interrupted by an unexpected exit',
  },
  resumeOne: 'Resume',
  dismissOne: 'Dismiss',
  resumeAll: 'Resume all ({count})',
  dismissAll: 'Dismiss all',
  openSession: 'Open session',
  closeNotice: 'Close',
  checkBackgroundHint: 'A background task may have finished — check its result before re-running',
  resumeAllFailed: 'Failed to resume {count} task(s)',
} as const;
