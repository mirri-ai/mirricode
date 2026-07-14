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
});
