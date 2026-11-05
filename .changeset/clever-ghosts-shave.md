---
'@mirri-ai/release': minor
---

Honor `MIRRICODE_DISABLE_CRON=1`: the documented switch now actually disables the cron subsystem in three layers (spec ADR-006 / requirements E.4):

- **Creation refused** — the model's `CronCreate` tool rejects new tasks with the reason `定时任务已禁用（MIRRICODE_DISABLE_CRON）`; existing tasks are left untouched (not pruned).
- **Scheduling halted** — the cron driver never enters its check loop, so existing tasks stop firing and no `cron.fired` events are published. The manual run-now face (`cron.trigger`) stays available: it is a management operation, not scheduled delivery.
- **Honest lists** — `cron.list` (and the `CronList` tool) keeps working and now always carries a top-level `disabled` field, `true` while the switch is on, so surfaces can show why nothing will fire.

The switch is read once at server startup: setting or clearing it takes effect on the next restart (no runtime hot reload). Any other value than exactly `1` leaves scheduling enabled.
