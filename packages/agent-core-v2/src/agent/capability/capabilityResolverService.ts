/**
 * `capability` domain — `ICapabilityResolverService` implementation.
 *
 * Wraps the `CapabilityRegistry` as an Agent-scoped DI service. Loads
 * integrations.yaml from user-global and project-local paths, applies it
 * to the registry, and provides the `augmentToolNames` resolution used by
 * profile binding. Bound at Agent scope.
 */

import { LifecycleScope, ScopeActivation, registerScopedService } from '#/_base/di/scope';
import { IBootstrapService } from '#/app/bootstrap/bootstrap';
import { IHostEnvironment } from '#/os/interface/hostEnvironment';

import { CapabilityRegistry } from './registry';
import { loadIntegrationsConfig } from './loader';
import type { IntegrationsConfig } from './types';
import { ICapabilityResolverService } from './capabilityResolver';

export class CapabilityResolverService implements ICapabilityResolverService {
  declare readonly _serviceBrand: undefined;

  private readonly registry = new CapabilityRegistry();

  constructor(
    @IBootstrapService private readonly bootstrap: IBootstrapService,
    @IHostEnvironment private readonly env: IHostEnvironment,
  ) {}

  registerBuiltinToolCapabilities(name: string, capabilities: readonly string[] | undefined): void {
    this.registry.registerBuiltinTool(name, capabilities);
  }

  applyIntegrations(
    mcpToolsByServer: ReadonlyMap<string, readonly string[]>,
  ): void {
    const result = loadIntegrationsConfig({
      userHome: this.env.homeDir,
      cwd: this.bootstrap.cwd,
    });
    // Warnings are intentionally not surfaced here — the capability resolver
    // is a data structure, not a UI component. The v1 equivalent logged
    // warnings through `agent.log.warn`; that integration will happen at a
    // higher layer when needed.
    this.registry.applyIntegrations(result.config, mcpToolsByServer);
  }

  /**
   * Directly apply a parsed integrations config to the registry.
   * Intended for hot-reload and tests.
   */
  applyIntegrationsConfig(
    config: IntegrationsConfig,
    mcpToolsByServer: ReadonlyMap<string, readonly string[]>,
  ): void {
    this.registry.applyIntegrations(config, mcpToolsByServer);
  }

  augmentToolNames(
    baseNames: readonly string[] | undefined,
    capabilitiesRequired: readonly string[] | undefined,
    availableToolNames: ReadonlySet<string>,
  ): readonly string[] | undefined {
    if (capabilitiesRequired === undefined || capabilitiesRequired.length === 0) {
      return baseNames;
    }
    const extras = this.registry.toolsForCapabilities(capabilitiesRequired, availableToolNames);
    if (extras.length === 0) return baseNames;
    if (baseNames === undefined) return [...extras];
    const merged = new Set(baseNames);
    for (const name of extras) merged.add(name);
    return [...merged];
  }

  buildHint(availableToolNames: ReadonlySet<string>): string {
    return this.registry.buildHint(availableToolNames);
  }

  /** Expose the underlying registry for tests. */
  get innerRegistry(): CapabilityRegistry {
    return this.registry;
  }
}

registerScopedService(
  LifecycleScope.Agent,
  ICapabilityResolverService,
  CapabilityResolverService,
  ScopeActivation.OnScopeCreated,
  'capability',
);
