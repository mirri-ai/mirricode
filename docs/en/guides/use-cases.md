# Common use cases

This page collects typical Mirri Code scenarios along with ready-to-use prompt examples — copy them as-is or adapt them to your needs.

## Understanding an unfamiliar project

When taking over an unfamiliar repository, a good first step is to use `mirri --plan` or enable Plan mode in the web UI (type `/plan` or use the mode toggle), so the agent outputs a research plan before touching anything:

```
Give me an overview of this repository's architecture. Specifically:
1. Where is the entry point and what happens at startup?
2. How do the main modules depend on each other?
3. How are configuration and data loaded?
Finally, draw a simple module dependency diagram.
```

You can also focus on a specific question:

```
How does the event loop in src/runtime work? Where do events originate, and what consumes them?
```

```
How is "permission approval" implemented in this project? Which files are involved, and what are the key types?
```

For large-scale investigations, you can have the main agent dispatch **sub-agents** to handle sub-tasks in parallel. See [Agents and sub-agents](../customization/agents.md).

## Implementing a new feature

Describe the requirement and acceptance criteria clearly. For complex changes, use Plan mode to confirm the approach before execution:

```
Add a retry utility under src/utils:
- Signature: retry<T>(fn: () => Promise<T>, options): Promise<T>
- Options: maxAttempts, initialDelayMs, backoffFactor
- On failure, throw the error from the last attempt
- Add a unit test suite covering: success on first try, success after retries, and all attempts failing
```

If the result isn't right, just describe what you want changed — no need to edit manually:

```
The backoff calculation used a fixed value. I'd like to add some jitter to avoid the thundering-herd effect. Update the implementation and the tests.
```

## Fixing a bug

Give the symptom, reproduction steps, and expected behavior all at once to avoid back-and-forth clarification:

```
Running npm test occasionally produces this error:

  TypeError: Cannot read properties of undefined (reading 'id')
      at SessionStore.update (src/session/store.ts:142:18)

It only appears in test cases that concurrently trigger multiple updates. Please locate the cause and fix it, then run the full test suite to confirm.
```

When the root cause is unclear, ask the agent to investigate before making changes:

```
User feedback: after a successful login, the first page refresh sends you back to the login page; a second refresh works fine. Please find the most likely causes first and list the most suspicious locations. I'll confirm the direction before you start making changes.
```

For purely mechanical tasks, you can let the agent run freely:

```
Run the test suite, fix every failing test case, then run it again to confirm everything is green.
```

## Writing tests and refactoring

Tasks with clear boundaries and explicit acceptance criteria are particularly well-suited for the agent:

```
src/parser/markdown.ts currently has almost no tests. Please add a unit test suite covering: normal paragraphs, nested lists, code blocks, tables, blockquotes, and mixed content. Follow the testing style already used in the project.
```

```
Extract the repeated "read body → validate → log → respond" pattern in src/handlers into a middleware. Run the tests afterwards to make sure existing behavior is unchanged.
```

For multi-file refactors, use Plan mode first to confirm the approach. You can also use `/fork` to create an experimental branch — if you don't like the result, just switch back to the original session.

## One-off scripts and automation

Batch file edits, statistics collection, and research comparisons can all be done with a single prompt:

```
Change all var declarations in .js files under src to const or let, preferring const where possible. Run lint once you're done to confirm.
```

```
Analyze the access logs in logs/ from the past 7 days. For each API path, compute the call count, p50, and p99 response times, and output the results as a Markdown table.
```

```
Research the main dependency injection options for TypeScript (tsyringe, inversify, awilix). Compare them across three dimensions: API style, decorator requirements, and runtime overhead. Give me a recommendation that fits on one page.
```

For batch tasks you know are safe, use `--yolo` or `/yolo` to skip approval prompts, or add pre-approved allowlist rules for specific tools in [Configuration files](../configuration/config-files.md#permission).

## Scheduled tasks and reminders

Inside an interactive session, you can ask the agent to set one-time reminders or recurring tasks. The agent generates a cron expression in your local timezone and re-injects the prompt into the same session when it fires:

```
Remind me at 2:30 PM to check the deployment.
```

```
Every weekday at 9 AM, summarize recent CI failures for me.
```

```
Check the production health endpoint every hour and let me know if anything looks wrong.
```

```
Come back in about 10 minutes and check whether the build has finished.
```

Scheduled tasks are bound to their session. Closing the UI is fine — they do not fire while the whole application is closed, and when you reopen the same session, the triggers missed in between are coalesced into a single catch-up delivery. They are not carried into brand-new sessions. Recurring tasks expire after 7 days — the agent receives a `stale` signal on the final trigger and decides whether to stop or renew based on your original instructions.

To see the current tasks, open the Scheduled Tasks page in settings. To cancel one, delete it from the task's detail view, or name the task for the agent.

You can also manage them yourself in Settings → Scheduled Tasks, without going through the agent:

- **Create directly**: click New on the task page, pick a schedule and write the instructions — your own creation needs no approval.
- **One workspace at a time**: the page shows the current workspace's tasks (a session's own timers stay visible, read-only, in that session), filtered by status, so finding a task never depends on having its session open.

For the full tool reference, see [Scheduled tasks](../reference/tools.md#scheduled-tasks). The global kill switch is `MIRRICODE_DISABLE_CRON=1`.

### Workspace tasks (run without any session open)

Session-bound tasks stop when their session is gone. A **workspace task** is the other kind: it belongs to the workspace itself, fires while the application runs (no session needs to be open), and every trigger runs a fresh one-off session that finishes and records its outcome. This is the right shape for delegated routine work — "every morning, sweep the repo and tell me if anything looks wrong".

- **Create directly**: Settings → Scheduled Tasks → *New*. Beyond the schedule and instructions you pick the **permission mode** (manual — the default / auto / yolo, the same three as a new session) and a visible timezone (pre-filled with your local zone — an explicit value, never a silent inherit). Your own creation needs no approval.
- **Agent creation needs your confirmation**: when the agent asks for a task, a full draft (schedule, instructions, budget, permission mode, model and thinking effort) appears as an approval card and at the top of the task page. No permission mode can auto-approve it; only you can. The instructions are model-drafted, so review them before approving. Denying lets you attach a reason that is relayed back to the agent, and leaves no task behind.
- **Run history**: each workspace task lists its runs — status, duration, a short summary of what the run produced, and the permission operations that were refused during it. "Open run" materializes that run's session so you can read the full transcript.
- **Delivery honesty**: workspace rows say "Fires while the service runs"; session rows say "Depends on its session being alive" — both are visible in the same list.

## Generating and maintaining documentation

```
I just changed the interface signature in src/auth/login.ts. Please update the corresponding JSDoc, the example code in README, and any paragraphs in docs/en/guides that mention this interface.
```

```
For every public function under src/api that is missing a docstring, add a documentation comment following the style of the existing ones.
```

```
Based on the command implementations in src/cli, generate a draft command reference listing each subcommand, its arguments, and default values. Put it in docs/en/reference for me to review later.
```

When you need a record or a retrospective, every session keeps its full transcript as plain files under `<mirri-home>/v3_sessions/`; use `mirri session-list` to find the session, then read its `wire.jsonl`.

## Next steps

- [Agents and sub-agents](../customization/agents.md) — how to have the agent dispatch sub-tasks for parallel execution
- [Hooks](../customization/hooks.md) — trigger local scripts at task-completion and other lifecycle points
- [Built-in tools](../reference/tools.md) — full reference of all tools the agent can call
