/**
 * `capability` domain — central mapping from capability → tool names.
 *
 * Sources:
 *  - Built-in / user tool `capabilities` field (from tool contribution metadata).
 *  - integrations.yaml declarations, which attach capabilities to MCP tools by
 *    server name (qualified names are supplied by the caller via
 *    `mcpToolsByServer` — this class stays decoupled from MCP naming details).
 *
 * Pure data structure; owned and populated by the capability resolver service.
 * Not thread-safe; the agent loop is single-tasked so this is fine.
 */

import type { CapabilityProvider, IntegrationsConfig } from './types';

export class CapabilityRegistry {
  /** capability id → set of tool names providing it. */
  private readonly providers = new Map<string, Set<string>>();
  /** tool name → capability ids it declares. */
  private readonly byTool = new Map<string, Set<string>>();
  /** tool name → tool names it declared preference over (from integrations.yaml). */
  private readonly preferOver = new Map<string, Set<string>>();
  /** tool name → 'builtin' | 'mcp' (source). */
  private readonly sourceByTool = new Map<string, 'builtin' | 'mcp'>();

  /** Wipe integrations-derived MCP entries. Called before re-applying an integrations.yaml. */
  resetIntegrations(): void {
    for (const [tool, source] of this.sourceByTool) {
      if (source === 'mcp') this.forgetTool(tool);
    }
  }

  /**
   * Register a built-in / user tool's capability declarations.
   *
   * Reads the `capabilities` array (may be undefined or empty).
   */
  registerBuiltinTool(name: string, capabilities: readonly string[] | undefined): void {
    if (capabilities === undefined || capabilities.length === 0) return;
    this.addTool(name, capabilities, 'builtin');
  }

  /**
   * Apply an integrations.yaml config. For each declared integration whose
   * name matches an MCP server, attach the declared capabilities to every
   * tool the server exposes (identified by qualified name prefix). Non-matching
   * entries are ignored silently — a server may not be connected yet.
   *
   * `mcpToolsByServer` maps server name → list of qualified tool names.
   */
  applyIntegrations(
    config: IntegrationsConfig,
    mcpToolsByServer: ReadonlyMap<string, readonly string[]>,
  ): void {
    this.resetIntegrations();
    for (const [serverName, spec] of Object.entries(config.integrations)) {
      const qualifiedNames = mcpToolsByServer.get(serverName);
      if (qualifiedNames === undefined || qualifiedNames.length === 0) continue;
      const caps = spec.capabilities ?? [];
      const preferOver = spec.preferOver ?? [];
      for (const qualified of qualifiedNames) {
        if (caps.length > 0) this.addTool(qualified, caps, 'mcp');
        if (preferOver.length > 0) {
          const existing = this.preferOver.get(qualified) ?? new Set<string>();
          for (const n of preferOver) existing.add(n);
          this.preferOver.set(qualified, existing);
        }
      }
    }
  }

  /** Names of tools that provide a capability, in insertion order. */
  providersOf(capability: string): readonly string[] {
    const set = this.providers.get(capability);
    return set === undefined ? [] : [...set];
  }

  /** All capabilities known to the registry. */
  capabilities(): readonly string[] {
    return [...this.providers.keys()].toSorted((a, b) => a.localeCompare(b));
  }

  /**
   * Providers for a capability, annotated with source and preferOver claims.
   * Only includes tools whose names appear in `availableToolNames` (so a
   * disconnected MCP tool does not surface).
   *
   * Ordering is driven by explicit `preferOver` declarations: if provider A
   * declares `preferOver: [B]`, A precedes B in the output. Otherwise the
   * order is the registry's insertion order (stable, no implicit MCP-first).
   */
  resolveProviders(
    capability: string,
    availableToolNames: ReadonlySet<string>,
  ): readonly CapabilityProvider[] {
    const raw = this.providersOf(capability).filter((n) => availableToolNames.has(n));
    // Comparator: if A preferOver-includes B → A before B; symmetric case → tie.
    // Small provider counts in practice; a simple pairwise sort suffices.
    const sorted = raw.slice().toSorted((a, b) => {
      const aOverB = this.preferOver.get(a)?.has(b) === true;
      const bOverA = this.preferOver.get(b)?.has(a) === true;
      if (aOverB && !bOverA) return -1;
      if (bOverA && !aOverB) return 1;
      return 0;
    });
    return sorted.map((name) => ({
      toolName: name,
      source: this.sourceByTool.get(name) ?? 'builtin',
      preferOver: [...(this.preferOver.get(name) ?? [])],
    }));
  }

  /**
   * Produce a compact hint block for injection into the system prompt.
   * Returns an empty string when nothing to say.
   *
   * Format is intentionally short (a few lines) — the model reads it once
   * per turn and just needs the preference order, not prose.
   */
  buildHint(availableToolNames: ReadonlySet<string>): string {
    const lines: string[] = [];
    for (const capability of this.capabilities()) {
      const providers = this.resolveProviders(capability, availableToolNames);
      if (providers.length === 0) continue;
      const primary = providers[0]!;
      const preferOver = new Set<string>(primary.preferOver);
      // Also treat any other providers of the same capability as fallbacks.
      for (const p of providers.slice(1)) preferOver.add(p.toolName);
      const fallbacks = [...preferOver].filter(
        (n) => availableToolNames.has(n) && n !== primary.toolName,
      );
      // Only emit a hint when the primary has a stated preference (either an
      // explicit preferOver, or genuine alternatives it outranks). A lone
      // builtin with no rivals is silence-worthy.
      const hasExplicitPreference = primary.preferOver.length > 0;
      if (hasExplicitPreference || fallbacks.length > 0) {
        const tail = fallbacks.length > 0 ? ` (falls back to: ${fallbacks.join(', ')})` : '';
        lines.push(`- For \`${capability}\`, prefer \`${primary.toolName}\`${tail}.`);
      }
    }
    if (lines.length === 0) return '';
    return ['Tool preference hints (derived from installed integrations):', ...lines].join('\n');
  }

  /** Look up which tool names provide any of the given capabilities. */
  toolsForCapabilities(
    capabilities: readonly string[],
    availableToolNames: ReadonlySet<string>,
  ): readonly string[] {
    const result = new Set<string>();
    for (const cap of capabilities) {
      for (const name of this.providersOf(cap)) {
        if (availableToolNames.has(name)) result.add(name);
      }
    }
    return [...result];
  }

  private addTool(
    name: string,
    capabilities: readonly string[],
    source: 'builtin' | 'mcp',
  ): void {
    this.sourceByTool.set(name, source);
    const capSet = this.byTool.get(name) ?? new Set<string>();
    for (const cap of capabilities) {
      capSet.add(cap);
      const provs = this.providers.get(cap) ?? new Set<string>();
      provs.add(name);
      this.providers.set(cap, provs);
    }
    this.byTool.set(name, capSet);
  }

  private forgetTool(name: string): void {
    const caps = this.byTool.get(name);
    if (caps === undefined) return;
    for (const cap of caps) {
      const provs = this.providers.get(cap);
      if (provs === undefined) continue;
      provs.delete(name);
      if (provs.size === 0) this.providers.delete(cap);
    }
    this.byTool.delete(name);
    this.preferOver.delete(name);
    this.sourceByTool.delete(name);
  }
}
