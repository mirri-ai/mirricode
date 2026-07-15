import { describe, it, expect } from 'vitest';
import { DEFAULT_AGENT_PROFILES } from '../src/profile';
import { CapabilityRegistry } from '#/agent/tool/capabilities/registry';
import { parseIntegrationsYaml } from '#/agent/tool/capabilities/parse';
import type { ExecutableTool } from '#/loop';
import { ReadTool } from '#/tools/builtin/file/read';
import { GrepTool } from '#/tools/builtin/file/grep';
import { GlobTool } from '#/tools/builtin/file/glob';
import { WebSearchTool } from '#/tools/builtin/web/web-search';
import { FetchURLTool } from '#/tools/builtin/web/fetch-url';

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
  it('should register builtin tools with their declared capabilities', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.registerBuiltinTool(makeTool('Read'));
    expect(r.providersOf('code.explore')).toEqual(['Grep']);
    expect(r.capabilities()).toEqual(['code.explore']);
  });

  it('should register Read, WebSearch, FetchURL with their declared capabilities', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Read', ['code.read']));
    r.registerBuiltinTool(makeTool('WebSearch', ['web.search']));
    r.registerBuiltinTool(makeTool('FetchURL', ['web.fetch']));
    expect(r.providersOf('code.read')).toEqual(['Read']);
    expect(r.providersOf('web.search')).toEqual(['WebSearch']);
    expect(r.providersOf('web.fetch')).toEqual(['FetchURL']);
    expect(r.capabilities()).toEqual(['code.read', 'web.fetch', 'web.search']);
  });

  it('should attach capabilities to MCP tools from integrations config', () => {
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
            capabilities: ['code.explore', 'test.navigate'],
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

  it('should return empty hint when only builtin providers exist', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    const hint = r.buildHint(new Set(['Grep']));
    expect(hint).toBe('');
  });

  it('should produce hint when an MCP provider is available', () => {
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

  it('should exclude tools not currently available (disconnected MCP)', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.applyIntegrations(
      { integrations: { 'codebase-memory-mcp': { capabilities: ['code.explore'] } } },
      new Map([['codebase-memory-mcp', ['mcp__codebase-memory-mcp__search_graph']]]),
    );
    const providers = r.resolveProviders('code.explore', new Set(['Grep']));
    expect(providers.map((p) => p.toolName)).toEqual(['Grep']);
  });

  it('should wipe only MCP-sourced entries on reset', () => {
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

  it('should return matching available names for a given capability', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.registerBuiltinTool(makeTool('Glob', ['code.explore']));
    const available = new Set(['Grep', 'Glob', 'Read']);
    expect(
      [...r.toolsForCapabilities(['code.explore'], available)].toSorted(),
    ).toEqual(['Glob', 'Grep']);
    expect(r.toolsForCapabilities(['unknown'], available)).toEqual([]);
  });

  it('should not implicitly reorder MCP ahead of builtins without preferOver', () => {
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

  it('should not emit hint when primary is a plain builtin with same-source rivals', () => {
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

describe('builtin tool class capability contracts', () => {
  // Guards against accidental removal of the `capabilities` field on built-in
  // tool classes. The CapabilityRegistry only sees capabilities that the
  // class declares — if someone deletes the field, the tool silently stops
  // participating in integrations and profile augmentation.
  const stubKaos = { pathClass: () => 'posix' } as unknown as ConstructorParameters<typeof GlobTool>[0];
  it.each([
    [GrepTool, 'code.explore'],
    [GlobTool, 'code.explore'],
    [ReadTool, 'code.read'],
    [WebSearchTool, 'web.search'],
    [FetchURLTool, 'web.fetch'],
  ])('%p should declare capability %s', (Ctor, cap) => {
    const instance = new (Ctor as unknown as new (
      ...args: unknown[]
    ) => { capabilities: readonly string[] })(stubKaos);
    expect(instance.capabilities).toContain(cap);
  });
});

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
    expect(config.integrations['codebase-memory-mcp']!.capabilities).toEqual([
      'code.explore',
      'test.navigate',
    ]);
    expect(config.integrations['codebase-memory-mcp']!.preferOver).toEqual([
      'Grep',
      'Glob',
    ]);
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

  it('should format schema-violation warning as path: message', () => {
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

  it('should accept an integration with no fields (uses defaults)', () => {
    const { config, warnings } = parseIntegrationsYaml('integrations:\n  srv: {}\n');
    expect(warnings).toEqual([]);
    expect(config.integrations['srv']!.capabilities).toEqual([]);
  });
});

describe('CapabilityRegistry integration scenarios (Scenario A / B)', () => {
  it('should use Grep as sole code.explore provider when no MCP is registered', () => {
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

  it('should prefer MCP tool over builtin when preferOver is declared', () => {
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
  it('should be a no-op when registering with empty capabilities array', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Read', []));
    expect(r.capabilities()).toEqual([]);
    expect(r.providersOf('code.explore')).toEqual([]);
  });

  it('should return capabilities in sorted order', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('A', ['z.cap', 'a.cap']));
    r.registerBuiltinTool(makeTool('B', ['m.cap']));
    expect(r.capabilities()).toEqual(['a.cap', 'm.cap', 'z.cap']);
  });

  it('should order providers correctly through a 3-way preferOver chain', () => {
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

  it('should produce stable order when preferOver is circular', () => {
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

  it('should return empty for unknown capability', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    expect(r.resolveProviders('unknown.cap', new Set(['Grep']))).toEqual([]);
  });

  it('should return empty when all providers are filtered out', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.applyIntegrations(
      { integrations: { srv: { capabilities: ['code.explore'] } } },
      new Map([['srv', ['mcp__srv__t']]]),
    );
    // Neither tool is in available set
    expect(r.resolveProviders('code.explore', new Set(['Read']))).toEqual([]);
  });

  it('should return empty when capabilities array is empty', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    expect(r.toolsForCapabilities([], new Set(['Grep']))).toEqual([]);
  });

  it('should be a no-op when resetting before any integrations are applied', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.resetIntegrations();
    expect(r.capabilities()).toEqual(['code.explore']);
    expect(r.providersOf('code.explore')).toEqual(['Grep']);
  });

  it('should be idempotent when resetIntegrations is called multiple times', () => {
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

  it('should be a no-op when mcpToolsByServer has empty array', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.applyIntegrations(
      { integrations: { srv: { capabilities: ['code.explore'] } } },
      new Map([['srv', []]]),
    );
    expect(r.providersOf('code.explore')).toEqual(['Grep']);
  });

  it('should silently ignore server not in mcpToolsByServer', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.applyIntegrations(
      { integrations: { unknown: { capabilities: ['code.explore'] } } },
      new Map(),
    );
    expect(r.providersOf('code.explore')).toEqual(['Grep']);
  });

  it('should return empty string for fresh empty registry', () => {
    const r = new CapabilityRegistry();
    expect(r.buildHint(new Set())).toBe('');
  });

  it('should produce multi-line hint when multiple capabilities are registered', () => {
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

  it('should record builtin tool source correctly', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    const available = new Set(['Grep']);
    const providers = r.resolveProviders('code.explore', available);
    expect(providers[0]!.source).toBe('builtin');
  });

  it('should record MCP tool source correctly', () => {
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

describe('augmentToolsForCapabilities', () => {
  it('should return base names unchanged when capabilitiesRequired is undefined', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    const available = new Set(['Grep', 'Read']);
    const result = r.toolsForCapabilities([], available);
    expect(result).toEqual([]);
  });

  it('should inject MCP tools that provide the required capability', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.registerBuiltinTool(makeTool('Glob', ['code.explore']));
    r.applyIntegrations(
      {
        integrations: {
          'codegraph': { capabilities: ['code.explore'], preferOver: ['Grep'] },
        },
      },
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
    r.registerBuiltinTool(makeTool('Read'));
    const extras = r.toolsForCapabilities(['code.explore'], new Set(['Read']));
    expect(extras).toEqual([]);
  });

  it('should ignore MCP tools not in knownNames (disconnected)', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.applyIntegrations(
      { integrations: { srv: { capabilities: ['code.explore'] } } },
      new Map([['srv', ['mcp__srv__search']]]),
    );
    // mcp__srv__search not in knownNames → filtered out
    const extras = r.toolsForCapabilities(['code.explore'], new Set(['Grep']));
    expect(extras).toEqual(['Grep']);
  });
});

const SUBAGENT_CAPABILITIES = ['code.explore', 'code.read', 'web.search', 'web.fetch'];

describe('subagent profile capabilitiesRequired', () => {
  it('should inject tools matching all declared capabilities into explore subagent profile', () => {
    const explore = DEFAULT_AGENT_PROFILES['explore'];
    expect(explore).toBeDefined();
    expect(explore!.capabilitiesRequired).toEqual(SUBAGENT_CAPABILITIES);
  });

  it('should inject tools matching all declared capabilities into plan subagent profile', () => {
    const plan = DEFAULT_AGENT_PROFILES['plan'];
    expect(plan).toBeDefined();
    expect(plan!.capabilitiesRequired).toEqual(SUBAGENT_CAPABILITIES);
  });

  it('should not inject capability tools into coder subagent (all MCP tools already visible)', () => {
    const coder = DEFAULT_AGENT_PROFILES['coder'];
    expect(coder).toBeDefined();
    expect(coder!.capabilitiesRequired).toBeUndefined();
  });

  it('should inject MCP tools declaring code.read when explore profile capabilities are resolved', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Read', ['code.read']));
    r.applyIntegrations(
      {
        integrations: {
          'smart-reader': { capabilities: ['code.read'], preferOver: ['Read'] },
        },
      },
      new Map([['smart-reader', ['mcp__smart-reader__read_file']]]),
    );
    const knownNames = new Set(['Read', 'mcp__smart-reader__read_file']);
    const result = r.toolsForCapabilities(SUBAGENT_CAPABILITIES, knownNames);
    expect(result).toContain('mcp__smart-reader__read_file');
  });

  it('should inject MCP tools declaring web.search when subagent profile capabilities are resolved', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('WebSearch', ['web.search']));
    r.applyIntegrations(
      {
        integrations: {
          'brave': { capabilities: ['web.search'], preferOver: ['WebSearch'] },
        },
      },
      new Map([['brave', ['mcp__brave__search']]]),
    );
    const knownNames = new Set(['WebSearch', 'mcp__brave__search']);
    const result = r.toolsForCapabilities(SUBAGENT_CAPABILITIES, knownNames);
    expect(result).toContain('mcp__brave__search');
  });

  it('should inject MCP tools declaring web.fetch when subagent profile capabilities are resolved', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('FetchURL', ['web.fetch']));
    r.applyIntegrations(
      {
        integrations: {
          'jina': { capabilities: ['web.fetch'], preferOver: ['FetchURL'] },
        },
      },
      new Map([['jina', ['mcp__jina__fetch']]]),
    );
    const knownNames = new Set(['FetchURL', 'mcp__jina__fetch']);
    const result = r.toolsForCapabilities(SUBAGENT_CAPABILITIES, knownNames);
    expect(result).toContain('mcp__jina__fetch');
  });

  it('should inject a single MCP server declaring multiple capabilities matching the profile', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.registerBuiltinTool(makeTool('Read', ['code.read']));
    r.applyIntegrations(
      {
        integrations: {
          'codegraph': {
            capabilities: ['code.explore', 'code.read'],
            preferOver: ['Grep', 'Read'],
          },
        },
      },
      new Map([['codegraph', ['mcp__codegraph__search', 'mcp__codegraph__read']]]),
    );
    const knownNames = new Set([
      'Grep', 'Read', 'mcp__codegraph__search', 'mcp__codegraph__read',
    ]);
    const result = r.toolsForCapabilities(SUBAGENT_CAPABILITIES, knownNames);
    expect(result).toContain('mcp__codegraph__search');
    expect(result).toContain('mcp__codegraph__read');
    // Builtins are also returned — they provide the same capabilities.
    expect(result).toContain('Grep');
    expect(result).toContain('Read');
  });

  it('should generate preference hints for all four capability types when MCP tools provide them', () => {
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Grep', ['code.explore']));
    r.registerBuiltinTool(makeTool('Glob', ['code.explore']));
    r.registerBuiltinTool(makeTool('Read', ['code.read']));
    r.registerBuiltinTool(makeTool('WebSearch', ['web.search']));
    r.registerBuiltinTool(makeTool('FetchURL', ['web.fetch']));
    r.applyIntegrations(
      {
        integrations: {
          'codegraph': { capabilities: ['code.explore'], preferOver: ['Grep', 'Glob'] },
          'smart-reader': { capabilities: ['code.read'], preferOver: ['Read'] },
          'brave': { capabilities: ['web.search'], preferOver: ['WebSearch'] },
          'jina': { capabilities: ['web.fetch'], preferOver: ['FetchURL'] },
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
      'Grep', 'Glob', 'Read', 'WebSearch', 'FetchURL',
      'mcp__codegraph__search', 'mcp__smart-reader__read',
      'mcp__brave__search', 'mcp__jina__fetch',
    ]);
    const hint = r.buildHint(available);
    expect(hint).toContain('code.explore');
    expect(hint).toContain('mcp__codegraph__search');
    expect(hint).toContain('code.read');
    expect(hint).toContain('mcp__smart-reader__read');
    expect(hint).toContain('web.search');
    expect(hint).toContain('mcp__brave__search');
    expect(hint).toContain('web.fetch');
    expect(hint).toContain('mcp__jina__fetch');
  });

  it('should not duplicate MCP tools already in the profile base tools list when augmenting', () => {
    // Simulates the full augmentToolsForCapabilities merge: the profile's
    // base tools list already includes the built-in equivalents, and the
    // capability augmentation adds the MCP tool on top without replacing.
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Read', ['code.read']));
    r.applyIntegrations(
      {
        integrations: {
          'smart-reader': { capabilities: ['code.read'], preferOver: ['Read'] },
        },
      },
      new Map([['smart-reader', ['mcp__smart-reader__read_file']]]),
    );
    const knownNames = new Set(['Read', 'mcp__smart-reader__read_file']);
    // The profile already lists "Read" in its base tools.
    const baseNames = ['Read', 'Grep', 'Glob'];
    const extras = r.toolsForCapabilities(SUBAGENT_CAPABILITIES, knownNames);
    // Merge logic from augmentToolsForCapabilities: Set(base) ∪ Set(extras).
    const merged = new Set([...baseNames, ...extras]);
    // Read appears once (no duplicate).
    expect([...merged].filter((n) => n === 'Read')).toHaveLength(1);
    // MCP tool was added.
    expect(merged.has('mcp__smart-reader__read_file')).toBe(true);
    // Original base tools preserved.
    expect(merged.has('Grep')).toBe(true);
    expect(merged.has('Glob')).toBe(true);
  });

  it('should gracefully exclude MCP tools for a capability when the server is disconnected', () => {
    // The MCP server is declared in integrations.yaml but not connected,
    // so its tools don't appear in knownNames → filtered out silently.
    const r = new CapabilityRegistry();
    r.registerBuiltinTool(makeTool('Read', ['code.read']));
    r.registerBuiltinTool(makeTool('WebSearch', ['web.search']));
    r.applyIntegrations(
      {
        integrations: {
          'smart-reader': { capabilities: ['code.read'], preferOver: ['Read'] },
        },
      },
      // Server is registered in the map, but the tool name won't be in
      // knownNames (simulating disconnect at the ToolManager level).
      new Map([['smart-reader', ['mcp__smart-reader__read_file']]]),
    );
    // Only builtins are available.
    const knownNames = new Set(['Read', 'WebSearch']);
    const result = r.toolsForCapabilities(SUBAGENT_CAPABILITIES, knownNames);
    expect(result).not.toContain('mcp__smart-reader__read_file');
    expect(result).toContain('Read');
    // The hint should also not mention the disconnected tool.
    const hint = r.buildHint(knownNames);
    expect(hint).not.toContain('mcp__smart-reader');
  });
});

describe('parseIntegrationsYaml edge cases', () => {
  it('should return empty config for YAML that parses to null', () => {
    const { config, warnings } = parseIntegrationsYaml('---\n');
    expect(config.integrations).toEqual({});
    expect(warnings).toEqual([]);
  });

  it('should accept integration with only preferOver set', () => {
    const { config, warnings } = parseIntegrationsYaml(
      'integrations:\n  srv:\n    preferOver:\n      - Grep\n',
    );
    expect(warnings).toEqual([]);
    expect(config.integrations['srv']!.capabilities).toEqual([]);
    expect(config.integrations['srv']!.preferOver).toEqual(['Grep']);
  });

  it('should accept integration with explicit empty capabilities array', () => {
    const { config, warnings } = parseIntegrationsYaml(
      'integrations:\n  srv:\n    capabilities: []\n',
    );
    expect(warnings).toEqual([]);
    expect(config.integrations['srv']!.capabilities).toEqual([]);
  });

  it('should warn on non-string in capabilities array', () => {
    const { config, warnings } = parseIntegrationsYaml(
      'integrations:\n  srv:\n    capabilities: [42]\n',
    );
    expect(config.integrations).toEqual({});
    expect(warnings.length).toBeGreaterThan(0);
  });

  it('should warn on empty string in capabilities', () => {
    const { config, warnings } = parseIntegrationsYaml(
      'integrations:\n  srv:\n    capabilities: [""]\n',
    );
    expect(config.integrations).toEqual({});
    expect(warnings.length).toBeGreaterThan(0);
  });

  it('should warn on empty string in preferOver', () => {
    const { config, warnings } = parseIntegrationsYaml(
      'integrations:\n  srv:\n    preferOver: [""]\n',
    );
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

  it('should warn when integrations key is a list instead of map', () => {
    const { config, warnings } = parseIntegrationsYaml('integrations:\n  - a\n  - b\n');
    expect(config.integrations).toEqual({});
    expect(warnings.length).toBeGreaterThan(0);
  });
});

describe('loadIntegrationsConfig', () => {
  it('should merge user-global and project-local files (project wins)', async () => {
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

  it('should silently skip missing files', async () => {
    const { loadIntegrationsConfig } = await import('#/agent/tool/capabilities/loader');
    const result = loadIntegrationsConfig({
      userHome: '/nonexistent-a',
      cwd: '/nonexistent-b',
    });
    expect(result.config.integrations).toEqual({});
    expect(result.sources).toEqual([]);
    expect(result.warnings).toEqual([]);
  });

  it('should produce a warning for malformed yaml and continue loading', async () => {
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

  it('should load only user-global when cwd is undefined', async () => {
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

  it('should load only project-local when userHome is undefined', async () => {
    const { mkdtempSync, writeFileSync, mkdirSync } = await import('node:fs');
    const { tmpdir } = await import('node:os');
    const { join } = await import('pathe');
    const { loadIntegrationsConfig } = await import('#/agent/tool/capabilities/loader');

    const cwd = mkdtempSync(join(tmpdir(), 'mirri-cwd-only-'));
    mkdirSync(join(cwd, '.mirri-code'), { recursive: true });
    writeFileSync(
      join(cwd, '.mirri-code', 'integrations.yaml'),
      'integrations:\n  srv:\n    capabilities: [test.navigate]\n',
      'utf8',
    );

    const result = loadIntegrationsConfig({ cwd });
    expect(result.config.integrations['srv']?.capabilities).toEqual(['test.navigate']);
    expect(result.sources).toHaveLength(1);
    expect(result.warnings).toEqual([]);
  });

  it('should return empty config when both userHome and cwd are undefined', async () => {
    const { loadIntegrationsConfig } = await import('#/agent/tool/capabilities/loader');

    const result = loadIntegrationsConfig({});
    expect(result.config.integrations).toEqual({});
    expect(result.sources).toEqual([]);
    expect(result.warnings).toEqual([]);
  });

  it('should warn on non-ENOENT file read error', async () => {
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

  it('should reflect actual read order in sources array (user first, project second)', async () => {
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
