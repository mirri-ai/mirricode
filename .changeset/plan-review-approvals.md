---
"@mirri-ai/release": patch
---

Fix plan-review approval so the plan can be approved as-is even when alternative approaches are offered, and so the chosen approach label survives every approval channel.

The web plan-review card now keeps an explicit "Approve plan" button next to the offered approaches (previously the approaches replaced it, leaving no way to approve the plan text as submitted), and the Revise feedback box gains visible Submit / Cancel buttons for touch users. When an approval is resolved through the `approval.decide` / `interaction.respond` channel — for example after a reconnect, where the parked `approval.ask` reply is gone — the selected approach label now rides the decision instead of being lost, so `ExitPlanMode` reports "Selected approach: <label>" exactly like the reverse-RPC reply path. The ACP surface also gains the spec's plan-review option namespace (`plan_approve` / `plan_opt_<i>` / `plan_revise` / `plan_reject_and_exit`) instead of the generic three options.
