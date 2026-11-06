---
"@mirri-ai/release": minor
---

Scheduled tasks are simpler and more honest. A task the assistant proposes now needs a single approval — the card spells out the task name, schedule, workspace, model, permissions and instruction in plain language, approving creates it immediately, and the separate "needs confirmation" section is gone. A scheduled run behaves like an ordinary session turn: the run history shows what triggered each run, how late it started, and "Waiting for approval" while a manual-mode run is parked on an approval you can answer by opening the run. A run only counts as succeeded when its turn actually produced output, and run sessions are titled with the task name and date. Uncapped recurring tasks are no longer mislabelled as "Ended".
