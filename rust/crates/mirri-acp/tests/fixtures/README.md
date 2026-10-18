# Vendored ACP schema snapshot

`official-schema.json` is a byte-for-byte copy of the official Agent Client
Protocol JSON schema shipped by the `@agentclientprotocol/sdk` npm package:

- Source: `packages/acp-server/node_modules/@agentclientprotocol/sdk/schema/schema.json`
- SDK version: `@agentclientprotocol/sdk@1.3.0`
- Copied: 2026-09-05 (P5a)

The `mirri-acp` conformance tests compile this snapshot (wrapping the
`$defs` in a `{"$ref": "#/$defs/SessionNotification"}` root — the schema's
own root is the three-way Agent/Client/ProtocolLevel `anyOf`, which cannot
validate a bare notification params object) and validate every emitted
`session/update` notification against it, so the Rust binary's wire behavior
is checked against the official contract rather than a self-declared one.

When upgrading `@agentclientprotocol/sdk`, re-copy the schema here
(`cp packages/acp-server/node_modules/@agentclientprotocol/sdk/schema/schema.json
rust/crates/mirri-acp/tests/fixtures/official-schema.json`) and update the
version + date above.
