# Cross-engine agent profile fixture corpus

This directory is the **single shared source** for the R2-4 cross-engine
consistency acceptance: the same `.md` agent profile samples are parsed by
BOTH engines' test suites, and both must agree field-by-field.

- **TS side:** `packages/agent-profile/test/cross-engine-fixture.test.ts`
  parses every `corpus/*.md` file with `parseAgentFileText` and compares a
  normalized form against `expected-outputs.json`.
- **Rust side:** `rust/crates/mirri-core/tests/agent_md_cross_engine_tests.rs`
  reads **this same directory** (located via `CARGO_MANIFEST_DIR` + a relative
  path) and parses each file with `parse_agent_file_text`, producing the same
  normalized form and comparing against the same `expected-outputs.json`.

Both suites consume the corpus from this location only — **never copy it**
into the rust tree or anywhere else.

## Corpus scope

The corpus pins the **common accepted surface** of the adjudicated YAML
subset (R2-1 ruling: hand-written rust parser, no YAML crate). It deliberately
avoids constructs where the engines legitimately differ:

- no nested mappings, block scalars (`|`/`>`), anchors, tags, folded plain
  scalars or non-empty flow sequences (rust hard-errors, js-yaml accepts);
- no `[ ]` (empty flow list with inner whitespace — rust hard-errors);
- no pathological number forms like `.inf`, `0o17` or ISO dates (js-yaml is
  stricter there, the opposite direction);
- parse-only comparison — the engines' *serializers* quote differently by
  design (rust is conservative), so round-trip byte equality is not pinned.

`tools: # note` followed by a block list **is** covered: both engines bind
the list to the key (`trailing-comment-list.md`; the rust parser aligned in
R3-7 — a comment-only value line behaves exactly like an empty one).

`composed-body.md`, `bare-reference-body.md` and `full-replacement-body.md`
(R6) pin the three prompt-body **forms** the write channels persist —
composed (`${base_prompt}` + role text), the bare reference, and a
full replacement with no reference. At the parse level they are plain
text, asserted verbatim: the format layer has no composition awareness
(ADR-010 ① — the composed-vs-replacement discrimination lives in the
engines' consumers, not the parser).

`corpus/crlf.md` must keep its literal CRLF bytes through checkouts; it is
exempted from the repo-wide LF normalization in `.gitattributes` (`-text`).


Each fixture is parsed with the synthetic path `agents/<file-name>`, which
both suites construct identically (the path feeds name-from-filename
derivation and `filePath`/`file_path` output).

## Normalization convention

Per fixture, the parse result is normalized to:

- `{ ok: true, def: { … } }` on success — the eleven `AgentFileDef` payload
  fields (`name`, `description`, `whenToUse`, `defaultModel`, `tools`,
  `disallowedTools`, `subagents`, `capabilitiesRequired`, `enabled`, `prompt`,
  `filePath`), keys in alphabetical order, and **absent/undefined/None
  optional fields emitted as explicit `null`** (never omitted);
- `{ ok: false, error: "<message>" }` on failure — the full
  `AgentFileParseError` message (which embeds the synthetic path).

`source` is not part of the normalized output (both suites parse without a
discovery source). The whole expectation is stored in
`expected-outputs.json` keyed by fixture file name.

## Updating expectations

When a fixture change legitimately alters parser output, regenerate the
expectations from the TS side (the reference implementation) and re-run the
rust suite — it must still pass without edits:

```
cd packages/agent-profile && UPDATE_CROSS_ENGINE_SNAPSHOT=1 npx vitest run
cd ../../rust && cargo test -p mirri-core --test agent_md_cross_engine_tests
```

The snapshot file is generated with stable formatting (2-space JSON,
alphabetical keys, trailing newline); both sides compare parsed JSON values,
not bytes.
