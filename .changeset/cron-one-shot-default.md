---
"@mirri-ai/release": minor
---

Change the CronCreate `recurring` default from true to false: a bare cron + prompt now schedules a one-shot reminder that fires once and is removed. Pass `recurring: true` explicitly to keep a task rescheduling after every fire.
