import { describe, expect, it } from 'vitest';

import { countDisabledForServer, enabledToolCount, totalToolCount } from '../src/components/settings/mcpToolCounting';

describe('mcp tool counting semantics', () => {
  it('should count only qualified disabled tools that belong to the given server', () => {
    const qualified = [
      'mcp__alpha__read',
      'mcp__alpha__write',
      'mcp__beta__read',
    ];
    expect(countDisabledForServer(qualified, 'alpha')).toBe(2);
    expect(countDisabledForServer(qualified, 'beta')).toBe(1);
    expect(countDisabledForServer(qualified, 'gamma')).toBe(0);
    expect(countDisabledForServer([], 'alpha')).toBe(0);
  });

  it('should subtract the server denylist from the raw discovered count since toolCount includes disabled tools', () => {
    // The rust wire's toolCount is the raw `tools/list` size (SDR-S005 M5
    // fix — the old "already excludes disabled" reading described the v1
    // TS engine). A server advertising 7 tools with 1 denied reads 6/7.
    expect(enabledToolCount(7, 1)).toBe(6);
    expect(enabledToolCount(0, 0)).toBe(0);
    expect(enabledToolCount(0, 3)).toBe(0);
  });

  it('should return the raw discovered count as the total regardless of the denylist', () => {
    expect(totalToolCount(7, 1)).toBe(7);
    expect(totalToolCount(0, 0)).toBe(0);
    expect(totalToolCount(0, 3)).toBe(0);
  });
});
