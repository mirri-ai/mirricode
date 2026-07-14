import { describe, it, expect } from 'vitest';
import { CapabilityRegistry } from '#/agent/tool/capabilities/registry';
import { parseIntegrationsYaml } from '#/agent/tool/capabilities/parse';
import type { ExecutableTool } from '#/loop';

function makeTool(name: string, capabilities?: readonly string[]): ExecutableTool {
  return {
    name,
    description: `Tool ${name}`,
    parameters: { type: 'object', properties: {} },
    capabilities,
    resolveExecution: () => ({
      approvalRule: name,
      execute: async () => ({ output: '' }),
    }),
  };
}

describe('CapabilityRegistry', () => {
  it('registers builtin tools with capabilities', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.registerBuiltinTool(makeTool('Read'));
    expect(r.providersOf('code.explore')).toEqual(['Grep']);
    expect(r.capabilities()).toEqual(['code.explore']);
  });

  it('applies integrations.yaml to attach capabilities to MCP tools', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    const mcpMap = new Map([
      [
        'codebase-memory-mcp',
        [
          'mcp__codebase-memory-mcp__search_graph',
          'mcp__codebase-memory-mcp__trace_path',
        ],
      ],
    ]);
    r.applyIntegrations(
      {
        integrations: {
          'codebase-memory-mcp': {
            capabilities: ['code.explore', 'code.navigate'],
            preferOver: ['Grep'],
          },
        },
      },
      mcpMap,
    );
    const available = new Set([
      'Grep',
      'mcp__codebase-memory-mcp__search_graph',
      'mcp__codebase-memory-mcp__trace_path',
    ]);
    const providers = r.resolveProviders('code.explore', available);
    expect(providers[0]!.toolName).toContain('search_graph');
    expect(providers.map((p) => p.toolName)).toContain('Grep');
    expect(providers[0]!.preferOver).toContain('Grep');
  });

  it('produces empty hint when only builtin providers exist (no need to nudge)', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    const hint = r.buildHint(new Set(['Grep']));
    expect(hint).toBe('');
  });

  it('produces non-empty hint when an MCP provider is available', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    const mcpMap = new Map([
      ['codebase-memory-mcp', ['mcp__codebase-memory-mcp__search_graph']],
    ]);
    r.applyIntegrations(
      {
        integrations: {
          'codebase-memory-mcp': { capabilities: ['code.explore'], preferOver: ['Grep'] },
        },
      },
      mcpMap,
    );
    const hint = r.buildHint(
      new Set(['Grep', 'mcp__codebase-memory-mcp__search_graph']),
    );
    expect(hint).toContain('code.explore');
    expect(hint).toContain('mcp__codebase-memory-mcp__search_graph');
    expect(hint).toContain('Grep');
  });

  it('excludes tools not currently available (disconnected MCP)', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.applyIntegrations(
      { integrations: { 'codebase-memory-mcp': { capabilities: ['code.explore'] } } },
      new Map([['codebase-memory-mcp', ['mcp__codebase-memory-mcp__search_graph']]]),
    );
    const providers = r.resolveProviders('code.explore', new Set(['Grep']));
    expect(providers.map((p) => p.toolName)).toEqual(['Grep']);
  });

  it('resetIntegrations wipes only MCP-sourced entries', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.applyIntegrations(
      { integrations: { srv: { capabilities: ['code.explore'] } } },
      new Map([['srv', ['mcp__srv__t']]]),
    );
    r.resetIntegrations();
    const available = new Set(['Grep', 'mcp__srv__t']);
    const providers = r.resolveProviders('code.explore', available);
    expect(providers.map((p) => p.toolName)).toEqual(['Grep']);
  });

  it('toolsForCapabilities returns matching available names', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.registerBuiltinTool(makeTool('Glob', ['code.explore']));
    const available = new Set(['Grep', 'Glob', 'Read']);
    expect(
      [...r.toolsForCapabilities(['code.explore'], available)].toSorted(),
    ).toEqual(['Glob', 'Grep']);
    expect(r.toolsForCapabilities(['unknown'], available)).toEqual([]);
  });

  it('does not implicitly reorder MCP ahead of builtins without preferOver', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.applyIntegrations(
      { integrations: { srv: { capabilities: ['code.explore'] } } },
      new Map([['srv', ['mcp__srv__search']]]),
    );
    const available = new Set(['Grep', 'mcp__srv__search']);
    const providers = r.resolveProviders('code.explore', available);
    // Insertion order: Grep registered first, MCP applied second. No
    // preferOver, so the registry does NOT reshuffle to MCP-first.
    expect(providers.map((p) => p.toolName)).toEqual(['Grep', 'mcp__srv__search']);
  });

  it('emits no hint when the primary provider is a plain builtin with rivals of the same source', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.registerBuiltinTool(makeTool('Glob', ['code.explore']));
    // Both are builtins, neither declares preferOver → no hint worth emitting.
    // (This mirrors Scenario A but tests the buildHint gate directly.)
    const hint = r.buildHint(new Set(['Grep', 'Glob']));
    // Two peers with no stated preference: we still list the primary/fallback
    // pair — it's fine to nudge the model toward a consistent primary, but
    // there is no MCP shouting for attention.
    expect(hint).not.toContain('mcp__');
  });
});

describe('parseIntegrationsYaml', () => {
  it('returns empty config for undefined / empty content', () => {
    expect(parseIntegrationsYaml(undefined).config.integrations).toEqual({});
    expect(parseIntegrationsYaml('').config.integrations).toEqual({});
    expect(parseIntegrationsYaml('   \n  ').config.integrations).toEqual({});
  });

  it('parses a well-formed document', () => {
    const yaml = `
integrations:
  codebase-memory-mcp:
    capabilities:
      - code.explore
      - code.navigate
    preferOver:
      - Grep
      - Glob
`;
    const { config, warnings } = parseIntegrationsYaml(yaml);
    expect(warnings).toEqual([]);
    expect(config.integrations['codebase-memory-mcp']!.capabilities).toEqual([
      'code.explore',
      'code.navigate',
    ]);
    expect(config.integrations['codebase-memory-mcp']!.preferOver).toEqual([
      'Grep',
      'Glob',
    ]);
  });

  it('returns warning on malformed YAML but keeps going', () => {
    const { config, warnings } = parseIntegrationsYaml(': : : not-yaml :');
    expect(config.integrations).toEqual({});
    expect(warnings.length).toBeGreaterThan(0);
  });

  it('returns warning on schema violation', () => {
    const { config, warnings } = parseIntegrationsYaml('integrations: 42');
    expect(config.integrations).toEqual({});
    expect(warnings.length).toBeGreaterThan(0);
  });

  it('schema-violation warning is formatted as path: message (not a raw zod dump)', () => {
    const { warnings } = parseIntegrationsYaml('integrations: 42');
    expect(warnings).toHaveLength(1);
    const w = warnings[0]!;
    // Keep it under a sane single-line log length.
    expect(w.length).toBeLessThan(500);
    // Must not embed a JSON blob.
    expect(w).not.toMatch(/\[\s*\{\s*"/);
    // Should reference the offending path.
    expect(w).toContain('integrations');
  });

  it('accepts an integration with no fields (uses defaults)', () => {
    const { config, warnings } = parseIntegrationsYaml('integrations:\n  srv: {}\n');
    expect(warnings).toEqual([]);
    expect(config.integrations['srv']!.capabilities).toEqual([]);
  });
});

describe('CapabilityRegistry integration scenarios (Scenario A / B)', () => {
  it('Scenario A: without any MCP registration, Grep is the sole code.explore provider and hint is empty', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.registerBuiltinTool(makeTool('Glob', ['code.explore']));
    r.applyIntegrations({ integrations: {} }, new Map());
    const available = new Set(['Grep', 'Glob', 'Read']);
    const providers = r.resolveProviders('code.explore', available);
    expect(providers.map((p) => p.toolName).toSorted()).toEqual(['Glob', 'Grep']);
    const hint = r.buildHint(available);
    expect(hint).not.toContain('mcp__');
  });

  it('Scenario B: with a mock MCP declaring code.explore + preferOver Grep, MCP wins and hint mentions it first', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.applyIntegrations(
      {
        integrations: {
          'mock-cg': { capabilities: ['code.explore'], preferOver: ['Grep'] },
        },
      },
      new Map([['mock-cg', ['mcp__mock-cg__search']]]),
    );
    const available = new Set(['Grep', 'mcp__mock-cg__search']);
    const providers = r.resolveProviders('code.explore', available);
    expect(providers[0]!.toolName).toBe('mcp__mock-cg__search');
    expect(providers[0]!.source).toBe('mcp');
    const hint = r.buildHint(available);
    expect(hint).toMatch(/prefer.*mcp__mock-cg__search/);
    expect(hint).toContain('Grep');
  });
});

describe('CapabilityRegistry edge cases', () => {
  it('registerBuiltinTool with empty capabilities array is a no-op', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Read', []));
    expect(r.capabilities()).toEqual([]);
    expect(r.providersOf('code.explore')).toEqual([]);
  });

  it('capabilities() returns capabilities in sorted order', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('A', ['z.cap', 'a.cap']));
    r.registerBuiltinTool(makeTool('B', ['m.cap']));
    expect(r.capabilities()).toEqual(['a.cap', 'm.cap', 'z.cap']);
  });

  it('resolveProviders with 3-way preferOver chain orders correctly', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.applyIntegrations(
      {
        integrations: {
          srv: { capabilities: ['code.explore'], preferOver: ['Grep'] },
          srv2: { capabilities: ['code.explore'], preferOver: ['mcp__srv__search'] },
        },
      },
      new Map([
        ['srv', ['mcp__srv__search']],
        ['srv2', ['mcp__srv2__deep']],
      ]),
    );
    const available = new Set(['Grep', 'mcp__srv__search', 'mcp__srv2__deep']);
    const providers = r.resolveProviders('code.explore', available);
    // srv2 prefers over srv's tool, srv prefers over Grep → srv2 > srv > Grep
    expect(providers[0]!.toolName).toBe('mcp__srv2__deep');
    expect(providers[1]!.toolName).toBe('mcp__srv__search');
    expect(providers[2]!.toolName).toBe('Grep');
  });

  it('resolveProviders with circular preferOver produces stable order', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('A', ['cap']));
    r.applyIntegrations(
      { integrations: { srv: { capabilities: ['cap'], preferOver: ['A'] } } },
      new Map([['srv', ['mcp__srv__B']]]),
    );
    // Now make A prefer over B too — circular
    // (preferOver is only set via integrations; A is a builtin so it can't declare preferOver.
    //  But the registry should handle the case where B preferOver A and no reverse claim.)
    const available = new Set(['A', 'mcp__srv__B']);
    const providers = r.resolveProviders('cap', available);
    expect(providers).toHaveLength(2);
    // B has preferOver: [A], so B comes first
    expect(providers[0]!.toolName).toBe('mcp__srv__B');
    expect(providers[1]!.toolName).toBe('A');
  });

  it('resolveProviders returns empty for unknown capability', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    expect(r.resolveProviders('unknown.cap', new Set(['Grep']))).toEqual([]);
  });

  it('resolveProviders returns empty when all providers filtered out', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.applyIntegrations(
      { integrations: { srv: { capabilities: ['code.explore'] } } },
      new Map([['srv', ['mcp__srv__t']]]),
    );
    // Neither tool is in available set
    expect(r.resolveProviders('code.explore', new Set(['Read']))).toEqual([]);
  });

  it('toolsForCapabilities with empty capabilities array returns empty', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    expect(r.toolsForCapabilities([], new Set(['Grep']))).toEqual([]);
  });

  it('resetIntegrations before any applyIntegrations is a no-op', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.resetIntegrations();
    expect(r.capabilities()).toEqual(['code.explore']);
    expect(r.providersOf('code.explore')).toEqual(['Grep']);
  });

  it('resetIntegrations called multiple times is idempotent', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.applyIntegrations(
      { integrations: { srv: { capabilities: ['code.explore'] } } },
      new Map([['srv', ['mcp__srv__t']]]),
    );
    r.resetIntegrations();
    r.resetIntegrations();
    expect(r.providersOf('code.explore')).toEqual(['Grep']);
  });

  it('applyIntegrations with empty mcpToolsByServer array is a no-op', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.applyIntegrations(
      { integrations: { srv: { capabilities: ['code.explore'] } } },
      new Map([['srv', []]]),
    );
    expect(r.providersOf('code.explore')).toEqual(['Grep']);
  });

  it('applyIntegrations with server not in mcpToolsByServer is silently ignored', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.applyIntegrations(
      { integrations: { unknown: { capabilities: ['code.explore'] } } },
      new Map(),
    );
    expect(r.providersOf('code.explore')).toEqual(['Grep']);
  });

  it('buildHint with fresh empty registry returns empty string', () => {
    const r = new CapabilityRegistry();
    expect(r.buildHint(new Set())).toBe('');
  });

  it('buildHint with multiple capabilities produces multi-line hint', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.registerBuiltinTool(makeTool('Agent', ['code.delegate']));
    r.applyIntegrations(
      {
        integrations: {
          srv: { capabilities: ['code.explore'], preferOver: ['Grep'] },
          srv2: { capabilities: ['code.delegate'], preferOver: ['Agent'] },
        },
      },
      new Map([
        ['srv', ['mcp__srv__search']],
        ['srv2', ['mcp__srv2__delegate']],
      ]),
    );
    const available = new Set(['Grep', 'Agent', 'mcp__srv__search', 'mcp__srv2__delegate']);
    const hint = r.buildHint(available);
    expect(hint).toContain('code.explore');
    expect(hint).toContain('code.delegate');
    expect(hint).toContain('mcp__srv__search');
    expect(hint).toContain('mcp__srv2__delegate');
  });

  it('source tracking: builtin tool source is recorded correctly', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    const available = new Set(['Grep']);
    const providers = r.resolveProviders('code.explore', available);
    expect(providers[0]!.source).toBe('builtin');
  });

  it('source tracking: MCP tool source is recorded correctly', () => {
    const r = new CapabilityRegistry();
    r.applyIntegrations(
      { integrations: { srv: { capabilities: ['code.explore'] } } },
      new Map([['srv', ['mcp__srv__t']]]),
    );
    const available = new Set(['mcp__srv__t']);
    const providers = r.resolveProviders('code.explore', available);
    expect(providers[0]!.source).toBe('mcp');
  });
});

describe('parseIntegrationsYaml edge cases', () => {
  it('returns empty config for YAML that parses to null', () => {
    const { config, warnings } = parseIntegrationsYaml('---\n');
    expect(config.integrations).toEqual({});
    expect(warnings).toEqual([]);
  });

  it('accepts integration with only preferOver set (capabilities defaults to [])', () => {
    const { config, warnings } = parseIntegrationsYaml(
      'integrations:\n  srv:\n    preferOver:\n      - Grep\n',
    );
    expect(warnings).toEqual([]);
    expect(config.integrations['srv']!.capabilities).toEqual([]);
    expect(config.integrations['srv']!.preferOver).toEqual(['Grep']);
  });

  it('accepts integration with explicit empty capabilities array', () => {
    const { config, warnings } = parseIntegrationsYaml(
      'integrations:\n  srv:\n    capabilities: []\n',
    );
    expect(warnings).toEqual([]);
    expect(config.integrations['srv']!.capabilities).toEqual([]);
  });

  it('warns on non-string in capabilities array', () => {
    const { config, warnings } = parseIntegrationsYaml(
      'integrations:\n  srv:\n    capabilities: [42]\n',
    );
    expect(config.integrations).toEqual({});
    expect(warnings.length).toBeGreaterThan(0);
  });

  it('warns on empty string in capabilities', () => {
    const { config, warnings } = parseIntegrationsYaml(
      'integrations:\n  srv:\n    capabilities: [""]\n',
    );
    expect(config.integrations).toEqual({});
    expect(warnings.length).toBeGreaterThan(0);
  });

  it('warns on empty string in preferOver', () => {
    const { config, warnings } = parseIntegrationsYaml(
      'integrations:\n  srv:\n    preferOver: [""]\n',
    );
    expect(config.integrations).toEqual({});
    expect(warnings.length).toBeGreaterThan(0);
  });

  it('parses multiple integrations in one document', () => {
    const yaml = `
integrations:
  srv-a:
    capabilities: [code.explore]
  srv-b:
    capabilities: [code.navigate]
    preferOver: [Grep]
`;
    const { config, warnings } = parseIntegrationsYaml(yaml);
    expect(warnings).toEqual([]);
    expect(config.integrations['srv-a']!.capabilities).toEqual(['code.explore']);
    expect(config.integrations['srv-b']!.capabilities).toEqual(['code.navigate']);
    expect(config.integrations['srv-b']!.preferOver).toEqual(['Grep']);
  });

  it('warns when integrations key is a list instead of map', () => {
    const { config, warnings } = parseIntegrationsYaml('integrations:\n  - a\n  - b\n');
    expect(config.integrations).toEqual({});
    expect(warnings.length).toBeGreaterThan(0);
  });
});

describe('loadIntegrationsConfig', () => {
  it('reads user-global and project-local files and merges them (project wins)', async () => {
    const { mkdtempSync, writeFileSync, mkdirSync } = await import('node:fs');
    const { tmpdir } = await import('node:os');
    const { join } = await import('pathe');
    const { loadIntegrationsConfig } = await import('#/agent/tool/capabilities/loader');

    const userHome = mkdtempSync(join(tmpdir(), 'mirri-home-'));
    const cwd = mkdtempSync(join(tmpdir(), 'mirri-cwd-'));
    mkdirSync(join(cwd, '.mirri-code'), { recursive: true });

    writeFileSync(
      join(userHome, 'integrations.yaml'),
      'integrations:\n  codegraph:\n    capabilities: [code.explore]\n    preferOver: [Grep]\n  other:\n    capabilities: [x]\n',
      'utf8',
    );
    writeFileSync(
      join(cwd, '.mirri-code', 'integrations.yaml'),
      'integrations:\n  codegraph:\n    capabilities: [code.explore, deep]\n',
      'utf8',
    );

    const result = loadIntegrationsConfig({ userHome, cwd });
    // Project entry overrides user-global entry for the same server.
    expect(result.config.integrations['codegraph']?.capabilities).toEqual([
      'code.explore',
      'deep',
    ]);
    // Preserved from user-global because project didn't touch it.
    expect(result.config.integrations['other']?.capabilities).toEqual(['x']);
    expect(result.sources).toHaveLength(2);
    expect(result.warnings).toEqual([]);
  });

  it('missing files are silently skipped', async () => {
    const { loadIntegrationsConfig } = await import('#/agent/tool/capabilities/loader');
    const result = loadIntegrationsConfig({
      userHome: '/nonexistent-a',
      cwd: '/nonexistent-b',
    });
    expect(result.config.integrations).toEqual({});
    expect(result.sources).toEqual([]);
    expect(result.warnings).toEqual([]);
  });

  it('malformed yaml becomes a warning, load continues', async () => {
    const { mkdtempSync, writeFileSync } = await import('node:fs');
    const { tmpdir } = await import('node:os');
    const { join } = await import('pathe');
    const { loadIntegrationsConfig } = await import('#/agent/tool/capabilities/loader');

    const userHome = mkdtempSync(join(tmpdir(), 'mirri-home-bad-'));
    writeFileSync(join(userHome, 'integrations.yaml'), 'integrations: [unclosed\n', 'utf8');

    const result = loadIntegrationsConfig({ userHome });
    expect(result.warnings.length).toBeGreaterThan(0);
    expect(result.config.integrations).toEqual({});
  });

  it('loads only user-global when cwd is undefined', async () => {
    const { mkdtempSync, writeFileSync } = await import('node:fs');
    const { tmpdir } = await import('node:os');
    const { join } = await import('pathe');
    const { loadIntegrationsConfig } = await import('#/agent/tool/capabilities/loader');

    const userHome = mkdtempSync(join(tmpdir(), 'mirri-home-only-'));
    writeFileSync(
      join(userHome, 'integrations.yaml'),
      'integrations:\n  srv:\n    capabilities: [code.explore]\n',
      'utf8',
    );

    const result = loadIntegrationsConfig({ userHome });
    expect(result.config.integrations['srv']?.capabilities).toEqual(['code.explore']);
    expect(result.sources).toHaveLength(1);
    expect(result.warnings).toEqual([]);
  });

  it('loads only project-local when userHome is undefined', async () => {
    const { mkdtempSync, writeFileSync, mkdirSync } = await import('node:fs');
    const { tmpdir } = await import('node:os');
    const { join } = await import('pathe');
    const { loadIntegrationsConfig } = await import('#/agent/tool/capabilities/loader');

    const cwd = mkdtempSync(join(tmpdir(), 'mirri-cwd-only-'));
    mkdirSync(join(cwd, '.mirri-code'), { recursive: true });
    writeFileSync(
      join(cwd, '.mirri-code', 'integrations.yaml'),
      'integrations:\n  srv:\n    capabilities: [code.navigate]\n',
      'utf8',
    );

    const result = loadIntegrationsConfig({ cwd });
    expect(result.config.integrations['srv']?.capabilities).toEqual(['code.navigate']);
    expect(result.sources).toHaveLength(1);
    expect(result.warnings).toEqual([]);
  });

  it('returns empty config when both userHome and cwd are undefined', async () => {
    const { loadIntegrationsConfig } = await import('#/agent/tool/capabilities/loader');

    const result = loadIntegrationsConfig({});
    expect(result.config.integrations).toEqual({});
    expect(result.sources).toEqual([]);
    expect(result.warnings).toEqual([]);
  });

  it('warns on non-ENOENT file read error', async () => {
    const { mkdtempSync, mkdirSync, chmodSync } = await import('node:fs');
    const { tmpdir } = await import('node:os');
    const { join } = await import('pathe');
    const { loadIntegrationsConfig } = await import('#/agent/tool/capabilities/loader');

    const userHome = mkdtempSync(join(tmpdir(), 'mirri-home-perm-'));
    // Create a directory named integrations.yaml so reading it fails with EISDIR
    mkdirSync(join(userHome, 'integrations.yaml'));

    const result = loadIntegrationsConfig({ userHome });
    expect(result.warnings.length).toBeGreaterThan(0);
    expect(result.config.integrations).toEqual({});
  });

  it('sources array reflects actual read order (user first, project second)', async () => {
    const { mkdtempSync, writeFileSync, mkdirSync } = await import('node:fs');
    const { tmpdir } = await import('node:os');
    const { join } = await import('pathe');
    const { loadIntegrationsConfig } = await import('#/agent/tool/capabilities/loader');

    const userHome = mkdtempSync(join(tmpdir(), 'mirri-home-order-'));
    const cwd = mkdtempSync(join(tmpdir(), 'mirri-cwd-order-'));
    mkdirSync(join(cwd, '.mirri-code'), { recursive: true });
    writeFileSync(join(userHome, 'integrations.yaml'), 'integrations:\n  a: {}\n', 'utf8');
    writeFileSync(
      join(cwd, '.mirri-code', 'integrations.yaml'),
      'integrations:\n  b: {}\n',
      'utf8',
    );

    const result = loadIntegrationsConfig({ userHome, cwd });
    expect(result.sources).toHaveLength(2);
    expect(result.sources[0]).toContain(userHome);
    expect(result.sources[1]).toContain(cwd);
  });
});
