# @mirri-ai/tree-sitter-bash

## 0.2.0

### Minor Changes

- [#134](https://github.com/mirri-ai/mirricode/pull/134) [`4bc9a36`](https://github.com/mirri-ai/mirricode/commit/4bc9a36778640baad7e2f805311c6dc4ec1f6427) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Introduce the agent-core-v2 engine as a parallel backend that coexists with the v1 engine, plus a wire protocol bump to 1.5.

  - Add the v2 engine packages (agent-core-v2, kap-server, transcript, klient, v2-oauth, minidb, tree-sitter-bash) that port the 20 mirri features and close the 4 v2 gaps (G1-G4) with golden-test parity to v1.
  - Bump the wire protocol from 1.4 to 1.5 and register the v1.4→v1.5 wall-clock anchor migration so existing records backfill the missing anchor from create and resume timestamps.
  - web: Add a "start from" selector in the agent profiles panel so users can create a custom profile based on a builtin profile without the extends chain.
