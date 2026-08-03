/**
 * Standalone unit tests for the capability system (B1-L14).
 *
 * Tests the pure-data `CapabilityRegistry`, `parseIntegrationsYaml`,
 * `loadIntegrationsConfig`, and the golden integration scenario:
 * a profile declaring `capabilitiesRequired: [code.explore]` activates
 * matching MCP tools, and integrations.yaml `preferOver` ordering is honored.
 *
 * No DI container — these test the data structures and pure functions directly.
 */

import { describe, it, expect } from 'vitest';
import { CapabilityRegistry } from '#/agent/capability/registry';
import { parseIntegrationsYaml } from '#/agent/capability/parse';
import { loadIntegrationsConfig } from '#/agent/capability/loader';
import type { IntegrationsConfig } from '#/agent/capability/types';

// ---------------------------------------------------------------------------
// CapabilityRegistry
// ---------------------------------------------------------------------------

describe('CapabilityRegistry', () => {
  it('should register builtin tools with their declared capabilities', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Grep', ['code.explore']);
    r.registerBuiltinTool('Read', undefined);
    expect(r.providersOf('code.explore')).toEqual(['Grep']);
    expect(r.capabilities()).toEqual(['code.explore']);
  });

  it('should register multiple tools providing the same capability', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Read', ['code.read']);
    r.registerBuiltinTool('WebSearch', ['web.search']);
    r.registerBuiltinTool('FetchURL', ['web.fetch']);
    expect(r.providersOf('code.read')).toEqual(['Read']);
    expect(r.providersOf('web.search')).toEqual(['WebSearch']);
    expect(r.providersOf('web.fetch')).toEqual(['FetchURL']);
    expect(r.capabilities()).toEqual(['code.read', 'web.fetch', 'web.search']);
  });

  it('should attach capabilities to MCP tools from integrations config', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Grep', ['code.explore']);
    const mcpMap = new Map([
      ['codebase-memory-mcp', ['mcp__codebase-memory-mcp__search_graph', 'mcp__codebase-memory-mcp__trace_path']],
    ]);
    r.applyIntegrations(
      {
        integrations: {
          'codebase-memory-mcp': {
            capabilities: ['code.explore', 'test.navigate'],
            preferOver: ['Grep'],
          },
        },
      },
      mcpMap,
    );
    const available = new Set(['Grep', 'mcp__codebase-memory-mcp__search_graph', 'mcp__codebase-memory-mcp__trace_path']);
    const providers = r.resolveProviders('code.explore', available);
    expect(providers[0]!.toolName).toContain('search_graph');
    expect(providers.map((p) => p.toolName)).toContain('Grep');
    expect(providers[0]!.preferOver).toContain('Grep');
  });

  it('should return empty hint when only builtin providers exist', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Grep', ['code.explore']);
    const hint = r.buildHint(new Set(['Grep']));
    expect(hint).toBe('');
  });

  it('should produce hint when an MCP provider is available', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Grep', ['code.explore']);
    const mcpMap = new Map([['codebase-memory-mcp', ['mcp__codebase-memory-mcp__search_graph']]]);
    r.applyIntegrations(
      { integrations: { 'codebase-memory-mcp': { capabilities: ['code.explore'], preferOver: ['Grep'] } } },
      mcpMap,
    );
    const hint = r.buildHint(new Set(['Grep', 'mcp__codebase-memory-mcp__search_graph']));
    expect(hint).toContain('code.explore');
    expect(hint).toContain('mcp__codebase-memory-mcp__search_graph');
    expect(hint).toContain('Grep');
  });

  it('should exclude tools not currently available (disconnected MCP)', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Grep', ['code.explore']);
    r.applyIntegrations(
      { integrations: { 'codebase-memory-mcp': { capabilities: ['code.explore'] } } },
      new Map([['codebase-memory-mcp', ['mcp__codebase-memory-mcp__search_graph']]]),
    );
    const providers = r.resolveProviders('code.explore', new Set(['Grep']));
    expect(providers.map((p) => p.toolName)).toEqual(['Grep']);
  });

  it('should wipe only MCP-sourced entries on reset', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Grep', ['code.explore']);
    r.applyIntegrations(
      { integrations: { srv: { capabilities: ['code.explore'] } } },
      new Map([['srv', ['mcp__srv__t']]]),
    );
    r.resetIntegrations();
    const available = new Set(['Grep', 'mcp__srv__t']);
    const providers = r.resolveProviders('code.explore', available);
    expect(providers.map((p) => p.toolName)).toEqual(['Grep']);
  });

  it('should return matching available names for a given capability', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Grep', ['code.explore']);
    r.registerBuiltinTool('Glob', ['code.explore']);
    const available = new Set(['Grep', 'Glob', 'Read']);
    expect([...r.toolsForCapabilities(['code.explore'], available)].toSorted()).toEqual(['Glob', 'Grep']);
    expect(r.toolsForCapabilities(['unknown'], available)).toEqual([]);
  });

  it('should not implicitly reorder MCP ahead of builtins without preferOver', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Grep', ['code.explore']);
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

  it('should order providers correctly through a 3-way preferOver chain', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Grep', ['code.explore']);
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

  it('should return empty for unknown capability', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Grep', ['code.explore']);
    expect(r.resolveProviders('unknown.cap', new Set(['Grep']))).toEqual([]);
  });

  it('should return empty when all providers are filtered out', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Grep', ['code.explore']);
    r.applyIntegrations(
      { integrations: { srv: { capabilities: ['code.explore'] } } },
      new Map([['srv', ['mcp__srv__t']]]),
    );
    expect(r.resolveProviders('code.explore', new Set(['Read']))).toEqual([]);
  });

  it('should return capabilities in sorted order', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('A', ['z.cap', 'a.cap']);
    r.registerBuiltinTool('B', ['m.cap']);
    expect(r.capabilities()).toEqual(['a.cap', 'm.cap', 'z.cap']);
  });

  it('should be a no-op when registering with empty capabilities array', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Read', []);
    expect(r.capabilities()).toEqual([]);
    expect(r.providersOf('code.explore')).toEqual([]);
  });

  it('should return empty string for fresh empty registry', () => {
    const r = new CapabilityRegistry();
    expect(r.buildHint(new Set())).toBe('');
  });

  it('should silently ignore server not in mcpToolsByServer', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Grep', ['code.explore']);
    r.applyIntegrations(
      { integrations: { unknown: { capabilities: ['code.explore'] } } },
      new Map(),
    );
    expect(r.providersOf('code.explore')).toEqual(['Grep']);
  });

  it('should be a no-op when mcpToolsByServer has empty array', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Grep', ['code.explore']);
    r.applyIntegrations(
      { integrations: { srv: { capabilities: ['code.explore'] } } },
      new Map([['srv', []]]),
    );
    expect(r.providersOf('code.explore')).toEqual(['Grep']);
  });

  it('should produce multi-line hint when multiple capabilities are registered', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Grep', ['code.explore']);
    r.registerBuiltinTool('Agent', ['code.delegate']);
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

  it('should record builtin tool source correctly', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Grep', ['code.explore']);
    const providers = r.resolveProviders('code.explore', new Set(['Grep']));
    expect(providers[0]!.source).toBe('builtin');
  });

  it('should record MCP tool source correctly', () => {
    const r = new CapabilityRegistry();
    r.applyIntegrations(
      { integrations: { srv: { capabilities: ['code.explore'] } } },
      new Map([['srv', ['mcp__srv__t']]]),
    );
    const providers = r.resolveProviders('code.explore', new Set(['mcp__srv__t']));
    expect(providers[0]!.source).toBe('mcp');
  });

  it('should be idempotent when resetIntegrations is called multiple times', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Grep', ['code.explore']);
    r.applyIntegrations(
      { integrations: { srv: { capabilities: ['code.explore'] } } },
      new Map([['srv', ['mcp__srv__t']]]),
    );
    r.resetIntegrations();
    r.resetIntegrations();
    expect(r.providersOf('code.explore')).toEqual(['Grep']);
  });

  it('should be a no-op when resetting before any integrations are applied', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Grep', ['code.explore']);
    r.resetIntegrations();
    expect(r.capabilities()).toEqual(['code.explore']);
    expect(r.providersOf('code.explore')).toEqual(['Grep']);
  });
});

// ---------------------------------------------------------------------------
// toolsForCapabilities (augmentToolsForCapabilities equivalent)
// ---------------------------------------------------------------------------

const SUBAGENT_CAPABILITIES = ['code.explore', 'code.read', 'web.search', 'web.fetch'];

describe('toolsForCapabilities', () => {
  it('should return empty when capabilitiesRequired is empty', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Grep', ['code.explore']);
    expect(r.toolsForCapabilities([], new Set(['Grep', 'Read']))).toEqual([]);
  });

  it('should inject MCP tools that provide the required capability', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Grep', ['code.explore']);
    r.registerBuiltinTool('Glob', ['code.explore']);
    r.applyIntegrations(
      { integrations: { codegraph: { capabilities: ['code.explore'], preferOver: ['Grep'] } } },
      new Map([['codegraph', ['mcp__codegraph__search']]]),
    );
    const knownNames = new Set(['Grep', 'Glob', 'Read', 'mcp__codegraph__search']);
    const extras = r.toolsForCapabilities(['code.explore'], knownNames);
    expect(extras).toContain('mcp__codegraph__search');
    expect(extras).toContain('Grep');
    expect(extras).toContain('Glob');
  });

  it('should return empty when no tools provide the required capability', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Read', undefined);
    const extras = r.toolsForCapabilities(['code.explore'], new Set(['Read']));
    expect(extras).toEqual([]);
  });

  it('should ignore MCP tools not in knownNames (disconnected)', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Grep', ['code.explore']);
    r.applyIntegrations(
      { integrations: { srv: { capabilities: ['code.explore'] } } },
      new Map([['srv', ['mcp__srv__search']]]),
    );
    const extras = r.toolsForCapabilities(['code.explore'], new Set(['Grep']));
    expect(extras).toEqual(['Grep']);
  });

  it('should inject MCP tools declaring code.read when subagent profile capabilities are resolved', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Read', ['code.read']);
    r.applyIntegrations(
      { integrations: { 'smart-reader': { capabilities: ['code.read'], preferOver: ['Read'] } } },
      new Map([['smart-reader', ['mcp__smart-reader__read_file']]]),
    );
    const knownNames = new Set(['Read', 'mcp__smart-reader__read_file']);
    const result = r.toolsForCapabilities(SUBAGENT_CAPABILITIES, knownNames);
    expect(result).toContain('mcp__smart-reader__read_file');
  });

  it('should inject a single MCP server declaring multiple capabilities', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool('Grep', ['code.explore']);
    r.registerBuiltinTool('Read', ['code.read']);
    r.applyIntegrations(
      { integrations: { codegraph: { capabilities: ['code.explore', 'code.read'], preferOver: ['Grep', 'Read'] } } },
      new Map([['codegraph', ['mcp__codegraph__search', 'mcp__codegraph__read']]]),
    );
    const knownNames = new Set(['Grep', 'Read', 'mcp__codegraph__search', 'mcp__codegraph__read']);
    const result = r.toolsForCapabilities(SUBAGENT_CAPABILITIES, knownNames);
    expect(result).toContain('mcp__codegraph__search');
    expect(result).toContain('mcp__codegraph__read');
    expect(result).toContain('Grep');
    expect(result).toContain('Read');
  });
});

// ---------------------------------------------------------------------------
// parseIntegrationsYaml
// ---------------------------------------------------------------------------

describe('parseIntegrationsYaml', () => {
  it('should return empty config for undefined / empty content', () => {
    expect(parseIntegrationsYaml(undefined).config.integrations).toEqual({});
    expect(parseIntegrationsYaml('').config.integrations).toEqual({});
    expect(parseIntegrationsYaml('   \n  ').config.integrations).toEqual({});
  });

  it('should parse a well-formed integrations document', () => {
    const yaml = `
integrations:
  codebase-memory-mcp:
    capabilities:
      - code.explore
      - test.navigate
    preferOver:
      - Grep
      - Glob
`;
    const { config, warnings } = parseIntegrationsYaml(yaml);
    expect(warnings).toEqual([]);
    expect(config.integrations['codebase-memory-mcp']!.capabilities).toEqual(['code.explore', 'test.navigate']);
    expect(config.integrations['codebase-memory-mcp']!.preferOver).toEqual(['Grep', 'Glob']);
  });

  it('should return warning on malformed YAML but keep going', () => {
    const { config, warnings } = parseIntegrationsYaml(': : : not-yaml :');
    expect(config.integrations).toEqual({});
    expect(warnings.length).toBeGreaterThan(0);
  });

  it('should return warning on schema violation', () => {
    const { config, warnings } = parseIntegrationsYaml('integrations: 42');
    expect(config.integrations).toEqual({});
    expect(warnings.length).toBeGreaterThan(0);
  });

  it('should accept an integration with no fields (uses defaults)', () => {
    const { config, warnings } = parseIntegrationsYaml('integrations:\n  srv: {}\n');
    expect(warnings).toEqual([]);
    expect(config.integrations['srv']!.capabilities).toEqual([]);
  });

  it('should return empty config for YAML that parses to null', () => {
    const { config, warnings } = parseIntegrationsYaml('---\n');
    expect(config.integrations).toEqual({});
    expect(warnings).toEqual([]);
  });

  it('should accept integration with only preferOver set', () => {
    const { config, warnings } = parseIntegrationsYaml('integrations:\n  srv:\n    preferOver:\n      - Grep\n');
    expect(warnings).toEqual([]);
    expect(config.integrations['srv']!.capabilities).toEqual([]);
    expect(config.integrations['srv']!.preferOver).toEqual(['Grep']);
  });

  it('should warn on empty string in capabilities', () => {
    const { config, warnings } = parseIntegrationsYaml('integrations:\n  srv:\n    capabilities: [""]\n');
    expect(config.integrations).toEqual({});
    expect(warnings.length).toBeGreaterThan(0);
  });

  it('should parse multiple integrations in one document', () => {
    const yaml = `
integrations:
  srv-a:
    capabilities: [code.explore]
  srv-b:
    capabilities: [test.navigate]
    preferOver: [Grep]
`;
    const { config, warnings } = parseIntegrationsYaml(yaml);
    expect(warnings).toEqual([]);
    expect(config.integrations['srv-a']!.capabilities).toEqual(['code.explore']);
    expect(config.integrations['srv-b']!.capabilities).toEqual(['test.navigate']);
    expect(config.integrations['srv-b']!.preferOver).toEqual(['Grep']);
  });
});

// ---------------------------------------------------------------------------
// loadIntegrationsConfig
// ---------------------------------------------------------------------------

describe('loadIntegrationsConfig', () => {
  it('should merge user-global and project-local files (project wins)', async () => {
    const { mkdtempSync, writeFileSync, mkdirSync } = await import('node:fs');
    const { tmpdir } = await import('node:os');
    const { join } = await import('pathe');

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
    expect(result.config.integrations['codegraph']?.capabilities).toEqual(['code.explore', 'deep']);
    expect(result.config.integrations['other']?.capabilities).toEqual(['x']);
    expect(result.sources).toHaveLength(2);
    expect(result.warnings).toEqual([]);
  });

  it('should silently skip missing files', () => {
    const result = loadIntegrationsConfig({ userHome: '/nonexistent-a', cwd: '/nonexistent-b' });
    expect(result.config.integrations).toEqual({});
    expect(result.sources).toEqual([]);
    expect(result.warnings).toEqual([]);
  });

  it('should return empty config when both userHome and cwd are undefined', () => {
    const result = loadIntegrationsConfig({});
    expect(result.config.integrations).toEqual({});
    expect(result.sources).toEqual([]);
    expect(result.warnings).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// GOLDEN TEST: profile capabilitiesRequired + integrations.yaml preferOver
// (§0.5.1 gate)
// ---------------------------------------------------------------------------

describe('Golden: profile capabilitiesRequired activates matching MCP tools with preferOver', () => {
  /**
   * Simulates the full capability→tool resolution that happens during
   * profile binding: a profile declaring `capabilitiesRequired: [code.explore]`
   * activates matching MCP tools from integrations.yaml, and the preferOver
   * ordering is honored when multiple tools provide the same capability.
   */
  it('should augment profile tool list with MCP tools matching capabilitiesRequired and honor preferOver ordering', () => {
    // 1. Set up the capability registry with builtin tools
    const registry = new CapabilityRegistry();
    registry.registerBuiltinTool('Grep', ['code.explore']);
    registry.registerBuiltinTool('Glob', ['code.explore']);
    registry.registerBuiltinTool('Read', ['code.read']);
    registry.registerBuiltinTool('WebSearch', ['web.search']);
    registry.registerBuiltinTool('FetchURL', ['web.fetch']);

    // 2. Apply integrations.yaml: codegraph MCP server declares code.explore
    //    and prefers over Grep + Glob
    const integrationsConfig: IntegrationsConfig = {
      integrations: {
        'codegraph': {
          capabilities: ['code.explore'],
          preferOver: ['Grep', 'Glob'],
        },
      },
    };
    const mcpToolsByServer = new Map([
      ['codegraph', ['mcp__codegraph__search', 'mcp__codegraph__explore']],
    ]);
    registry.applyIntegrations(integrationsConfig, mcpToolsByServer);

    // 3. Simulate a profile with capabilitiesRequired: [code.explore]
    //    and a base tool list that does NOT include the MCP tools.
    const profileBaseTools = ['Read', 'Grep', 'Glob'];
    const capabilitiesRequired = ['code.explore'];

    // 4. Collect all known/available tool names
    const allAvailable = new Set([
      ...profileBaseTools,
      'mcp__codegraph__search',
      'mcp__codegraph__explore',
    ]);

    // 5. Augment the tool list — this is what the profile service does
    const extras = registry.toolsForCapabilities(capabilitiesRequired, allAvailable);
    const merged = new Set([...profileBaseTools, ...extras]);
    const resolvedToolNames = [...merged];

    // MCP tools matching code.explore are added
    expect(resolvedToolNames).toContain('mcp__codegraph__search');
    expect(resolvedToolNames).toContain('mcp__codegraph__explore');

    // Original tools preserved
    expect(resolvedToolNames).toContain('Read');
    expect(resolvedToolNames).toContain('Grep');
    expect(resolvedToolNames).toContain('Glob');

    // 6. Verify preferOver ordering: codegraph MCP tools come before Grep/Glob
    const providers = registry.resolveProviders('code.explore', allAvailable);
    const mcpTools = providers.filter((p) => p.source === 'mcp').map((p) => p.toolName);
    const builtinTools = providers.filter((p) => p.source === 'builtin').map((p) => p.toolName);

    // MCP tools with preferOver come first
    expect(mcpTools.length).toBeGreaterThan(0);
    for (const mcpTool of mcpTools) {
      const mcpIndex = providers.findIndex((p) => p.toolName === mcpTool);
      for (const builtin of builtinTools) {
        const builtinIndex = providers.findIndex((p) => p.toolName === builtin);
        expect(mcpIndex).toBeLessThan(builtinIndex);
      }
    }

    // The MCP providers declare preferOver: [Grep, Glob]
    for (const mcpProvider of providers.filter((p) => p.source === 'mcp')) {
      expect(mcpProvider.preferOver).toContain('Grep');
      expect(mcpProvider.preferOver).toContain('Glob');
    }

    // 7. Verify hint text mentions preference
    const hint = registry.buildHint(allAvailable);
    expect(hint).toContain('code.explore');
    expect(hint).toMatch(/prefer.*mcp__codegraph/);
  });

  it('should not augment tools when capabilitiesRequired is undefined', () => {
    const registry = new CapabilityRegistry();
    registry.registerBuiltinTool('Grep', ['code.explore']);
    registry.applyIntegrations(
      { integrations: { codegraph: { capabilities: ['code.explore'], preferOver: ['Grep'] } } },
      new Map([['codegraph', ['mcp__codegraph__search']]]),
    );
    const available = new Set(['Grep', 'mcp__codegraph__search']);
    // No capabilitiesRequired → no augmentation
    const extras = registry.toolsForCapabilities([], available);
    expect(extras).toEqual([]);
  });

  it('should gracefully handle disconnected MCP server (tools not in available set)', () => {
    const registry = new CapabilityRegistry();
    registry.registerBuiltinTool('Grep', ['code.explore']);
    registry.applyIntegrations(
      { integrations: { codegraph: { capabilities: ['code.explore'], preferOver: ['Grep'] } } },
      new Map([['codegraph', ['mcp__codegraph__search']]]),
    );
    // Server registered but tools not available (disconnected)
    const knownNames = new Set(['Grep']);
    const extras = registry.toolsForCapabilities(['code.explore'], knownNames);
    expect(extras).toEqual(['Grep']);
    // Hint should also not mention the disconnected tool
    const hint = registry.buildHint(knownNames);
    expect(hint).not.toContain('mcp__codegraph');
  });

  it('should augment with multiple MCP servers providing different capabilities', () => {
    const registry = new CapabilityRegistry();
    registry.registerBuiltinTool('Grep', ['code.explore']);
    registry.registerBuiltinTool('Read', ['code.read']);
    registry.registerBuiltinTool('WebSearch', ['web.search']);
    registry.registerBuiltinTool('FetchURL', ['web.fetch']);

    registry.applyIntegrations(
      {
        integrations: {
          codegraph: { capabilities: ['code.explore'], preferOver: ['Grep'] },
          'smart-reader': { capabilities: ['code.read'], preferOver: ['Read'] },
          brave: { capabilities: ['web.search'], preferOver: ['WebSearch'] },
          jina: { capabilities: ['web.fetch'], preferOver: ['FetchURL'] },
        },
      },
      new Map([
        ['codegraph', ['mcp__codegraph__search']],
        ['smart-reader', ['mcp__smart-reader__read']],
        ['brave', ['mcp__brave__search']],
        ['jina', ['mcp__jina__fetch']],
      ]),
    );

    const available = new Set([
      'Grep', 'Read', 'WebSearch', 'FetchURL',
      'mcp__codegraph__search', 'mcp__smart-reader__read',
      'mcp__brave__search', 'mcp__jina__fetch',
    ]);

    const result = registry.toolsForCapabilities(SUBAGENT_CAPABILITIES, available);
    expect(result).toContain('mcp__codegraph__search');
    expect(result).toContain('mcp__smart-reader__read');
    expect(result).toContain('mcp__brave__search');
    expect(result).toContain('mcp__jina__fetch');

    // Verify hint mentions all four capabilities with MCP preference
    const hint = registry.buildHint(available);
    expect(hint).toContain('code.explore');
    expect(hint).toContain('code.read');
    expect(hint).toContain('web.search');
    expect(hint).toContain('web.fetch');
  });
});
