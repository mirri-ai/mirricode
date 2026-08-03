/**
 * `capability` domain — `ICapabilityResolverService` contract.
 *
 * Agent-scoped service that owns the `CapabilityRegistry`, registers
 * builtin-tool capabilities from contributions, applies integrations.yaml
 * entries for connected MCP servers, and resolves `capabilitiesRequired`
 * declared by agent profiles into concrete tool names.
 */

import { createDecorator } from '#/_base/di/instantiation';

export interface ICapabilityResolverService {
  readonly _serviceBrand: undefined;

  /** Register a builtin/user tool's capabilities into the registry. */
  registerBuiltinToolCapabilities(name: string, capabilities: readonly string[] | undefined): void;

  /** Apply an integrations.yaml config. Called on MCP connect / reload. */
  applyIntegrations(
    mcpToolsByServer: ReadonlyMap<string, readonly string[]>,
  ): void;

  /**
   * Extend a base tool-name list with tool names that satisfy any of the
   * given capabilities. Used by profile binding to auto-add discovered
   * MCP tools when a profile declares `capabilitiesRequired`.
   */
  augmentToolNames(
    baseNames: readonly string[] | undefined,
    capabilitiesRequired: readonly string[] | undefined,
    availableToolNames: ReadonlySet<string>,
  ): readonly string[] | undefined;

  /**
   * Produce a capability-preference hint string for the system prompt.
   * Empty when there is nothing worth saying.
   */
  buildHint(availableToolNames: ReadonlySet<string>): string;
}

export const ICapabilityResolverService =
  createDecorator<ICapabilityResolverService>('capabilityResolverService');
