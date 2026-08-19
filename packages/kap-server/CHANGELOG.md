# @mirri-ai/kap-server

## 0.4.0

### Minor Changes

- [#146](https://github.com/mirri-ai/mirricode/pull/146) [`1aa104b`](https://github.com/mirri-ai/mirricode/commit/1aa104b98e14a851f0f9972bc1b80631225d0943) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Remove the experimental secondary-model option for subagents; subagent model selection now takes an explicit model selector or inherits the caller's model.

- [#146](https://github.com/mirri-ai/mirricode/pull/146) [`1aa104b`](https://github.com/mirri-ai/mirricode/commit/1aa104b98e14a851f0f9972bc1b80631225d0943) Thanks [@mirri-ai](https://github.com/mirri-ai)! - v1→v2 engine migration: the SDK and CLI now run the agent-core-v2 engine by default.

  - `mirri-code-sdk` gains a v2 backend (`createMirriHarnessV2`) over the same
    `MirriHarness` surface: session lifecycle, turn driving, plan/goal/cron/
    background, skills, plugins, MCP, approvals and questions are all bridged
    from the v2 engine to the frozen v1 wire contract.
  - The CLI's TUI defaults to the v2 engine. Set `MIRRICODE_LEGACY_FLAG=1` to
    force the legacy agent-core backend during the migration window.
  - `kap-server` is now the only server (`mirri web`); the v1 `packages/server`
    and its `mirri server` command tree were retired (`mirri server` prints a
    deprecation notice and exits; `mirri web` is the replacement).
  - New `@mirri-ai/acp-server` package: a v2-native Agent Client Protocol host
    backed directly by agent-core-v2.
  - `@mirri-ai/klient` facade extended with skills, thinking, and the matching
    wire-contract entries.

### Patch Changes

- Updated dependencies [[`1aa104b`](https://github.com/mirri-ai/mirricode/commit/1aa104b98e14a851f0f9972bc1b80631225d0943), [`1aa104b`](https://github.com/mirri-ai/mirricode/commit/1aa104b98e14a851f0f9972bc1b80631225d0943), [`1aa104b`](https://github.com/mirri-ai/mirricode/commit/1aa104b98e14a851f0f9972bc1b80631225d0943), [`1aa104b`](https://github.com/mirri-ai/mirricode/commit/1aa104b98e14a851f0f9972bc1b80631225d0943), [`1aa104b`](https://github.com/mirri-ai/mirricode/commit/1aa104b98e14a851f0f9972bc1b80631225d0943)]:
  - @mirri-ai/agent-profile@0.2.0
  - @mirri-ai/agent-core-v2@0.5.0

## 0.3.0

### Minor Changes

- [#134](https://github.com/mirri-ai/mirricode/pull/134) [`4bc9a36`](https://github.com/mirri-ai/mirricode/commit/4bc9a36778640baad7e2f805311c6dc4ec1f6427) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Introduce the agent-core-v2 engine as a parallel backend that coexists with the v1 engine, plus a wire protocol bump to 1.5.

  - Add the v2 engine packages (agent-core-v2, kap-server, transcript, klient, v2-oauth, minidb, tree-sitter-bash) that port the 20 mirri features and close the 4 v2 gaps (G1-G4) with golden-test parity to v1.
  - Bump the wire protocol from 1.4 to 1.5 and register the v1.4→v1.5 wall-clock anchor migration so existing records backfill the missing anchor from create and resume timestamps.
  - web: Add a "start from" selector in the agent profiles panel so users can create a custom profile based on a builtin profile without the extends chain.

- [#138](https://github.com/mirri-ai/mirricode/pull/138) [`5b8d4e5`](https://github.com/mirri-ai/mirricode/commit/5b8d4e5dfd11f7973a3571d007cb9d59fe045df0) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Add a session-independent tools catalog with human-readable descriptions for configuration UIs, and expose the cron scheduling tools to agents so models can schedule future prompts in a session. Open the web Settings panel to browse the tool catalog.

### Patch Changes

- [#138](https://github.com/mirri-ai/mirricode/pull/138) [`5b8d4e5`](https://github.com/mirri-ai/mirricode/commit/5b8d4e5dfd11f7973a3571d007cb9d59fe045df0) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Fix archiving a session making the sidebar's Load more count overshoot: workspace session totals now count only active (non-archived) sessions, and pagination recovers when the paged cursor session was archived.

- Updated dependencies [[`4bc9a36`](https://github.com/mirri-ai/mirricode/commit/4bc9a36778640baad7e2f805311c6dc4ec1f6427), [`c299f3d`](https://github.com/mirri-ai/mirricode/commit/c299f3d4713e12ff1f08a6688ce1bcc366b76476), [`5b8d4e5`](https://github.com/mirri-ai/mirricode/commit/5b8d4e5dfd11f7973a3571d007cb9d59fe045df0), [`7d1807f`](https://github.com/mirri-ai/mirricode/commit/7d1807fcd9b2c891ef43bda955e1b2fecaf296d6), [`c299f3d`](https://github.com/mirri-ai/mirricode/commit/c299f3d4713e12ff1f08a6688ce1bcc366b76476), [`5b8d4e5`](https://github.com/mirri-ai/mirricode/commit/5b8d4e5dfd11f7973a3571d007cb9d59fe045df0)]:
  - @mirri-ai/agent-core-v2@0.4.0
  - @mirri-ai/transcript@0.1.0
  - @mirri-ai/v2-oauth@0.4.0
  - @mirri-ai/minidb@0.3.0

## 0.2.0

### Minor Changes

- [#2382](https://github.com/MoonshotAI/mirri-code/pull/2382) [`40172c7`](https://github.com/MoonshotAI/mirri-code/commit/40172c7ca96ca981b043b793588dd32e898979fa) Thanks [@liruifengv](https://github.com/liruifengv)! - Require a `hostIdentity` when starting the server and derive the default outbound identity headers (User-Agent + `X-Msh-*`) from it, replacing the hardcoded CLI fallback. The `version` option is renamed to `serverVersion`, and session export manifests now record the host product version — plus an optional desktop version for desktop exports — instead of the engine version.

### Patch Changes

- Updated dependencies [[`40172c7`](https://github.com/MoonshotAI/mirri-code/commit/40172c7ca96ca981b043b793588dd32e898979fa), [`40172c7`](https://github.com/MoonshotAI/mirri-code/commit/40172c7ca96ca981b043b793588dd32e898979fa)]:
  - @mirri-ai/v2-oauth@0.3.0
  - @mirri-ai/agent-core-v2@0.3.0

## 0.1.0

### Minor Changes

- [#1735](https://github.com/MoonshotAI/mirri-code/pull/1735) [`ce0e3ce`](https://github.com/MoonshotAI/mirri-code/commit/ce0e3ceb04223bdaad8e8931bad46eff561055b6) Thanks [@7Sageer](https://github.com/7Sageer)! - Support custom agents defined as Markdown files with frontmatter, usable as the main agent or a sub-agent (v2 engine only).

- [#1735](https://github.com/MoonshotAI/mirri-code/pull/1735) [`ce0e3ce`](https://github.com/MoonshotAI/mirri-code/commit/ce0e3ceb04223bdaad8e8931bad46eff561055b6) Thanks [@7Sageer](https://github.com/7Sageer)! - Add global tool gating to constrain which tools agents may use, with a per-session override (v2 engine only).

### Patch Changes

- [#2030](https://github.com/MoonshotAI/mirri-code/pull/2030) [`ec88d35`](https://github.com/MoonshotAI/mirri-code/commit/ec88d352e8f4dc5e8ffd1212f016138458f69893) Thanks [@RealKai42](https://github.com/RealKai42)! - Fix catalog-imported Claude models being wrongly locked into always-on thinking, and stop offering a misleading thinking Off option for models that cannot truly disable reasoning (such as Gemini 3). Also normalizes configured thinking effort values and unifies context-usage reporting.

- [#2005](https://github.com/MoonshotAI/mirri-code/pull/2005) [`a3699dd`](https://github.com/MoonshotAI/mirri-code/commit/a3699dd6aa7b41efd3129a117007d195282379fd) Thanks [@7Sageer](https://github.com/7Sageer)! - Add an `active` flag to each tool in the server's tool listing API.

- Updated dependencies [[`ce0e3ce`](https://github.com/MoonshotAI/mirri-code/commit/ce0e3ceb04223bdaad8e8931bad46eff561055b6), [`ec88d35`](https://github.com/MoonshotAI/mirri-code/commit/ec88d352e8f4dc5e8ffd1212f016138458f69893), [`37eda4e`](https://github.com/MoonshotAI/mirri-code/commit/37eda4e59aebc8ecafa91be3f43f971ed63963a3), [`37eda4e`](https://github.com/MoonshotAI/mirri-code/commit/37eda4e59aebc8ecafa91be3f43f971ed63963a3), [`ce0e3ce`](https://github.com/MoonshotAI/mirri-code/commit/ce0e3ceb04223bdaad8e8931bad46eff561055b6), [`71bcfba`](https://github.com/MoonshotAI/mirri-code/commit/71bcfba54a6836f4b6d4e26babde67576b293a64), [`ce0e3ce`](https://github.com/MoonshotAI/mirri-code/commit/ce0e3ceb04223bdaad8e8931bad46eff561055b6), [`ce0e3ce`](https://github.com/MoonshotAI/mirri-code/commit/ce0e3ceb04223bdaad8e8931bad46eff561055b6), [`b5efba7`](https://github.com/MoonshotAI/mirri-code/commit/b5efba7abcaf4041f81ec520097a61e6546e8c50), [`ce0e3ce`](https://github.com/MoonshotAI/mirri-code/commit/ce0e3ceb04223bdaad8e8931bad46eff561055b6)]:
  - @mirri-ai/agent-core-v2@0.2.0

## 0.0.2

### Patch Changes

- [#1888](https://github.com/MoonshotAI/mirri-code/pull/1888) [`5ae60fa`](https://github.com/MoonshotAI/mirri-code/commit/5ae60fa6736b63b80bd764ef01d6c0334eb80595) Thanks [@sailist](https://github.com/sailist)! - Add a unified, agent-granular transcript rendering data layer and serve it from the v2 server: clients can fetch turn-paginated transcripts via `GET /sessions/{id}/transcript` and subscribe to per-agent transcript updates over the v1 WebSocket with per-connection granularity control (off / turn / block / delta). All transcript wire types are owned by the transcript package itself. `turn.started` now carries the turn's prompt text so live transcripts render the user input as soon as the turn opens.

- Updated dependencies [[`5ae60fa`](https://github.com/MoonshotAI/mirri-code/commit/5ae60fa6736b63b80bd764ef01d6c0334eb80595)]:
  - @mirri-ai/transcript@0.0.1
  - @mirri-ai/agent-core-v2@0.1.2

## 0.0.1

### Patch Changes

- [#1441](https://github.com/MoonshotAI/mirri-code/pull/1441) [`ceb158d`](https://github.com/MoonshotAI/mirri-code/commit/ceb158dc54586f254819edbc83c27e21dca1ecf6) Thanks [@sailist](https://github.com/sailist)! - Fix the v2 AskUserQuestion flow: answers now come back keyed by question text with option labels as values, aborting a turn or stopping a background question dismisses the pending question instead of leaking it, and duplicate question texts or option labels are rejected before the question is shown. The pending-question wire shape no longer carries a synthetic expires_at field.

- [#1638](https://github.com/MoonshotAI/mirri-code/pull/1638) [`7c889f3`](https://github.com/MoonshotAI/mirri-code/commit/7c889f3a960482cc9382203bda55d972b6fb6acd) Thanks [@RealKai42](https://github.com/RealKai42)! - In auto permission mode, plan exits are now marked as auto-approved (not user-reviewed) in both the tool result and the transcript, so the agent no longer treats automatic plan approval as a user signal to start executing.

- [#1441](https://github.com/MoonshotAI/mirri-code/pull/1441) [`ceb158d`](https://github.com/MoonshotAI/mirri-code/commit/ceb158dc54586f254819edbc83c27e21dca1ecf6) Thanks [@sailist](https://github.com/sailist)! - Reorganize the agent execution environment into separate filesystem, process and tool domains.

- [#1636](https://github.com/MoonshotAI/mirri-code/pull/1636) [`8027fe2`](https://github.com/MoonshotAI/mirri-code/commit/8027fe291b03fbfce6dc60aa06f8699ad0976ec5) Thanks [@sailist](https://github.com/sailist)! - Make file tools able to reach skill directories outside the working directory in the v2 engine (experimental), and honor --skillsDir in v2 print mode and the server's skillDirs option.

- [#1601](https://github.com/MoonshotAI/mirri-code/pull/1601) [`dc309a7`](https://github.com/MoonshotAI/mirri-code/commit/dc309a7dfb38b6ef885b8ae80be51b49f8486207) Thanks [@kermanx](https://github.com/kermanx)! - Report the live (measured + estimated) context size in the v2 server's v1-compatible status stream instead of the measured-only count, which read 0 until the first model response of a session completed and could dip mid-turn while the context was being rewritten.

- [#1617](https://github.com/MoonshotAI/mirri-code/pull/1617) [`4ec2e7f`](https://github.com/MoonshotAI/mirri-code/commit/4ec2e7fab14ab89cddf77821082c3ff4911f737b) Thanks [@sailist](https://github.com/sailist)! - Run the local server (`kimi server run` / `kimi web`) on the agent-core-v2 engine by default — the `MIRRICODE_EXPERIMENTAL_FLAG` opt-in is no longer needed, and the legacy v1 server package has been removed.

- [#1441](https://github.com/MoonshotAI/mirri-code/pull/1441) [`ceb158d`](https://github.com/MoonshotAI/mirri-code/commit/ceb158dc54586f254819edbc83c27e21dca1ecf6) Thanks [@sailist](https://github.com/sailist)! - Reroute the blob store backend from the host filesystem to the pluggable storage layer, so server-only deployments no longer require a local filesystem implementation.

- [#1441](https://github.com/MoonshotAI/mirri-code/pull/1441) [`ceb158d`](https://github.com/MoonshotAI/mirri-code/commit/ceb158dc54586f254819edbc83c27e21dca1ecf6) Thanks [@sailist](https://github.com/sailist)! - Fix the managed OAuth device-code login getting aborted when an unrelated provider refresh fires during the login flow.

- Updated dependencies [[`ceb158d`](https://github.com/MoonshotAI/mirri-code/commit/ceb158dc54586f254819edbc83c27e21dca1ecf6), [`ceb158d`](https://github.com/MoonshotAI/mirri-code/commit/ceb158dc54586f254819edbc83c27e21dca1ecf6), [`7c889f3`](https://github.com/MoonshotAI/mirri-code/commit/7c889f3a960482cc9382203bda55d972b6fb6acd), [`ceb158d`](https://github.com/MoonshotAI/mirri-code/commit/ceb158dc54586f254819edbc83c27e21dca1ecf6), [`ceb158d`](https://github.com/MoonshotAI/mirri-code/commit/ceb158dc54586f254819edbc83c27e21dca1ecf6), [`0527ca2`](https://github.com/MoonshotAI/mirri-code/commit/0527ca2267f8cf355d0c158953f3dbfc0c9692ac), [`1294a0e`](https://github.com/MoonshotAI/mirri-code/commit/1294a0e1ad739151573163505f9c58afb2d543e4), [`a4aae87`](https://github.com/MoonshotAI/mirri-code/commit/a4aae87cd9a240d3567601ed1a9aefaab540b075), [`0303b82`](https://github.com/MoonshotAI/mirri-code/commit/0303b82c3e691836163ecf906febfb6324c81d74), [`0527ca2`](https://github.com/MoonshotAI/mirri-code/commit/0527ca2267f8cf355d0c158953f3dbfc0c9692ac), [`8027fe2`](https://github.com/MoonshotAI/mirri-code/commit/8027fe291b03fbfce6dc60aa06f8699ad0976ec5), [`8027fe2`](https://github.com/MoonshotAI/mirri-code/commit/8027fe291b03fbfce6dc60aa06f8699ad0976ec5), [`ceb158d`](https://github.com/MoonshotAI/mirri-code/commit/ceb158dc54586f254819edbc83c27e21dca1ecf6), [`0e0a6e9`](https://github.com/MoonshotAI/mirri-code/commit/0e0a6e9a5170c28c5e6809c1b2cf6d6f8904de73), [`ceb158d`](https://github.com/MoonshotAI/mirri-code/commit/ceb158dc54586f254819edbc83c27e21dca1ecf6), [`96b8328`](https://github.com/MoonshotAI/mirri-code/commit/96b83281b2da3ee479b59e8a8da990708d1d6a30), [`ceb158d`](https://github.com/MoonshotAI/mirri-code/commit/ceb158dc54586f254819edbc83c27e21dca1ecf6), [`ceb158d`](https://github.com/MoonshotAI/mirri-code/commit/ceb158dc54586f254819edbc83c27e21dca1ecf6), [`5d1f904`](https://github.com/MoonshotAI/mirri-code/commit/5d1f9049cab84c0f40524a2382b085dfa976c866), [`003e583`](https://github.com/MoonshotAI/mirri-code/commit/003e583d865d40ae7dbeb0f1e6b3974a63781950), [`ceb158d`](https://github.com/MoonshotAI/mirri-code/commit/ceb158dc54586f254819edbc83c27e21dca1ecf6), [`ceb158d`](https://github.com/MoonshotAI/mirri-code/commit/ceb158dc54586f254819edbc83c27e21dca1ecf6), [`ceb158d`](https://github.com/MoonshotAI/mirri-code/commit/ceb158dc54586f254819edbc83c27e21dca1ecf6), [`ceb158d`](https://github.com/MoonshotAI/mirri-code/commit/ceb158dc54586f254819edbc83c27e21dca1ecf6), [`09c1c32`](https://github.com/MoonshotAI/mirri-code/commit/09c1c3296059255a5074fa5d4dbb22fef14cdef9), [`ceb158d`](https://github.com/MoonshotAI/mirri-code/commit/ceb158dc54586f254819edbc83c27e21dca1ecf6), [`ceb158d`](https://github.com/MoonshotAI/mirri-code/commit/ceb158dc54586f254819edbc83c27e21dca1ecf6)]:
  - @mirri-ai/agent-core-v2@0.1.0
  - @mirri-ai/protocol@0.4.0
