---
'@mirri-ai/release': minor
---

Cron scheduling (stage 1, batch 3): user-created tasks and cross-session visibility. The settings cron panel can now create tasks directly (the new `cron.add` RPC — origin "user", any fire budget, the structural validations intact), and the panel's default "All sessions" view aggregates every existing session's tasks (live or persisted) through the new `cron.listAll` RPC — tasks hiding in unopened sessions are visible with their real delivery condition instead of a silently empty list. Task rows now carry origin, fire count (n / cap), and the unattended declaration. The aggregated listing caches each persisted session's replay on the wire log's fingerprint and throttles repeat aggregations to one per second, so a polling UI never re-replays unchanged logs; the panel's "next fire" rows now note that actual delivery jitters around the ideal time to prevent congestion.
