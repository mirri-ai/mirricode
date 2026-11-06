---
'@mirri-ai/release': minor
---

Unified cron scheduling: held-fire persistence, trigger idle gate, fire-refresh TTL (cron-scheduling stage 1 batch 2):

- **Long-running tasks no longer die at 7 days** — a recurring task's staleness now counts from its most recent fire, not its creation (requirements B.1/B.2): an active task survives indefinitely; only a genuinely stalled one (no fire for the TTL) gets the flagged final delivery and removal.
- **Closing a session no longer discards pending fires** — fires that come due while the session is busy are now held on the task side and persisted to the session log (`cron.held`), and a session that closes with held fires redelivers them when reopened; deleting the session (or its task) clears the held fire with it (C.4). At-least-once: a crash between the injection and its clear replays the fire once more, never loses it.
- **One server-level scheduler drives every live session** — the per-session cron drivers are retired; a single loop walks all built workspaces' live sessions each tick, so one session closing never stops the others' scheduling (C.1 / V10).
- **Manual "run now" respects busy sessions** — `cron.trigger` on a busy session holds the fire through the same held channel instead of barging into the running turn; it is delivered on the first idle tick. The schedule itself stays untouched (run-now never moves the cursor or counts) (C.3 / SDR-004).
- **Late deliveries carry a lateness mark** — a fire injected more than 60s past its scheduled moment carries `lateByMs` in the `<cron-fire>` envelope and the turn origin, so the model can judge whether the task is still relevant (C.2a); on-time and run-now fires are unchanged.
