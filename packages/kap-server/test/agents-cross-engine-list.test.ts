/**
 * Cross-engine agent profile LIST projection acceptance (R4-6 / US-C1).
 *
 * Mounts the shared corpus at
 * `packages/agent-profile/test/fixtures/cross-engine-list/` into an
 * isolated home (user root) and a registered workspace (project root),
 * reads `GET /api/v1/agents` through the real route — the workspace is
 * registered, so the catalog projection path (the primary read path)
 * runs — and compares a normalized list projection against the shared
 * `expected-list.json`. The SAME corpus and the SAME expectation are
 * consumed by the rust side
 * (`rust/crates/mirri-core/tests/agent_list_cross_engine_tests.rs`); this
 * suite is the reference implementation that generates the expectation.
 *
 * Regenerate the expectation with:
 *
 *   UPDATE_CROSS_ENGINE_LIST_SNAPSHOT=1 pnpm exec vitest run test/agents-cross-engine-list.test.ts
 *
 * Normalization convention (see the fixture README for the full contract):
 * rows sorted by name; `suppressed_candidates[].reason` passes through for
 * the enum spellings `priority`/`disabled`, anything else becomes
 * `<load-failure>`; `skipped` entries carry the file basename and a
 * `<load-failure>` placeholder reason (the two engines word their
 * parse-error messages differently by design).
 */

import { copyFile, mkdir, mkdtemp, readdir, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { basename, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { IModelCatalog } from '@mirri-ai/agent-core-v2';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { type RunningServer } from '../src/start';
import { startReadyServer } from './helpers/startReadyServer';
import { TEST_HOST_IDENTITY } from './helpers/hostIdentity';
import { authHeaders } from './helpers/auth';

interface Envelope<T> {
  code: number;
  msg: string;
  data: T;
  request_id: string;
}

interface ProfileEntryWire {
  name: string;
  source: string;
  builtin: boolean;
  essential: boolean;
  enabled: boolean;
  has_override: boolean;
  suppressed_candidates?: { source: string; priority: number; reason: string }[];
}

interface ListData {
  items: ProfileEntryWire[];
  skipped?: { path: string; reason: string }[];
}

const fixtureRoot = fileURLToPath(
  new URL('../../agent-profile/test/fixtures/cross-engine-list', import.meta.url),
);
const userAgentsDir = join(fixtureRoot, 'user-agents');
const projectAgentsDir = join(fixtureRoot, 'project-agents');
const expectedPath = join(fixtureRoot, 'expected-list.json');
const updateSnapshot = process.env['UPDATE_CROSS_ENGINE_LIST_SNAPSHOT'] === '1';

/** The enum spellings a suppressed-candidate reason may carry verbatim. */
const SUPPRESSED_REASON_ENUM = new Set(['priority', 'disabled']);

/** Normalizes one wire row into the shared projection shape. */
function normalizeRow(row: ProfileEntryWire): Record<string, unknown> {
  const normalized: Record<string, unknown> = {
    name: row.name,
    source: row.source,
    builtin: row.builtin,
    essential: row.essential,
    enabled: row.enabled,
    has_override: row.has_override,
  };
  if (row.suppressed_candidates !== undefined && row.suppressed_candidates.length > 0) {
    normalized['suppressed_candidates'] = row.suppressed_candidates.map((candidate) => ({
      source: candidate.source,
      priority: candidate.priority,
      reason: SUPPRESSED_REASON_ENUM.has(candidate.reason)
        ? candidate.reason
        : '<load-failure>',
    }));
  }
  return normalized;
}

/** Normalizes the whole list response into the shared projection shape. */
function normalizeList(data: ListData): Record<string, unknown> {
  return {
    profiles: data.items.map(normalizeRow).toSorted((a, b) =>
      String(a['name']).localeCompare(String(b['name'])),
    ),
    skipped: (data.skipped ?? [])
      .map((skip) => ({
        // The absolute path is mount-dependent (isolated temp home here,
        // the fixture directory on the rust side) — only the basename is
        // part of the acceptance.
        path: basename(skip.path),
        // The two engines word their parse errors differently by design;
        // only the presence and identity of the skipped file is asserted.
        reason: '<load-failure>',
      }))
      .toSorted((a, b) => a.path.localeCompare(b.path)),
  };
}

describe('cross-engine agent profile list projection (R4-6)', () => {
  let server: RunningServer | undefined;
  let home: string | undefined;
  let base: string;

  beforeEach(async () => {
    home = await mkdtemp(join(tmpdir(), 'mirri-server-v2-agents-xe-'));
    // Mount the corpus the way both engines see it (see the fixture
    // README): the user-agents directory becomes the user agents root,
    // the project-agents directory the registered workspace's project
    // root. The legacy-profiles directory is rust-only and deliberately
    // NOT mounted here (documented exception).
    const homeAgents = join(home!, 'agents');
    await mkdir(homeAgents, { recursive: true });
    for (const file of await readdir(userAgentsDir)) {
      await copyFile(join(userAgentsDir, file), join(homeAgents, file));
    }
    const projectRoot = join(home!, 'project');
    const projectAgents = join(projectRoot, '.mirri-code', 'agents');
    await mkdir(projectAgents, { recursive: true });
    for (const file of await readdir(projectAgentsDir)) {
      await copyFile(join(projectAgentsDir, file), join(projectAgents, file));
    }

    const modelCatalog: IModelCatalog = {
      _serviceBrand: undefined,
      get: () => { throw new Error('modelCatalog.get not exercised'); },
      getRequester: () => { throw new Error('modelCatalog.getRequester not exercised'); },
      inspect: () => { throw new Error('modelCatalog.inspect not exercised'); },
      ping: () => { throw new Error('modelCatalog.ping not exercised'); },
      getById: () => undefined,
      listModels: async () => [],
      listProviders: async () => [],
      getProvider: async () => { throw new Error('modelCatalog.getProvider not exercised'); },
      setDefaultModel: async () => { throw new Error('modelCatalog.setDefaultModel not exercised'); },
      removeModel: async () => ({ deleted: true }),
    };
    server = await startReadyServer({
      hostIdentity: TEST_HOST_IDENTITY,
      host: '127.0.0.1',
      port: 0,
      homeDir: home,
      logLevel: 'silent',
      seeds: [[IModelCatalog, modelCatalog]],
    });
    base = `http://127.0.0.1:${server.port}`;

    // Register the workspace so the agents routes run their primary read
    // path (the catalog projection over the registry) — the same path the
    // rust engine's list handler mirrors.
    const res = await fetch(`${base}/api/v1/workspaces`, {
      method: 'POST',
      headers: authHeaders(server, { 'content-type': 'application/json' }),
      body: JSON.stringify({ root: projectRoot, name: 'proj' }),
    });
    const body = (await res.json()) as Envelope<{ id: string }>;
    expect(body.code, 'workspace registration must succeed').toBe(0);
  });

  afterEach(async () => {
    if (server !== undefined) {
      await server.close();
      server = undefined;
    }
    if (home !== undefined) {
      await rm(home, { recursive: true, force: true, maxRetries: 3, retryDelay: 25 } as never);
      home = undefined;
    }
  });

  async function listAgents(): Promise<ListData> {
    const res = await fetch(`${base}/api/v1/agents`, {
      headers: authHeaders(server as RunningServer),
    });
    const body = (await res.json()) as Envelope<ListData>;
    expect(body.code).toBe(0);
    return body.data;
  }

  async function listAgentsNormalized(): Promise<Record<string, unknown>> {
    return normalizeList(await listAgents());
  }

  it('projects the corpus identically to the shared expected-list.json', async () => {
    // The workspace handlers load asynchronously after registration, and the
    // user-agents loader registers its single contribution afterwards — poll
    // until the user suppressed candidates surface too, so the snapshot
    // assertion never races the user rows (the agents.test.ts M1 pattern).
    await vi.waitFor(async () => {
      const data = await listAgents();
      const shared = data.items.find((p) => p.name === 'shared');
      expect(shared?.source).toBe('project');
      expect(shared?.suppressed_candidates).toContainEqual({
        source: 'user',
        priority: 10,
        reason: 'priority',
      });
    });

    const actual = await listAgentsNormalized();

    if (updateSnapshot) {
      const { writeFileSync } = await import('node:fs');
      writeFileSync(expectedPath, `${JSON.stringify(actual, null, 2)}\n`, 'utf8');
    }

    const expected = JSON.parse(
      (await import('node:fs')).readFileSync(expectedPath, 'utf8'),
    ) as Record<string, unknown>;
    expect(actual).toEqual(expected);
  });

  // Key-sample assertions — these pin the semantics the shared expectation
  // encodes, so the snapshot cannot become self-fulfilling: if either
  // engine's merge behavior regresses, these fail independently of the
  // (regenerable) expectation file.
  it('pins the essential-coverage and multi-source-collision semantics explicitly', async () => {
    // Same user-loader race as the snapshot test above: wait for the user
    // suppressed candidate before asserting the pinned semantics.
    await vi.waitFor(async () => {
      const data = await listAgents();
      const shared = data.items.find((p) => p.name === 'shared');
      expect(shared?.source).toBe('project');
      expect(shared?.suppressed_candidates).toContainEqual({
        source: 'user',
        priority: 10,
        reason: 'priority',
      });
    });
    const data = await listAgents();

    // Essential-name coverage (D14/D16): the user file wins, the row keeps
    // `essential: true`, and the displaced builtin definition is the row's
    // single suppressed candidate.
    const agent = data.items.find((p) => p.name === 'agent');
    expect(agent).toBeDefined();
    expect(agent?.source).toBe('user');
    expect(agent?.builtin).toBe(false);
    expect(agent?.essential).toBe(true);
    expect(agent?.enabled).toBe(true);
    expect(agent?.has_override).toBe(true);
    expect(agent?.suppressed_candidates).toEqual([
      { source: 'builtin', priority: 0, reason: 'priority' },
    ]);

    // Non-essential builtin coverage: same row shape as the essential.
    const explore = data.items.find((p) => p.name === 'explore');
    expect(explore?.source).toBe('user');
    expect(explore?.builtin).toBe(false);
    expect(explore?.has_override).toBe(true);
    expect(explore?.suppressed_candidates).toEqual([
      { source: 'builtin', priority: 0, reason: 'priority' },
    ]);

    // Multi-source collision: the project file wins, the user file is the
    // suppressed loser with the loser's own source and priority.
    const shared = data.items.find((p) => p.name === 'shared');
    expect(shared?.source).toBe('project');
    expect(shared?.suppressed_candidates).toEqual([
      { source: 'user', priority: 10, reason: 'priority' },
    ]);

    // Suppressed-candidate order with two losers: the file loser first,
    // the displaced builtin LAST (Task 27's ordering alignment).
    const plan = data.items.find((p) => p.name === 'plan');
    expect(plan?.source).toBe('project');
    expect(plan?.suppressed_candidates).toEqual([
      { source: 'user', priority: 10, reason: 'priority' },
      { source: 'builtin', priority: 0, reason: 'priority' },
    ]);

    // The `disabled` reason: a disabled higher-priority file never wins.
    const guarded = data.items.find((p) => p.name === 'guarded');
    expect(guarded?.source).toBe('user');
    expect(guarded?.enabled).toBe(true);
    expect(guarded?.suppressed_candidates).toEqual([
      { source: 'project', priority: 30, reason: 'disabled' },
    ]);

    // A lone disabled profile stays listed but marked, and its own
    // disabled candidate is the row's self-referential suppressed entry
    // (the TS case b — `disabledWinners`: nothing else declared the name,
    // so the first disabled candidate IS the row).
    const sleeper = data.items.find((p) => p.name === 'sleeper');
    expect(sleeper?.enabled).toBe(false);
    expect(sleeper?.suppressed_candidates).toEqual([
      { source: 'user', priority: 10, reason: 'disabled' },
    ]);

    // An uncovered non-essential builtin stays clean: builtin
    // provenance, no suppressed candidates (the wire spells an empty list).
    const coder = data.items.find((p) => p.name === 'coder');
    expect(coder?.source).toBe('builtin');
    expect(coder?.builtin).toBe(true);
    expect(coder?.suppressed_candidates ?? []).toEqual([]);

    // The parse-failure sample lands in the skip channel with a non-empty
    // engine-worded reason (normalized to the placeholder above).
    const broken = data.skipped?.find((s) => s.path.endsWith('broken.md'));
    expect(broken).toBeDefined();
    expect(broken?.reason.length).toBeGreaterThan(0);

    // Unknown frontmatter keys must not break discovery.
    expect(data.items.find((p) => p.name === 'mystery')).toBeDefined();
  });
});
