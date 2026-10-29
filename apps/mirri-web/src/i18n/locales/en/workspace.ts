export default {
  // Switcher
  switcherTitle: 'Switch workspace',
  switchTooltip: 'Switch workspace',
  eyebrow: 'Workspace',
  branchLabel: 'branch: {branch}',
  noBranch: 'no branch',
  sessionCount: '{count} session | {count} sessions',
  allWorkspaces: 'All workspaces',
  currentWorkspace: 'Current workspace only',
  addWorkspace: 'Add workspace…',
  noWorkspace: 'No workspace',
  deleteHasSessions: 'This workspace still has sessions — archive them before deleting it',
  // Three-choice delete dialog (W6): purge = delete the data and unregister;
  // remove = unregister only, re-adding the same root restores the sessions.
  deletePurgeLabel: 'Delete data and remove',
  deleteRemoveLabel: 'Remove from list only',
  deletePurgeHint:
    '“Remove from list only” keeps every session on disk — re-adding the same folder path brings them back.',
  // Trust confirmation (W8, SDR-007): fired when the user SELECTS an
  // untrusted workspace — non-blocking, two actions; 暂不 keeps the
  // workspace usable with its project config honestly not-in-effect.
  trustConfirmLabel: 'Trust this workspace',
  trustLaterLabel: 'Not now',
  trustHint:
    'This workspace is not trusted yet. Its project-level config (MCP servers and the AGENTS.md project section) is not in effect.',
  untrustedTag: 'untrusted',
  untrustedTitle: 'Not trusted — project-level config (MCP servers, AGENTS.md project section) is not in effect',
  // Secondary confirmation (modal)
  removeWorkspaceConfirm: 'Remove workspace "{name}"?',
  swarmEnableConfirm: 'Enable Swarm mode? The agent will run multiple sub-agents in parallel.',
  goalStartConfirm: 'Start goal: "{objective}"? The agent will run autonomously toward it.',
  // Column-header scope toggle
  scopeCurrent: 'this workspace',
  scopeAll: 'all workspaces',
  // Group headers (all-workspaces scope)
  newInGroup: 'New session in this workspace',
  // Add-workspace dialog
  addTitle: 'Add workspace',
  recentLabel: 'Recent folders',
  cancel: 'Cancel',
  addFailed: "Couldn't open this folder. Check the path and try again.",
  // Folder browser
  openThisFolder: 'Open this folder',
  up: 'Up',
  browsing: 'Browsing…',
  filterPlaceholder: 'Filter subfolders…',
  searchPlaceholder: 'Fuzzy-search subfolders, or paste an absolute path…',
  searching: 'Searching…',
  noFilterMatch: 'No subfolders match “{q}”',
  noSubfolders: 'No subfolders here',
  gitTag: 'git',
  browseHint: 'Click a folder to enter it, then "Open this folder" to add it as a workspace.',
  // Path entry (absolute path typed into the same box)
  checkingPath: 'Checking path…',
  pathPickHint: 'Path not found — did you mean:',
  noPathMatch: 'Path not found — no matching folders under {parent}',
  badParent: 'Parent directory does not exist: {parent}',
  pathFollowHint: 'Folder located — press Enter or "Open this folder" to add it.',
  degradedPlaceholder: 'Type an absolute path, press Enter to add…',
  degradedHint: 'File browsing is unavailable — type an absolute path and press Enter to add.',
  // Attention marker
  attentionTitle: '{count} item needs your attention | {count} items need your attention',
  // Per-session pending tags (sidebar)
  awaitingAnswer: 'Answer',
  awaitingAnswerTitle: 'A question is waiting for your answer',
  awaitingPermission: 'Approve',
  awaitingPermissionTitle: 'An action is waiting for your approval',
  aborted: 'Stopped',
  abortedTitle: 'This session was interrupted before finishing',
  // Interrupted-task recovery (glossary IT1): the engine exited while the
  // session was running — its own word, never conflated with a user stop.
  interrupted: 'Interrupted',
  interruptedTitle: 'The app exited while this session was running',
  // ADR-004 (turn-scheduling O8-d): the watchdog's no-progress
  // termination gets its own word — never conflated with a user stop.
  stalled: 'Stalled',
  compressed: 'Compressed',
  compressedTitle: 'This session is compressed on disk; restore it from the archive to fork it',
  stalledTitle: 'The engine stopped this session: no progress for a long while',
} as const;
