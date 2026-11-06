---
'@mirri-ai/release': minor
---

Scheduled tasks, rebuilt around the job itself: a dedicated tasks page, editable and pausable tasks, a prediction timeline, and a simpler tool surface for the agent.

- The agent now has two scheduling tools instead of three: `SetTimer` for a one-off delayed callback ("check the build in 10 minutes") and `CreateScheduledTask` for a repeating task. **`CronCreate`, `CronList` and `CronDelete` are removed** (and so is the separate proposal tool — creating a task from chat is now part of `CreateScheduledTask`, approved or rejected in one confirmation card that shows every parameter).
- New bottom-line rule for the two tools: `SetTimer` is a timer; `CreateScheduledTask` is a routine and always asks for confirmation. A rejected creation notifies the originating conversation with the reason and leaves nothing behind.
- Repeating tasks are managed on their own page ("Scheduled tasks" in settings) instead of inside the chat panel: a two-column card list with status filter tabs, no per-card buttons (everything lives in the task's detail view), and an empty state that explains what a scheduled task is for.
- Tasks can be edited and paused. Editing keeps the task's identity and run history; a paused task stops firing without pretending the missed runs happened, and resuming picks up from the next future occurrence.
- The schedule preview is now a timeline: the theoretical previous run (marked as a prediction), the next run and the one after, each with its timezone. Real run history stays in the task's History tab.
- Permission mode for a task is the same manual / auto / yolo choice as a new session, applied literally: manual asks land in the task page's "Awaiting confirmation" section, auto approves the routine flow, yolo never asks. The fourth "read-only" option is gone. The create form's permission picker lands with it — until then the API accepts the three modes and the form's environment row stays workspace / model / effort.
- Tasks created by the agent appear in the page's "Awaiting confirmation" section until the user decides, the same place the notification center points to. One decision, wherever it is made, clears the item everywhere.
- The page shows the workspace's tasks only — the "all sessions / current session" view, the search box and the per-session creation entry are gone. Sessions still show their own timers read-only.

web: Fix schedule sentences showing raw translation keys in the task page (e.g. the card showed `settings.cronDescDaily` instead of "每天 09:00 执行"), and make `permissionMode` take effect when a session task is created through the API.
