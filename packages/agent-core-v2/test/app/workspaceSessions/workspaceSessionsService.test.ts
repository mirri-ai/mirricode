import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  LifecycleScope,
  ScopeActivation,
  _clearScopedRegistryForTests,
  registerScopedService,
} from '#/_base/di/scope';
import { createScopedTestHost, stubPair } from '#/_base/di/test';
import {
  ISessionIndex,
  type SessionListQuery,
  type SessionSummary,
} from '#/app/sessionIndex/sessionIndex';
import { IWorkspaceAliases } from '#/app/workspaceAliases/workspaceAliases';
import {
  IWorkspaceSessions,
  RECENT_SESSIONS_LIMIT,
} from '#/app/workspaceSessions/workspaceSessions';
import { WorkspaceSessionsService } from '#/app/workspaceSessions/workspaceSessionsService';

class FakeSessionIndex implements ISessionIndex {
  readonly _serviceBrand: undefined;
  lastListQuery: SessionListQuery | undefined;
  listQueries: SessionListQuery[] = [];
  items: readonly SessionSummary[] = [];
  countActiveCalls: string[][] = [];
  countActiveResult = 0;

  async list(query: SessionListQuery) {
    this.lastListQuery = query;
    this.listQueries.push(query);
    return { items: this.items };
  }

  async get(_id: string): Promise<SessionSummary | undefined> {
    return undefined;
  }

  async countActive(workspaceIds: readonly string[]): Promise<number> {
    this.countActiveCalls.push([...workspaceIds]);
    return this.countActiveResult;
  }

  async refreshWorkspace(): Promise<{ inserted: number; removed: number }> {
    return { inserted: 0, removed: 0 };
  }
}

class FakeWorkspaceAliases implements IWorkspaceAliases {
  readonly _serviceBrand: undefined;
  aliases: Record<string, readonly string[]> = {};

  resolveAliasIds(id: string): Promise<readonly string[]> {
    return Promise.resolve(this.aliases[id] ?? [id]);
  }

  clearCaches(): void {}

  prewarm(): Promise<void> {
    return Promise.resolve();
  }
}

describe('WorkspaceSessionsService', () => {
  let currentHost: ReturnType<typeof createScopedTestHost> | undefined;

  beforeEach(() => {
    _clearScopedRegistryForTests();
    registerScopedService(
      LifecycleScope.App,
      IWorkspaceSessions,
      WorkspaceSessionsService,
      ScopeActivation.OnDemand,
      'workspaceSessions',
    );
  });

  afterEach(() => {
    currentHost?.dispose();
    currentHost = undefined;
  });

  function build(): {
    sessions: IWorkspaceSessions;
    index: FakeSessionIndex;
    aliases: FakeWorkspaceAliases;
  } {
    const index = new FakeSessionIndex();
    const aliases = new FakeWorkspaceAliases();
    const host = createScopedTestHost([
      stubPair(ISessionIndex, index),
      stubPair(IWorkspaceAliases, aliases),
    ]);
    currentHost = host;
    return { sessions: host.app.accessor.get(IWorkspaceSessions), index, aliases };
  }

  function summary(id: string, workspaceId: string, updatedAt: number): SessionSummary {
    return { id, workspaceId, createdAt: updatedAt - 1, updatedAt, archived: false };
  }

  it('listRecent delegates with the folded alias set and the recent limit', async () => {
    const { sessions, index, aliases } = build();
    aliases.aliases['wd_abc'] = ['wd_abc', 'wd_abc_legacy'];

    await sessions.listRecent('wd_abc');

    expect(index.lastListQuery).toEqual({
      workspaceIds: ['wd_abc', 'wd_abc_legacy'],
      limit: RECENT_SESSIONS_LIMIT,
    });
    expect(RECENT_SESSIONS_LIMIT).toBe(20);
  });

  it('listRecent returns the index items for the workspace', async () => {
    const { sessions, index } = build();
    const items = [summary('s2', 'wd_abc', 200), summary('s1', 'wd_abc', 100)];
    index.items = items;

    await expect(sessions.listRecent('wd_abc')).resolves.toEqual(items);
  });

  it('listRecent returns an empty array when the workspace has no sessions', async () => {
    const { sessions } = build();

    await expect(sessions.listRecent('wd_empty')).resolves.toEqual([]);
  });

  it('count folds aliases and delegates to the index active-session count', async () => {
    const { sessions, index, aliases } = build();
    aliases.aliases['wd_abc'] = ['wd_abc', 'wd_abc_legacy'];
    index.countActiveResult = 3;

    await expect(sessions.count('wd_abc')).resolves.toBe(3);
    expect(index.countActiveCalls).toEqual([['wd_abc', 'wd_abc_legacy']]);
  });

  it('count returns 0 when the workspace has no sessions', async () => {
    const { sessions } = build();

    await expect(sessions.count('wd_empty')).resolves.toBe(0);
  });
});
