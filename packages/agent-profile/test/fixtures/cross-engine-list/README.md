# Cross-engine agent profile LIST fixture corpus

This directory is the **single shared source** for the R4-6 cross-engine
list-projection acceptance (US-C1): the same `.md` agent profile samples are
mounted by BOTH engines' test suites as discovery roots, and both must agree
on the normalized `GET /api/v1/agents` list projection — the profile name
set, each row's `source` / `enabled` / `essential` / `builtin` /
`has_override` / `suppressed_candidates`, and the `skipped` channel.

This is the **list-level** companion of the parse-level corpus in
[`../cross-engine/`](../cross-engine/): that one pins the `.md` parser
field-by-field; this one pins the discovery/merge/projection semantics on
top of it.

- **TS side:** `packages/kap-server/test/agents-cross-engine-list.test.ts`
  boots the in-process server with an isolated home, mounts the corpus
  roots (see below), reads `GET /api/v1/agents` through the real route
  (workspace-registered, so the catalog projection path runs), normalizes
  the response and compares against `expected-list.json`.
- **Rust side:** `rust/crates/mirri-core/tests/agent_list_cross_engine_tests.rs`
  reads **this same directory** (located via `CARGO_MANIFEST_DIR` + a
  relative path), discovers the same roots with the same source tags,
  registers the winners into a `ProfileManager` and produces the same
  normalized projection, comparing against the same `expected-list.json`.

Both suites consume the corpus from this location only — **never copy it**
into the rust tree or anywhere else.

## Root mounting layout

The corpus is organized by mount point, not by sample:

| Directory | TS mount | Rust mount (`ProfileSourceRoot`) |
|---|---|---|
| `user-agents/` | `<home>/agents` — the user agents root (`userAgentRoots` brand root) | second root, tag `User` |
| `project-agents/` | `<workspace-root>/.mirri-code/agents` — the project (workspace) root | first root, tag `Project` |
| `legacy-profiles/` | **not mounted** (rust-only; see exceptions) | third root, tag `User` |

- The TS **user root** and the rust **home agents root** are the same
  concept: the per-user brand directory (`<mirri-home>/agents`). The rust
  suite mounts the fixture directory directly (read-only); the TS suite
  copies it into the isolated temp home because the server also writes
  logs there.
- Root ORDER encodes merge priority: the rust suite lists the
  `Project`-tagged root first (mirroring the TS catalog's
  `workspace 30 > user 10` source priority), so a same-name project file
  wins and the user file becomes a suppressed candidate.
- The legacy root is mounted only by the rust suite (the documented
  exception below) and lands AFTER the user roots, mirroring the real rust
  boot chain (`user_profile_root_candidates` then `legacy_profile_root`).

## Corpus samples and what each pins

| Sample | What it locks |
|---|---|
| `user-agents/plain.md` | a no-conflict user row: `source: user`, enabled, no suppressed |
| `user-agents/composed-role.md` | (R6) a composed body row (`${base_prompt}` + role text): the wire `system_prompt_template` is the STRIPPED role text (ADR-010 ② — the strip applies to ALL file rows) |
| `user-agents/bare-ref-role.md` | (R6) a bare-reference body row: the wire projects NO `system_prompt_template` (strips to empty → key absent) |
| `user-agents/replacement-role.md` | (R6) a full-replacement body row: the wire `system_prompt_template` is the full body verbatim (replacement semantics) |
| `user-agents/shared.md` + `project-agents/shared.md` | multi-source name collision: project wins, the user file lands in `suppressed_candidates` as `{source: user, priority: 10, reason: priority}` |
| `user-agents/guarded.md` + `project-agents/guarded.md` (disabled) | the `disabled` suppressed reason: a disabled higher-priority file never wins; the enabled user file wins and records `{source: project, priority: 30, reason: disabled}` |
| `user-agents/sleeper.md` (`enabled: false`) | a lone disabled profile stays listed but marked (`enabled: false` on the wire) |
| `user-agents/explore.md` | user file covering a non-essential builtin: `source: user`, `builtin: false`, `has_override: true`, displaced builtin in `suppressed_candidates` as `{source: builtin, priority: 0, reason: priority}` (D16) |
| `user-agents/agent.md` | essential-name coverage (D14/D16): loads, user wins, `essential: true` + `has_override: true`, builtin displaced into suppressed — same row shape as `explore` |
| `user-agents/plan.md` + `project-agents/plan.md` | the suppressed-candidate ORDER: file losers first (`{user, 10, priority}`), the displaced builtin LAST (`{builtin, 0, priority}`) |
| `user-agents/mystery.md` | unknown frontmatter keys must not break discovery — the profile lists normally |
| `user-agents/broken.md` | a parse failure lands in the `skipped` channel (reason normalized; see below) |
| `legacy-profiles/*.toml|json` | **rust-only**: legacy formats load through the legacy root (asserted inline by the rust suite, not in `expected-list.json`) |
| `legacy-profiles/coder.toml` | **rust-only**: a broken legacy `extends` covering a builtin name — the builtin row stays clean, the file lands in `skipped` (the Task 27 review M1 regression, at integration level) |

## Normalization convention

Both suites normalize the list response to the same JSON shape, stored in
`expected-list.json`:

```json
{
  "profiles": [
    {
      "name": "…",
      "source": "…",
      "builtin": false,
      "essential": false,
      "enabled": true,
      "has_override": false,
      "suppressed_candidates": [{ "source": "…", "priority": 10, "reason": "…" }]
    }
  ],
  "skipped": [{ "path": "broken.md", "reason": "<load-failure>" }]
}
```

- **Rows are sorted by `name`** (the two engines' wire orders differ by
  design; the acceptance is on the projected set, not row order).
- `suppressed_candidates` is emitted only when non-empty (both engines omit
  the key on the wire when there are no losers).
- `suppressed_candidates[].reason` passes through for the enum spellings
  `priority` / `disabled`; any other (free-text) value is normalized to the
  `<load-failure>` placeholder. Both engines emit only the enum values
  today — the placeholder rule exists so a future free-text reason cannot
  silently diverge.
- `skipped[].path` is the **basename** of the file (the two engines mount
  the corpus at different absolute paths — the full `file_path` /
  `skipped[].path` values are mount-dependent by construction and are
  therefore not part of the acceptance).
- `skipped[].reason` is always normalized to the `<load-failure>`
  placeholder: the two engines word their parse-error messages differently
  by design (each engine's own error format), so only the *presence and
  identity of the skipped file* is asserted. The TS and rust suites each
  additionally assert locally that the underlying reason text is
  non-empty.
- Content fields (`description`, `system_prompt_template`,
  `effective_system_prompt`, …) are NOT part of this projection — the
  parse-level corpus pins them, and the R6 composed/bare-reference/
  replacement samples additionally have their `system_prompt_template`
  shapes pinned inline by both engines' suites (TS:
  `agents-cross-engine-list.test.ts`; rust: `agent_list_cross_engine_tests.rs`).

## Documented exceptions (two-sided behaviors)

1. **Legacy `.toml` / `.json` roots are invisible to the TS engine.** The
   v2 TS engine reads `.md` / `.yaml` / `.yml` agent files
   (`AGENT_FILE_EXTENSIONS`); the rust engine reads `.md` / `.toml` /
   `.json`. A `.toml`/`.json` file under a mounted root therefore loads
   only on the rust side. Handling: `legacy-profiles/` is mounted only by
   the rust suite, which asserts those rows inline (outside
   `expected-list.json`), so the shared expectation stays honest about
   what "both engines" means.
2. **`.yaml` / `.yml` files are skipped by the rust engine.** The mirror
   of (1): the rust engine has no YAML profile parser by adjudication
   (R2-1: hand-written parser, no YAML crate), and skips such files with
   an explicit reason instead. The corpus contains no `.yaml` samples;
   the divergence is documented here rather than pinned.
3. **`skipped[].reason` / parse-error wording differs by engine.** Not
   alignable: each engine formats its own error text (and the TS message
   embeds the TS-side mount path). Normalized to `<load-failure>` (see
   above).

## Updating expectations

When a corpus change legitimately alters the list projection, regenerate
the expectations from the TS side (the reference implementation) and
re-run the rust suite — it must still pass without edits:

```
cd packages/kap-server && UPDATE_CROSS_ENGINE_LIST_SNAPSHOT=1 pnpm exec vitest run test/agents-cross-engine-list.test.ts
cd ../../rust && cargo test -p mirri-core --test agent_list_cross_engine_tests
```

The snapshot file is generated with stable formatting (2-space JSON,
sorted keys where applicable, trailing newline); both sides compare parsed
JSON values, not bytes.
