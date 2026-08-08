# @mirri-ai/minidb

## 0.3.0

### Minor Changes

- [#134](https://github.com/mirri-ai/mirricode/pull/134) [`4bc9a36`](https://github.com/mirri-ai/mirricode/commit/4bc9a36778640baad7e2f805311c6dc4ec1f6427) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Introduce the agent-core-v2 engine as a parallel backend that coexists with the v1 engine, plus a wire protocol bump to 1.5.

  - Add the v2 engine packages (agent-core-v2, kap-server, transcript, klient, v2-oauth, minidb, tree-sitter-bash) that port the 20 mirri features and close the 4 v2 gaps (G1-G4) with golden-test parity to v1.
  - Bump the wire protocol from 1.4 to 1.5 and register the v1.4→v1.5 wall-clock anchor migration so existing records backfill the missing anchor from create and resume timestamps.
  - web: Add a "start from" selector in the agent profiles panel so users can create a custom profile based on a builtin profile without the extends chain.

### Patch Changes

- [#137](https://github.com/mirri-ai/mirricode/pull/137) [`7d1807f`](https://github.com/mirri-ai/mirricode/commit/7d1807fcd9b2c891ef43bda955e1b2fecaf296d6) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Make the embedded key-value server handle large RESP requests without quadratic re-buffering: oversized requests are rejected as soon as their declared length is read, and allowed large payloads stream in with linear copy cost.

## 0.2.0

### Minor Changes

- [#1816](https://github.com/MoonshotAI/mirri-code/pull/1816) [`44f3341`](https://github.com/MoonshotAI/mirri-code/commit/44f334191989183d21920f6867c405581347c748) Thanks [@sailist](https://github.com/sailist)! - Cluster readers of the embedded key-value engine now catch up incrementally by replaying only newly appended WAL frames after another process writes, instead of fully reopening the shard on every read; cross-process read latency drops by orders of magnitude at larger shard sizes, and readers still fall back to a full reopen after WAL rotation or truncation.

### Patch Changes

- [#1816](https://github.com/MoonshotAI/mirri-code/pull/1816) [`44f3341`](https://github.com/MoonshotAI/mirri-code/commit/44f334191989183d21920f6867c405581347c748) Thanks [@sailist](https://github.com/sailist)! - Harden the embedded key-value engine's durability: WAL compaction now always terminates under sustained write storms instead of chasing the tail forever, a committed write can no longer slip through a compaction rotation undetected, torn WAL tails no longer misplace later disk-mode value pointers, read-only opens never create or modify database files or compact under a live writer, corrupt index-definition files no longer force a full rebuild, stale compaction temp files are cleaned on open, and the process lock can no longer be taken over by several processes at once.

- [#1816](https://github.com/MoonshotAI/mirri-code/pull/1816) [`44f3341`](https://github.com/MoonshotAI/mirri-code/commit/44f334191989183d21920f6867c405581347c748) Thanks [@sailist](https://github.com/sailist)! - Speed up the embedded key-value engine under stress: queries with skip/limit now stream candidates instead of decoding every match first, LRU eviction picks victims in O(1) instead of scanning every key, bursts of simultaneously expired TTL keys are drained within seconds, existence checks and size counting no longer read values when they only need metadata, and one oversized token can no longer poison the full-text index.

- [#1816](https://github.com/MoonshotAI/mirri-code/pull/1816) [`44f3341`](https://github.com/MoonshotAI/mirri-code/commit/44f334191989183d21920f6867c405581347c748) Thanks [@sailist](https://github.com/sailist)! - Keep the embedded key-value engine writable when a WAL compaction rotation fails mid-way instead of wedging it until reopen, stop a rolled-back write from erasing a concurrently committed value for the same key, let the RESP server survive aborted connections, recover after oversized requests, and answer each pipelined command independently, and keep the previous full-text index intact when a postings rebuild fails.
