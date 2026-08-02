# B4-COEX: v1/v2 Coexistence Verification & Concurrent-Session Probe Report

> **Date**: 2026-08-01
> **Author**: B4-COEX acceptance test run
> **Related**: `design.md` §4.3, §7, §13 Q2 (decision (c): defer lock)
> **Test files**: `packages/agent-core-v2/test/coexistence/`

## 1. Coexistence Verification Results

### 1.1 Shared `workspaces.json` — ✅ PASS

Both v1 and v2 read/write the same `<homeDir>/workspaces.json` format without data loss.

| Scenario | Result |
|----------|--------|
| v1 writes → v2 reads | ✅ All workspaces, names, roots, tombstones preserved |
| v2 writes → v1 reads | ✅ All fields preserved; `version` field is 1 on both sides |
| Tombstone round-trip (v1→v2→v1) | ✅ `deleted_workspace_ids` survive across engines |
| ISO date string round-trip (v2→v1) | ✅ Epoch ms → ISO 8601 → epoch ms is lossless |
| Missing file tolerance | ✅ Both return empty catalog |

**Format agreement**: Both engines use `{ version: 1, workspaces: Record<string, { root, name, created_at, last_opened_at }>, deleted_workspace_ids: string[] }`. v2's `FileWorkspacePersistence` converts between epoch ms (in-memory) and ISO strings (on-disk) transparently, matching v1's on-disk format exactly.

**Concurrency**: v1 uses atomic tmp+rename; v2's `IAtomicDocumentStore` → `FileStorageService.write()` also uses atomic write. Lost-update window is one read-modify-write cycle — v2's `WorkspaceService` explicitly does fresh read-modify-write (no in-memory cache) with a promise-chain mutex for in-process serialization.

**Test file**: `test/coexistence/shared-workspaces-json.test.ts` (6 tests)

### 1.2 Wire Cross-Readability — ✅ CONFIRMED (no duplication needed)

B0-L18 already verified wire-record bidirectional compatibility via 25+ cross-resume tests in `test/wire/v1-v2-cross-resume.test.ts`. Both engines are at protocol version 1.5:

- **v1**: `AGENT_WIRE_PROTOCOL_VERSION = '1.5'` (bumped via B0-L18a from 1.4)
- **v2**: `WIRE_PROTOCOL_VERSION = '1.5'`

The B0-L18 test suite covers: goal.create/update/complete, planMode.enter, swarm.enter, permissionMode.set, cron schedule — all mirri-specific record types have v2 ops that parse v1-written records without field loss. No duplication needed.

### 1.3 Backend Switch (`/meta.backend`) — ✅ PASS

| Check | Result |
|-------|--------|
| v2 route returns `backend: 'v2'` | ✅ Verified in `kap-server/src/routes/meta.ts:61` |
| v1 route omits `backend` field | ✅ Verified in `server/src/routes/meta.ts` — no `backend` in data object |
| v2 schema types `backend` as `z.enum(['v1','v2']).optional()` | ✅ Verified in `kap-server/src/protocol/rest-meta.ts:42` |
| Both routes share server_version, capabilities, server_id, started_at | ✅ Shape compatible |

The web UI switcher pill reads `GET /api/v1/meta` → checks `backend` field → treats absence as v1, `'v2'` as v2. This contract is verified structurally.

**Test file**: `test/coexistence/backend-switch.test.ts` (4 tests)

---

## 2. Concurrent-Session Probe Findings

### 2.1 Code Path Analysis

**v1 resume path** (`packages/agent-core/src/rpc/core-impl.ts:445-536`):
- Checks `this.sessions.get(summary.id)` — an in-process `Map<string, Session>`.
- If not active, creates new `Session`, calls `session.resume()`, adds to map.
- Wire writes go through `FileSystemAgentRecordPersistence.flush()`: opens `wire.jsonl` with `open(filePath, 'a')`, writes all pending records as one batch, calls `fh.sync()`.
- No PID file, no `flock()`, no `state.json` lock field.

**v2 resume path** (`packages/agent-core-v2/src/workspace/sessionLifecycle/sessionLifecycleService.ts:318-358`):
- Checks `this.sessions.get(sessionId)` and `this.resuming` — both in-process Maps.
- If not active, creates new Session scope via DI, calls `materializeSession()`.
- Wire writes go through `AppendLogStore` → `FileStorageService.append()`: opens with `open(filePath, 'a')`, writes batch, calls `fh.sync()`.
- No PID file, no `flock()`, no `state.json` lock field.

**Confirmed**: Neither engine has any cross-process session lock. Both rely solely on in-process `Map` dedup. Two server processes (v1 on 58627, v2 on 56129) have completely independent session maps — resuming the same session in both creates two independent writer contexts.

### 2.2 Failure Mode: Record Sequence Jumbling

The probe test (`test/coexistence/concurrent-session-probe.test.ts`) demonstrated:

1. **Individual JSONL lines are NOT corrupted** — POSIX `O_APPEND` guarantees atomic writes up to `PIPE_BUF` (≥512 bytes). Individual tool-call/result records are typically under this limit, so lines are never split mid-JSON.

2. **Record SEQUENCE is jumbled** — Both engines batch-flush their pending records. When two writers flush concurrently, the batch order is nondeterministic. A wire.jsonl intended to contain:

   ```
   goal.create(v1), tool.call(bash,v1), tool.result(v1)
   goal.create(v2), tool.call(read,v2), tool.result(v2)
   ```

   May actually contain:

   ```
   goal.create(v1), goal.create(v2), tool.call(read,v2), tool.result(v2), tool.call(bash,v1), tool.result(v1)
   ```

3. **Model-state corruption on replay** — v2's Model/Op/Reducer pattern applies records in wire.jsonl order. When v2 replays an interleaved journal, `goal.create(v2)` may overwrite `goal.create(v1)` in the model state, then `tool.call(bash,v1)` references `goal-v1` which is no longer the active goal. The reducer either:
   - Rejects the stale reference (causing a resume error), or
   - Accepts it and produces inconsistent model state (goal metrics attributed to wrong goal, permission mode in wrong state, etc.)

4. **The 50-record stress test confirmed**: All 100 records (50 per engine) survive and parse correctly — no data loss, no mid-line corruption. The failure is *semantic* (wrong order), not *syntactic* (corrupted lines).

### 2.3 Severity Assessment

| Aspect | Assessment |
|--------|-----------|
| **Likelihood in normal use** | Low — requires user to open the SAME session in TUI(v1) AND web(v2) simultaneously and run turns in both |
| **Impact when it occurs** | High — session model state becomes inconsistent on next resume; may require manual wire.jsonl editing or session deletion |
| **Recovery** | Possible but painful: manually sort wire.jsonl by timestamp, or archive the session |
| **Detection** | Neither engine detects interleaving; resume silently applies jumbled records |

### 2.4 Recommended Lock Design (if post-B4 gate approves)

If the post-B4 decision gate judges that a cross-engine lock is justified by the risk, the recommended design is:

**File-based advisory lock** (`<homeDir>/sessions/<sessionId>/.lock`):

1. **Acquisition**: On `resumeSession`/`resume()`, attempt to create a `.lock` file containing `{ pid, engine, acquiredAt }`. Use `O_EXCL | O_CREAT` (atomic create-if-not-exists).
2. **Stale detection**: If `.lock` exists, check if the PID is still running. If not, remove stale lock and retry.
3. **Release**: On session `close()`, delete the `.lock` file.
4. **v1 patch required**: This is the "controlled exception to v1 frozen" — a small patch to `MirriCore.resumeSessionWithOverrides()` and `MirriCore.closeSession()` that acquires/releases the lock file. The patch is narrow (2 call sites) and doesn't change business logic.
5. **v2 side**: Same logic in `SessionLifecycleService.doResume()` and `SessionLifecycleService.close()`.
6. **Timeout**: Optional acquire-timeout with a configurable grace period.

**Why not in-process lock**: Two separate Node.js processes cannot share an in-process Map. A filesystem-based lock is the minimum viable coordination mechanism.

**Why not `flock()`**: `flock()` works but is not available on Windows. `O_EXCL | O_CREAT` is cross-platform.

---

## 3. Summary

| Coexistence Invariant | Status | Evidence |
|----------------------|--------|----------|
| Shared `workspaces.json` format | ✅ Verified | 6 tests, bidirectional read/write, tombstone round-trip |
| Wire cross-readability (both at 1.5) | ✅ Confirmed | B0-L18: 25+ cross-resume tests; both at protocol 1.5 |
| `/meta.backend` discriminator | ✅ Verified | 4 tests; v2 returns `'v2'`, v1 omits, schema optional |
| Concurrent session safety | ⚠️ Risk documented | 5 probe tests; no data loss but record sequence jumbling → model-state corruption |

**Overall**: The three coexistence invariants (§4.3) are verified. The concurrent-session risk is real but contained — it requires deliberate simultaneous use of the same session across two backends, which the developer-discipline notice already warns against. The §13 Q2 decision to defer the lock is affirmed by this probe: the failure mode is well-understood, the impact is high but the likelihood is low, and a lock design is documented above for the post-B4 gate.

---

## 4. Test Inventory

| File | Tests | What it covers |
|------|-------|----------------|
| `test/coexistence/shared-workspaces-json.test.ts` | 6 | v1↔v2 workspaces.json read/write, tombstone round-trip, version agreement, date round-trip |
| `test/coexistence/backend-switch.test.ts` | 4 | `/meta.backend` field presence/absence, schema typing, shape compatibility |
| `test/coexistence/concurrent-session-probe.test.ts` | 5 | Record interleaving, JSONL line atomicity, v1-v2 cross-read, model-state corruption scenario, in-process-only lock verification |

**Total**: 15 tests, all passing.
