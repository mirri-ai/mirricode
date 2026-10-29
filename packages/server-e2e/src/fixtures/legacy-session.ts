/**
 * Legacy TS-engine session fixture factory (journey stage ⑥, spec R11).
 *
 * Produces the minimal legacy layout `mirri migrate` consumes, mirroring
 * the fixture shapes proven by the rust unit tests
 * (`rust/crates/mirri-cli/src/migrate.rs`, `make_ts_session`):
 *
 *   <homeDir>/sessions/<bucket>/session_<uuid>/
 *     state.json            — TS camelCase schema (alias-compatible with
 *                             the rust `PersistedSessionMetadata` reader:
 *                             `createdAt`/`updatedAt` epoch-ms ints,
 *                             `title`, `workDir`)
 *     agents/main/wire.jsonl — TS wire log: a `metadata` first record
 *                             (protocol_version 1.5 — the lazy 1.6→2.0
 *                             chain keeps it readable) + complete turn
 *                             records (`context.append_message` user +
 *                             assistant)
 *
 * The bucket name under the legacy root is arbitrary from the migrate
 * path's perspective — the target bucket is re-derived from the
 * state.json `workDir` (`encode_work_dir_key`), the source bucket only
 * matters when no workDir is recorded. We use a TS-era-looking bucket
 * for fidelity.
 */
import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';

export interface LegacySessionFixture {
  /** The session id (directory name, `session_<uuid>`). */
  sessionId: string;
  /** The legacy session directory (under `<homeDir>/sessions/...`). */
  sessionDir: string;
  /** The workDir recorded in state.json (decides the v3 target bucket). */
  workDir: string;
  /** The title recorded in state.json. */
  title: string;
}

export interface LegacySessionFixtureOptions {
  /** Required: the workspace the migrated session will be routed to. */
  workDir: string;
  /** Default: 'legacy ts session'. */
  title?: string;
  /** Default: fresh `session_<uuid>`. */
  sessionId?: string;
}

/**
 * Seed one legacy TS-layout session under `<homeDir>/sessions/`.
 *
 * The wire log carries one complete turn — a user message asking about
 * the project and an assistant reply containing the marker text the
 * scenario asserts against after migration (the user-perceivable proof
 * the migrated history is genuinely readable).
 */
export function createLegacySession(
  homeDir: string,
  options: LegacySessionFixtureOptions,
): LegacySessionFixture {
  const sessionId = options.sessionId ?? `session_${randomUUID()}`;
  const title = options.title ?? 'legacy ts session';
  const workDir = options.workDir;
  // A TS-era bucket spelling — migrate re-buckets by workDir anyway.
  const bucket = 'wd_old_000000000001';
  const sessionDir = join(homeDir, 'sessions', bucket, sessionId);

  mkdirSync(join(sessionDir, 'agents', 'main'), { recursive: true });

  const now = Date.now();
  const state = {
    createdAt: now - 86_400_000, // a day-old session
    updatedAt: now - 43_200_000,
    title,
    workDir,
  };
  writeFileSync(join(sessionDir, 'state.json'), JSON.stringify(state), 'utf8');

  // One complete turn: metadata header + user + assistant messages
  // (`context.append_message`, the TS wire op spelling — both `type`
  // and `op` discriminators are accepted, we use the TS `type` form).
  const wireLines = [
    JSON.stringify({ type: 'metadata', protocol_version: '1.5', created_at: now - 86_400_000 }),
    JSON.stringify({
      type: 'context.append_message',
      message: { role: 'user', content: 'What does the legacy project contain?' },
    }),
    JSON.stringify({
      type: 'context.append_message',
      message: { role: 'assistant', content: 'LEGACY-REPLY-MARKER The legacy project contains a utils module.' },
    }),
  ];
  writeFileSync(
    join(sessionDir, 'agents', 'main', 'wire.jsonl'),
    wireLines.join('\n') + '\n',
    'utf8',
  );

  return { sessionId, sessionDir, workDir, title };
}
