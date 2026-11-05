---
'@mirri-ai/release': minor
---

Task-creation form for workspace scheduled tasks — full delivery (batches 6-8): backend foundation, structured schedule picker, and the three-section creation form.

- New creation form in the web UI's scheduled-tasks panel, in three sections: what the task is (a display title that falls back to "untitled", and the instructions the task injects on every run), when it runs (a structured schedule picker — every hour / day / weekday / week / month / custom, no cron knowledge assumed), and the environment (workspace, permission mode, model, thinking effort, working directory — the same control the user has when starting a session).
- The structured schedule compiles to the exact behavior its preview states: a readable description, a live three-moment preview (last / next / next-next run), an explicit timezone, and an end date whose day still fires (inclusive). "Every N days/weeks/months" is a first-class elapsed-interval schedule, not an approximation via cron; a schedule ending exactly on its first run day is a one-off and the preview says so ("will run once on X").
- The environment is carried on the task and takes effect on every run exactly as configured: the chosen model, thinking effort, working directory, and permission posture (the picker shows the familiar manual / auto / yolo names plus a read-only fourth for unattended watches); nothing selected falls back to the workspace defaults, and thinking effort disables honestly for models without the capability.
- The end date is enforced by the scheduler: the task's last fire may land on its end date, after which the task is removed. Creating a task whose end date precedes its first run is rejected on both the form and the server.
- New read-only `cron.preview` RPC answers the next few occurrences of a schedule (expression or interval) from the same math the scheduler uses — the form's three-moment preview rides it.
- Task list rows and detail now show the title, the model / effort / end-date badges (only when set), and the residency three moments: last run (the most recent real run — skipped detections no longer masquerade as runs), next and next-next planned fires, with honest placeholders when absent.
