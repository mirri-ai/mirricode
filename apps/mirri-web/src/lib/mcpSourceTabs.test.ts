// apps/mirri-web/src/lib/mcpSourceTabs.test.ts
// Tab grouping for the settings MCP panel (SDR-S005 / D-S5).

import { describe, expect, it } from 'vitest';

import { filterServersByTab, tabOfServer } from './mcpSourceTabs';

describe('tabOfServer (source → tab membership)', () => {
  it('should fold projectRoot and project into the project tab when both project flavors exist', () => {
    expect(tabOfServer('projectRoot')).toBe('project');
    expect(tabOfServer('project')).toBe('project');
  });

  it('should default undefined-source servers to the user tab when the backend omits source', () => {
    expect(tabOfServer(undefined)).toBe('user');
    expect(tabOfServer('user')).toBe('user');
    expect(tabOfServer('plugin')).toBe('plugin');
  });
});

describe('filterServersByTab (tab → filtered list)', () => {
  const servers = [
    { name: 'global-db', source: 'user' },
    { name: 'team-shared', source: 'projectRoot' },
    { name: 'my-local', source: 'project' },
    { name: 'bundled', source: 'plugin' },
    { name: 'legacy', source: undefined },
  ];

  it('should return every server unchanged when the tab is all', () => {
    expect(filterServersByTab(servers, 'all').map((s) => s.name)).toEqual([
      'global-db',
      'team-shared',
      'my-local',
      'bundled',
      'legacy',
    ]);
  });

  it('should include both project flavors and exclude the rest when the tab is project', () => {
    expect(filterServersByTab(servers, 'project').map((s) => s.name)).toEqual([
      'team-shared',
      'my-local',
    ]);
  });

  it('should group undefined-source servers with the user tab when filtering by user', () => {
    expect(filterServersByTab(servers, 'user').map((s) => s.name)).toEqual([
      'global-db',
      'legacy',
    ]);
  });

  it('should return only plugin-sourced servers when the tab is plugin', () => {
    expect(filterServersByTab(servers, 'plugin').map((s) => s.name)).toEqual(['bundled']);
  });
});
