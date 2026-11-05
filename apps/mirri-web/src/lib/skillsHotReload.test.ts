// apps/mirri-web/src/lib/skillsHotReload.test.ts
// Unit tests for the skillCatalogChanged invalidation mapping (ADR-013
// decision 3): which cached skill keys a catalog change must refresh.

import { describe, expect, it } from 'vitest';
import { skillCacheKeysToRefresh } from './skillsHotReload';

describe('skillCacheKeysToRefresh', () => {
  it('should refresh every cached session of the event workspace and the workspace key when caches were populated', () => {
    const result = skillCacheKeysToRefresh(
      'ws-1',
      [
        { id: 's-1', workspaceId: 'ws-1', cwd: '' },
        { id: 's-2', cwd: '/repo' },
        { id: 's-3', workspaceId: 'ws-2', cwd: '' },
        { id: 's-4', workspaceId: 'ws-1', cwd: '' },
      ],
      [{ id: 'ws-1', root: '/repo' }],
      new Set(['s-1', 's-2', 's-3', 's-4']),
      new Set(['ws-1', 'ws-2']),
    );
    expect(result).toEqual({ sessions: ['s-1', 's-2', 's-4'], workspace: true });
  });

  it('should not fetch anything when the UI never pulled the workspace caches', () => {
    const result = skillCacheKeysToRefresh(
      'ws-9',
      [{ id: 's-1', workspaceId: 'ws-9', cwd: '' }],
      [{ id: 'ws-9', root: '/x' }],
      new Set<string>(),
      new Set<string>(),
    );
    expect(result).toEqual({ sessions: [], workspace: false });
  });

  it('should skip cached sessions of other workspaces', () => {
    const result = skillCacheKeysToRefresh(
      'ws-2',
      [
        { id: 's-1', workspaceId: 'ws-1', cwd: '' },
        { id: 's-2', workspaceId: 'ws-2', cwd: '' },
      ],
      [],
      new Set(['s-1', 's-2']),
      new Set(['ws-2']),
    );
    expect(result).toEqual({ sessions: ['s-2'], workspace: true });
  });
});
