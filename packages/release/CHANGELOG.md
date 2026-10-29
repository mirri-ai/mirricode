# Changelog

## 3.0.0

### Major Changes

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Retire the TypeScript engine chain and the TS CLI/TUI shell. The `mirri` rust binary is now the sole engine and CLI entry point (`mirri web` / `serve` / `acp` / `print` / `migrate` / `session-list` / `config-show` / `doctor`); the web UI and desktop apps are unchanged and keep running against the rust core. Removed with the shell: the interactive terminal UI, `mirri vis` (session visualizer), `mirri web rotate-token`, multi-instance port auto-allocation, and the self-upgrade command (re-running the install script is the upgrade path). Old-engine sessions migrate with `mirri migrate`. The npm packages `@mirri-ai/mirri-code` / `@mirri-ai/mirri-node` remain frozen at their last published versions.

### Minor Changes

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - IDE sessions now run inside the desktop engine when the desktop app is running. `mirri acp` probes for a live desktop server at startup and connects to it as a remote client, so sessions driven from the IDE appear live in the desktop UI (message deltas, tool progress, turn lifecycle) and are stored by the desktop server. When the desktop is not running — or the connection dies mid-flight — the host falls back to standalone operation with the previous behavior. Set `MIRRI_ACP_ENGINE=embedded` to force standalone mode without probing. The session directory must be a workspace already opened in the desktop app; unregistered directories are rejected with a hint to open them there first.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Update the desktop app to Electron 45, bringing the desktop shell onto the latest Chromium and security fixes. Signed and notarized macOS builds continue to show native notifications; unsigned local builds now fall back to the in-app notification banner instead of silently dropping them.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Rust CLI: new `mirri acp` subcommand — the ACP host is now part of the main `mirri` binary, so IDE integration (Zed, JetBrains) needs no separate download. The standalone `mirri-acp` release artifact is retired.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Rename the Grep `exclude_pattern` argument to `exclude_line_pattern` (breaking rename with no alias; it had zero usage so far). Its description now states it matches line text — like `| grep -v` — and points to `glob` with a `'!'` prefix for excluding files or directories, which the `glob` description now discloses as well. The result now reports how many matches the exclude pattern dropped (`excluded_matches`), and when the pattern drops nothing the result carries a note hinting at the path-dimension alternative.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Add interrupted-task recovery engine: lifecycle sentinel detects graceful vs abnormal service exit, identifies sessions whose turns were interrupted at exit, backfills an `interrupted` turn outcome, and exposes `recovery.*` RPCs to list, resume, and dismiss interrupted sessions.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - web: Add batch model deletion with checkboxes and a single config write, and keep the model filter intact after deleting. Deleting a model now also excludes it permanently — re-importing the provider never resurrects deleted models.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - web: Add a model checklist when importing a provider from the catalog — uncheck models you do not want, search within the list, flip the whole list with one select-all / unselect-all toggle, and see the import count live before confirming.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fetching private-network addresses (localhost, LAN hosts, Tailscale/CGNAT ranges) now asks for approval instead of failing outright. Approve once per host for the session via the approval prompt; cloud metadata endpoints (e.g. `169.254.169.254`) remain permanently blocked. Headless runs still refuse private-network fetches.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - A provider whose key gets rejected now shows a "needs reconfigure" badge in the settings page the moment the failure lands — with an action hint instead of a bare error — and the badge clears as soon as you save a new key.

  The settings page manages provider credentials end to end: set a key or register the Azure CLI as an external source, remove a credential, and see type / source-presence / health per provider. Editing never echoes a stored key back into the form; an env reference you type is written to the provider's env table rather than the retired config key field. Changes made from the CLI surface in an open settings page without a restart.

  `mirri credential list` gains a STATUS column (honestly "unknown" in direct mode) and an AZ-present flag for external entries; migration and its dry-run now name each skipped provider with handling guidance instead of a bare count.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Provider credentials are safer and clearer in three ways.

  When a provider's credential comes from an external source like the Azure CLI and that source fails, the error now tells you what to do (for example, run `az login`) instead of silently falling back to a possibly wrong API key from the environment.

  Saving a provider with an API key through `provider.set` or a catalog import no longer writes the key into config.toml: the key moves straight into the credential store, and the config file stays free of plaintext secrets.

  Small fixes: the `credential list` CREATED column now shows when each credential was created rather than last updated, and a defensive downcast on LLM factories returns "not found" instead of crashing on test doubles.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Provider API keys now live in a dedicated credential store instead of config.toml, so your config file can be safely shared or committed.

  Existing keys in config.toml migrate automatically on startup: nothing to do, and the old fields are cleaned up behind you. Each provider holds exactly one credential; setting a new one overwrites the old (takes effect on the very next model call, no restart).

  New `mirri credential` subcommands: `list`, `set` (hidden input), `remove`, `show` (the only plaintext reveal outlet, with a TTY warning), and `migrate --dry-run`.

  Providers can also use an external credential source such as the Azure CLI: the store keeps only a reference and the key itself never lands on disk — when the source is missing, the error says so and points at `az login` instead of pretending to be a wrong API key.

  New RPC surface for settings pages: `credential.list/set/remove/status` (no plaintext over the wire) plus a `credential.updated` notification. Deleting a provider removes its stored credential.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Merge the archived-sessions settings page into the storage-management page: the archive view now offers title search, a workspace filter, and created-time / name sorting alongside occupancy and archive time, and the standalone archived tab is retired on the v3 backend (it remains as the fallback for v1/v2 sessions backends). Compressed sessions keep their occupancy-and-age view without filtering, and archive rows now carry their creation date end to end.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Add multi-select delete to the archived-sessions management page: check several sessions (select-all over the visible, filtered set included) and delete them in one confirmed action. Selected ids that leave the list before the confirm — deleted or restored elsewhere, or moved on by auto-compression — are excluded and named in the result instead of erroring. A failed restore no longer refreshes the list out from under the user, and the compressed view's column alignment was fixed.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Keep mid-turn steers inside the turn they belong to: a message typed while the agent is working no longer splits the conversation into a new turn. In the web UI it now renders as a full user bubble with a steer badge, in place inside the running turn, with the output before and after it staying in one continuous turn.

  Steered messages now survive a restart: they are persisted as real conversation entries, so reopening a session shows them exactly where they were sent instead of losing them.

  Undo and compaction now treat a steer as real user input: undo can rewind to a steered message, and compaction keeps it when picking which user messages to preserve.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Web recovery surface for interrupted tasks: a startup notification card with per-task resume/dismiss actions, session-tree interrupted badges with in-place resume, and workspace-level interruption counts.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - On Windows, the Bash tool now runs commands through Git Bash instead of cmd (matching the documented behavior), and errors with actionable guidance when Git Bash is missing. A new PowerShell tool (Windows only) runs native PowerShell commands with faithful exit codes and UTF-8 output. Grep now falls back to the cached ripgrep binary on Windows.

### Patch Changes

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix the ACP model picker listing only the active model: it now lists every model alias configured in `config.toml` (using `displayName` when set), so IDE integrations like Zed and JetBrains can switch between all configured models.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix auto-compaction under-counting context after a compaction or undo: the fixed per-request overhead (system prompt and tool table) is now kept in the pressure measure, so the context no longer reports as mostly-empty right after compacting when the next request is actually near the window limit.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Add an in-page toggle for automatic archive compression on the archived-sessions settings page: a switch plus grace-days and scan-interval fields, so the feature can be enabled without hand-editing `config.toml`.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Make the session-export archive deterministic: entries are now ordered by name instead of following the session directory's filesystem iteration order, so two sessions with identical content export an identical entry list and zip layout.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix oversized tool outputs silently filling the context window: results over 50K characters are now saved to disk and replaced with a short preview the agent can page through with `Read`, and single-line `Grep` matches (minified JSON, bundled CSS) are capped instead of blowing past the output budget.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix turn-ending projection for user-initiated aborts: hard-aborted turns now report `cancelled` instead of `failed`, so stopping a turn no longer surfaces a "model request failed" error toast in the web UI.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - web: Show an info notice when manual compaction is skipped because there is nothing to compact, instead of silently doing nothing.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix the engine refusing to start on Windows when the `HOME` environment variable is not set (the default outside Git Bash) — the data directory now falls back to `USERPROFILE`.

- [#175](https://github.com/mirri-ai/mirricode/pull/175) [`034b36d`](https://github.com/mirri-ai/mirricode/commit/034b36dbe0a153084bf6fcf9979f708b6255d0e4) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix session recording failing on Windows with "access denied" (os error 5) — the session's `wire.jsonl` log was created but every write was rejected. Messages sent in a session are now persisted again on Windows.

## 2.2.1

### Patch Changes

- [#171](https://github.com/mirri-ai/mirricode/pull/171) [`67f3f26`](https://github.com/mirri-ai/mirricode/commit/67f3f26272e8544118b55123cd44340e4bdd4548) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix a structured question sometimes losing its answer options: a question card restored after a reconnect, or shown in a second window, now offers the same choices as the original prompt instead of only a free-text field. When the same question reaches the client twice, the card with the full option list is the one kept.

- [#174](https://github.com/mirri-ai/mirricode/pull/174) [`ed21b57`](https://github.com/mirri-ai/mirricode/commit/ed21b57df3d497eef687620af5a65cb05abff3ad) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix the server auth token file being left empty or half-written after an unclean shutdown. The token is now written durably before the server reports it ready, and two servers starting at the same time no longer overwrite each other's pending write.

## 2.2.0

### Minor Changes

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Keep the sidebar session list stable while background sessions stream: rows no longer jump to the top as conversations produce output — position now only moves when you send a message or open a session, and run/wait/finished status shows as an inline badge instead. Also cuts renderer CPU during parallel sessions: state updates are coalesced per frame and each session only refreshes its own view, and code blocks highlight once when a reply finishes instead of re-highlighting while streaming.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Upgrade the Grep tool to return ready-to-read code blocks in content mode: matching lines are grouped with line-numbered context into blocks with a `path:start-end` header, so one call answers find-and-read without a follow-up Read. Grep also accepts an array of up to 32 paths in `path` (search several known files in one pass), supports `exclude_pattern` to drop noise lines (e.g. tests, generated code) in all output modes, and tightens the content-mode default result cap to 30 matches with a 4 KB byte budget so results stay compact by default.

  Upgrade the Glob tool with result filtering and sizing: a new `filter` argument narrows matched paths by substring or regex (replacing shell pipelines like `rg --files | rg 'x'`), a new `limit` argument (default 200, `0` for the full set) replaces the fixed 100-match cap, and every result now starts with a count header telling you how many files matched before and after filtering.

  Both search tools now fall back to a cached ripgrep binary at `~/.mirri-code/bin/rg` when `rg` is not on `PATH`, so searches keep working on machines where ripgrep was bootstrapped but the shell environment lacks it.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Rename the built-in `Grep` tool's output modes for clarity and drop a deprecated `Glob` option. Use `files` instead of `files_with_matches` and `count` instead of `count_matches` when selecting how `Grep` reports results (the `Glob` tool's unused `include_dirs` option is removed). Calls still passing the old mode names are rejected; the tool now lists its valid arguments when one is unrecognized.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Add automatic archive compression: enable the `[retention]` section in `config.toml` (`enabled`, `archive_grace_days`, `interval_hours`) and archived sessions are gzip-compressed in the background once they pass the grace window. Compressed sessions stay fully readable, searchable and exportable; restoring one decompresses it transparently; forking requires a restore first. Off by default — nothing runs and nothing changes on disk until enabled.

  Add an archived-sessions management page in web settings: browse archived and compressed sessions with disk usage, savings and age columns, restore or delete single sessions, or run tiered batch cleanup for sessions compressed more than 30/90/180/365 days. Deleting a session now also reclaims image attachments no other session references. The terminal UI gains a compressed badge and a `/retention` command with per-session restore/delete.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Make the session todo list durable session state instead of a value that only lives while the session is running. Todo writes are now recorded in the session log, so resuming a session restores the list (the terminal panel no longer empties after resume), undo checkpoints include it, and compaction summaries keep it. The stale-todo reminder now also fires for the main agent, and only when the TodoList tool is available to the agent. The web panel stays populated after compaction because the list is served from the session snapshot.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Reload the skill catalog without restarting: skill directories are watched automatically, and running sessions get an updated skill listing reminder on the next turn. A manual `/skills reload` command is available in the terminal UI, and skill changes show a brief notification with what was added, removed or updated.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Skill source badges in the slash menu and skill trigger menu: every skill row now shows where it comes from (built-in, project, user, extra, or the contributing plugin's id), and menu filtering matches canonical source tokens and plugin ids alongside skill names.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - web: Merge the System Prompt settings tab into a single large preview/edit area — the editor seeds with the built-in default template when no override exists (editing and saving creates the override), a preview tab renders the effective shared prompt, and a save is disabled while the content matches the seeded default.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - web: Add a standalone System Prompt settings tab to view the effective shared system prompt, customize it (persisted as `SYSTEM.md`, with `${base_prompt}` available to embed the original), and reset it to the default with a confirmation.

### Patch Changes

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Tighten the agent's built-in guidance: investigate unfamiliar files before deleting or overwriting them, avoid noisy shell output separators, and keep progress notes to non-routine updates.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix saving a built-in agent profile from the settings page replacing the agent's full system prompt with a generic placeholder sentence — a saved profile now keeps the shared system prompt and layers its own role text on top, and saving with an empty prompt preserves the default behavior instead of degrading it. Existing profile files already carrying the placeholder are repaired automatically on load.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - core: Sync the TS base prompt template with the rust mirror (8 stale sections refreshed) and add cross-engine drift guards in both engines' test suites — the two files feed the same settings surface, so future one-sided edits now fail tests instead of silently diverging.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix the server process crashing when FetchURL fetched a page containing non-ASCII characters (such as emoji or accented letters) before a script block; the fetch now succeeds and preserves the text as-is instead of taking down the server and corrupting non-ASCII content.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix forked sessions disappearing from the session list after a web refresh: forking a conversation now records the fork in the session index immediately, so the forked session stays visible instead of being treated as an empty session until its first message.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Kimi quota-exhausted 429s now fail fast: video uploads and chat requests surface a terminal `provider.quota_exhausted` error (with the provider's Retry-After hint) instead of being retried as generic rate limits.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix structured questions silently failing after a client reconnect: when the connection dropped mid-turn, the agent's next question used to fail instantly and fall back to plain text. Questions now stay pending while waiting for the client to reconnect (up to 5 minutes by default, configurable via `[interaction] question_channel_wait_secs` in `config.toml`), and only fail with a clear error if the connection does not come back in time.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Update TLS certificate handling for a security fix: bump `webpki-roots` to 1.0.10, which removes the compromised `emSign ECC Root CA - G3` root (October 2026 eMudhra intermediate CA key compromise), and upgrade `reqwest` to 0.13 so HTTPS connections now validate against the operating system's trust store instead of a bundled root set.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Require the agent to load a matched skill's full instructions before acting on it, instead of acting on the skill's one-line description. After two failures of the same operation, the agent now stops retrying with varied arguments and re-reads the skill for a missed mechanism (context selection, environment switching, multi-instance routing).

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - web: Rank skills whose name actually contains the search text above the rest when searching the `$` skill menu with a query that matches the `skill:` prefix, e.g. `write-a-skill` now comes before `write-tui` for `$skill`.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix hundreds of "skill shadowed by a later discovery" warnings spamming the log on every startup when the workspace root is the user's home directory — the same skill folders were scanned once as project-level and again as user-level sources. Skills load exactly as before; the duplicate scans and their warnings are gone.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Strengthen the agent's built-in guidance: verify time-sensitive claims (versions, releases) against live sources, ask clarifying questions as options with a default action, keep URLs clickable in terminal output, and treat tests as defect-finding with gradual scope escalation.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix two stale statements in the agent's built-in guidance: secret files are escalated through the permission flow rather than refused outright by the file tools, and context compaction keeps a recent retention tail plus the user's messages rather than dropping a middle block.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix TaskOutput and TaskStop failing with "task not found" when the agent sent a background task id without its `task-` prefix. The engine now recognizes the bare id and reads or stops the task as intended.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix the sidebar's "Answer" badge staying on a session after the user already answered the agent's question in the web UI.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix the web UI showing a scary "Cannot load current session" warning when archiving a session: the in-flight snapshot sync error from the engine (JSON-RPC code -32001) is now recognized as session-not-found and handled gracefully — the session is silently removed from the list and the app switches to the next session without any warning toast.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - web: Fix contradictory status shown for parallel subagents — a finished member kept the pulsing "working" indicator forever, while a still-running member showed the completed checkmark with a "Working" summary. Member status now comes from a single source, and rows whose finish event was missed are reconciled closed on the next refresh instead of staying stuck.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - web: Stop flagging pasted log and code text as invalid skill references before sending — the pre-send warning now only checks skill chips actually inserted from the menu, and "remove invalid references" can no longer delete pasted text. Drafts saved by older builds with compact `[$name]` chips now resolve back to full skill links on load.

- [#169](https://github.com/mirri-ai/mirricode/pull/169) [`9dadb3c`](https://github.com/mirri-ai/mirricode/commit/9dadb3c56015f0533b807c3efb448c4e09b5a124) Thanks [@mirri-ai](https://github.com/mirri-ai)! - web: Fix the subagent detail panel showing duplicated or re-streamed content — a retried step no longer double-appends the member's streamed text and reasoning, the member's history now renders after the run ends, and long-running progress lines are capped.

## 2.1.0

### Minor Changes

- [#160](https://github.com/mirri-ai/mirricode/pull/160) [`debfac3`](https://github.com/mirri-ai/mirricode/commit/debfac357c2e0f5d151cf57c96b7fabbde86a60e) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Change the agent profile `subagents` field to default-deny semantics: a profile without an explicit `subagents` allowlist can no longer dispatch any subagent (previously it could dispatch all of them). The built-in main agent ships with the built-in subagents allowlisted, so the out-of-the-box experience is unchanged; users who relied on the old "omitted = all" behavior must add the subagent names they want to allowlist.

### Patch Changes

- [#160](https://github.com/mirri-ai/mirricode/pull/160) [`debfac3`](https://github.com/mirri-ai/mirricode/commit/debfac357c2e0f5d151cf57c96b7fabbde86a60e) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix blank Finder and Terminal entries in the desktop "Open In" menu by extracting their icons from the macOS system app bundles.

- [#160](https://github.com/mirri-ai/mirricode/pull/160) [`debfac3`](https://github.com/mirri-ai/mirricode/commit/debfac357c2e0f5d151cf57c96b7fabbde86a60e) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix the web/desktop transcript ordering for inline skill activations: skill-activated cards no longer float above the previous assistant turn; they now render in order within their own message's turn.

- [#160](https://github.com/mirri-ai/mirricode/pull/160) [`debfac3`](https://github.com/mirri-ai/mirricode/commit/debfac357c2e0f5d151cf57c96b7fabbde86a60e) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix skill.list session routing: the composer slash-menu skill list now follows the active session's workspace, so project-level skills under a workspace's .agents/skills are visible again in desktop and web.

- [#160](https://github.com/mirri-ai/mirricode/pull/160) [`debfac3`](https://github.com/mirri-ai/mirricode/commit/debfac357c2e0f5d151cf57c96b7fabbde86a60e) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix the AskUserQuestion free-text "Other" input disappearing after a tab refocus or reconnect: snapshot-restored question cards now carry the same allowOther flag the live card always had.

- [#160](https://github.com/mirri-ai/mirricode/pull/160) [`debfac3`](https://github.com/mirri-ai/mirricode/commit/debfac357c2e0f5d151cf57c96b7fabbde86a60e) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix subagents spawned mid-session starting without the session's connected MCP tools, which left a subagent profile's MCP tool authorization unusable until the app restarted. A newly spawned subagent now receives the tools the session already has connected.

- [#160](https://github.com/mirri-ai/mirricode/pull/160) [`debfac3`](https://github.com/mirri-ai/mirricode/commit/debfac357c2e0f5d151cf57c96b7fabbde86a60e) Thanks [@mirri-ai](https://github.com/mirri-ai)! - web: Keep markdown tables inside the reading column instead of stretching them to their content width.

## 2.0.1

### Patch Changes

- [#158](https://github.com/mirri-ai/mirricode/pull/158) [`4e2cca8`](https://github.com/mirri-ai/mirricode/commit/4e2cca8e699ca8f96eceb54ec95ff3b536be8f63) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix the Linux desktop artifacts failing to build, so the AppImage and `.deb` installers ship again.

- [#155](https://github.com/mirri-ai/mirricode/pull/155) [`7964c23`](https://github.com/mirri-ai/mirricode/commit/7964c2321af194df102dd843d2cdf871e5f49d06) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Show each installed macOS app's real icon in the chat "Open In" menu; previously every app displayed the same placeholder icon.

- [#155](https://github.com/mirri-ai/mirricode/pull/155) [`7964c23`](https://github.com/mirri-ai/mirricode/commit/7964c2321af194df102dd843d2cdf871e5f49d06) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix the native release pipeline failing to package the CLI and ACP binaries on every platform, which had prevented native releases from shipping.

- [#157](https://github.com/mirri-ai/mirricode/pull/157) [`b3bf8c7`](https://github.com/mirri-ai/mirricode/commit/b3bf8c79408e774e978c5ecec4f38b7d4d5328de) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix session titles sometimes staying untitled: when the first-turn title generation was still running as the turn ended, its retry was skipped, so a failed generation never fell back to naming the session from your prompt.

- [#155](https://github.com/mirri-ai/mirricode/pull/155) [`7964c23`](https://github.com/mirri-ai/mirricode/commit/7964c2321af194df102dd843d2cdf871e5f49d06) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix project skills from other workspaces not showing in the web skills panel: the skills list now answers from the selected workspace's own catalog instead of always the default workspace's.

- [#155](https://github.com/mirri-ai/mirricode/pull/155) [`7964c23`](https://github.com/mirri-ai/mirricode/commit/7964c2321af194df102dd843d2cdf871e5f49d06) Thanks [@mirri-ai](https://github.com/mirri-ai)! - web: Fix the "Open In" menu in the chat header being unclickable in the desktop app, where clicks were swallowed as window drags.

- [#155](https://github.com/mirri-ai/mirricode/pull/155) [`7964c23`](https://github.com/mirri-ai/mirricode/commit/7964c2321af194df102dd843d2cdf871e5f49d06) Thanks [@mirri-ai](https://github.com/mirri-ai)! - web: Remove the per-session Activate button from the skills panel.

## 2.0.0

### Major Changes

- [#148](https://github.com/mirri-ai/mirricode/pull/148) [`a19522c`](https://github.com/mirri-ai/mirricode/commit/a19522ca0b98e048bf7c5bb72bfbf535ce96e728) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Remove the managed account login. The `mirri login` and `mirri auth` subcommands and the `/login` and `/logout` slash commands are gone, the web UI opens a provider setup page instead of a sign-in gate, and ACP clients no longer see account authentication. Providers previously set up with OAuth need an explicit API key.

### Minor Changes

- [#148](https://github.com/mirri-ai/mirricode/pull/148) [`a19522c`](https://github.com/mirri-ai/mirricode/commit/a19522ca0b98e048bf7c5bb72bfbf535ce96e728) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Add Markdown agent profiles with same-name overrides, enable/disable controls and full editing from the CLI and web settings, alongside the built-in skills and per-skill enablement. MCP configuration becomes layered per user, workspace and plugin, with live pool reconciliation, OAuth support and automatic reconnect.

- [#150](https://github.com/mirri-ai/mirricode/pull/150) [`8d09f2c`](https://github.com/mirri-ai/mirricode/commit/8d09f2c0fab74810e4644776edd6543f5221637b) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Detect locally installed developer tools (editors, terminals, file managers) in the desktop app and offer them, each with its own application icon, in a dedicated "Open in" dropdown next to the session title. The last used target opens with one click; the old menu entry is replaced by the dropdown. On Windows and Linux the dropdown offers the system file manager, opened through the OS default handler.

- [#148](https://github.com/mirri-ai/mirricode/pull/148) [`a19522c`](https://github.com/mirri-ai/mirricode/commit/a19522ca0b98e048bf7c5bb72bfbf535ce96e728) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Broaden provider and model support with Google (Gemini) and Kimi, Anthropic prompt caching and one-time attachment uploads, and model resolution that never silently swaps your selection. Usage reporting now includes cache read and write tokens, and provider calls are bounded by timeouts with quota errors surfaced instead of retried.

- [#148](https://github.com/mirri-ai/mirricode/pull/148) [`a19522c`](https://github.com/mirri-ai/mirricode/commit/a19522ca0b98e048bf7c5bb72bfbf535ce96e728) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Make the native Rust engine the core behind the CLI, desktop app, web UI and ACP clients, replacing the TypeScript core, which no longer receives new work. Every client now receives the same live event stream, including tool progress, thinking deltas, compaction lifecycle and background task events. Run `mirri migrate` to carry existing sessions over, and `mirri web` to serve the built-in web UI.

- [#148](https://github.com/mirri-ai/mirricode/pull/148) [`a19522c`](https://github.com/mirri-ai/mirricode/commit/a19522ca0b98e048bf7c5bb72bfbf535ce96e728) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Rework sessions: fork at any completed turn, resume from the session log without losing history, and run compaction with real lifecycle events. Sessions become workspace-aware — register workspaces, list and search sessions across them, choose the workspace a new session belongs to, and gate project-scoped MCP servers and instructions behind a trust flag.

- [#148](https://github.com/mirri-ai/mirricode/pull/148) [`a19522c`](https://github.com/mirri-ai/mirricode/commit/a19522ca0b98e048bf7c5bb72bfbf535ce96e728) Thanks [@mirri-ai](https://github.com/mirri-ai)! - web: Expand settings to cover providers and models with catalog import, MCP servers, skills, agent profiles, permission rules, cron jobs and background tasks. The chat gains inline skill chips, attachment tiles, a terminal panel, per-turn usage stats, workspace switching and clearer approval and question cards.

### Patch Changes

- [#150](https://github.com/mirri-ai/mirricode/pull/150) [`8d09f2c`](https://github.com/mirri-ai/mirricode/commit/8d09f2c0fab74810e4644776edd6543f5221637b) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix the desktop app's native integrations (open in app, reveal in file manager, notifications) failing with "no native launch channel" after the app window loaded, restoring the desktop bridge that the build had accidentally broken.

- [#148](https://github.com/mirri-ai/mirricode/pull/148) [`a19522c`](https://github.com/mirri-ai/mirricode/commit/a19522ca0b98e048bf7c5bb72bfbf535ce96e728) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix a batch of stability problems: healthy turns flagged as stalled, background subagents left running after a stall, memory spikes when reopening long sessions, web sessions stuck after a stop, dropped or duplicated terminal and event frames, and file uploads truncated on binary payloads.

- [#150](https://github.com/mirri-ai/mirricode/pull/150) [`8d09f2c`](https://github.com/mirri-ai/mirricode/commit/8d09f2c0fab74810e4644776edd6543f5221637b) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix a daemon crash when archiving a session whose wire log contains a step that began but never ended: replaying such a session left the token-coverage counter pointing past the end of the reconstructed message history, and the next context-token read panicked and killed the server (web showed "not connected" and realtime reconnect failures). The coverage counter is now retracted whenever history is shortened, and the pending-token read clamps defensively so a future invariant break degrades to a mis-estimate instead of a crash.

- [#150](https://github.com/mirri-ai/mirricode/pull/150) [`8d09f2c`](https://github.com/mirri-ai/mirricode/commit/8d09f2c0fab74810e4644776edd6543f5221637b) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix live goal and skill-activation updates not appearing in a forked session until the page was refreshed. The forked session's agent was announcing these changes on the original session's event stream, so its own clients never received them in real time.

- [#152](https://github.com/mirri-ai/mirricode/pull/152) [`186727f`](https://github.com/mirri-ai/mirricode/commit/186727f8d43e1122234efb72ffdc46e6c79cd06e) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix workspace file grep returning no matches while scanning nothing. `rg` was invoked without a search path, and with a non-terminal stdin (the normal case for a server, daemon or CI run) ripgrep then searches stdin instead of the workspace — so greps silently came back empty. The search path is now explicit, stdin is closed, and the scanned-file count falls back to counting the matching candidates when ripgrep's own report omits it (ripgrep 14 and earlier only count files that produced output).

- [#152](https://github.com/mirri-ai/mirricode/pull/152) [`186727f`](https://github.com/mirri-ai/mirricode/commit/186727f8d43e1122234efb72ffdc46e6c79cd06e) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix the Linux startup orphan sweep treating an already-terminated process as still alive. A killed child that has not been reaped yet keeps its `/proc` entry and process-group id, so the sweep re-verified it as a live orphan, left its task archive `Running`, and burned the full grace window before escalating to `SIGKILL`. Zombie processes now probe as dead and no longer count as live process-group members, so such archives settle `Stopped` (and are disposed under the terminate policy) immediately.

- [#152](https://github.com/mirri-ai/mirricode/pull/152) [`186727f`](https://github.com/mirri-ai/mirricode/commit/186727f8d43e1122234efb72ffdc46e6c79cd06e) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix a session title set just before the session was closed being lost on resume. The restored session looked untitled and ran the title model call again on its next turn; the close path now persists the current metadata before tearing the session down.

- [#150](https://github.com/mirri-ai/mirricode/pull/150) [`8d09f2c`](https://github.com/mirri-ai/mirricode/commit/8d09f2c0fab74810e4644776edd6543f5221637b) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix a wedged provider SSE stream hanging agent turns indefinitely: streams that only send keep-alive comments (or nothing at all) now fail after a 300-second idle timeout instead of hanging until the user aborts, and SSE read/decode errors are now retried instead of failing on the first attempt.

- [#150](https://github.com/mirri-ai/mirricode/pull/150) [`8d09f2c`](https://github.com/mirri-ai/mirricode/commit/8d09f2c0fab74810e4644776edd6543f5221637b) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix terminal panel failing with "session not found" for sessions outside the default workspace (the web/desktop terminal panel now resolves sessions through workspace routing, including on-demand resume after a server restart).

- [#150](https://github.com/mirri-ai/mirricode/pull/150) [`8d09f2c`](https://github.com/mirri-ai/mirricode/commit/8d09f2c0fab74810e4644776edd6543f5221637b) Thanks [@mirri-ai](https://github.com/mirri-ai)! - web: Rework the finished thinking block into a collapsed "Thought · duration" header that expands in place to a scrollable ~8-line window; the streaming preview window shrinks to 2 lines.

## 1.11.0
