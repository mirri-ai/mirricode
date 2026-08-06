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

  it('should return the server enabled count as-is since toolCount already excludes disabled tools', () => {
    // toolCount is `enabledNames.size`, so disabled tools are already excluded.
    // A naive `toolCount - disabledForServer` would double-subtract.
    expect(enabledToolCount(3, 1)).toBe(3);
    expect(enabledToolCount(0, 0)).toBe(0);
  });

  it('should reconstruct total discovered tools as enabled plus disabled for the server', () => {
    expect(totalToolCount(3, 1)).toBe(4);
    expect(totalToolCount(0, 0)).toBe(0);
  });
});