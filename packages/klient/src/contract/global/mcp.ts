/**
 * `mcpService` / `mcpConfigService` — the MCP domain wire contracts.
 *
 * The shape source for both engines' MCP wire methods (the running-server
 * view and the layered config read/write surface of
 * `docs/specs/mcp-architecture.md` §4.1). The config entries carry the
 * stored file content verbatim — `${VAR}` environment references are
 * preserved and never resolved to values on this surface (adjudication A5).
 */

import { z } from 'zod';

import { mcpServerConfigSchema, mcpStoredServerConfigSchema, mcpTimeoutMsSchema } from '../mcp.js';
import { noResult } from '../helpers.js';
import type { ServiceContract } from '../types.js';

// ---------------------------------------------------------------------------
// mcpService — the running-server view (`mcp.list` / `mcp.reconnect`)
// ---------------------------------------------------------------------------

/** Which config layer an entry comes from in a layered view. */
export const mcpConfigSourceSchema = z.enum(['user', 'projectRoot', 'project', 'plugin']);

/** One running server as `mcp.list` reports it. */
export const mcpServerInfoSchema = z.object({
  name: z.string(),
  transport: z.string(),
  status: z.enum(['pending', 'connected', 'failed', 'disabled', 'needs-auth']),
  toolCount: z.number().int(),
  error: z.string().optional(),
  /**
   * Which pool owns the connection (the three-pool model of
   * `docs/specs/mcp-architecture.md` §4.2). Optional until every engine
   * reports provenance: the rust engine emits it; TS-side emission lands
   * with the v2 user-pool split.
   */
  pool: z.enum(['user', 'workspace', 'plugin']).optional(),
  /** Which config layer the running entry came from. */
  source: mcpConfigSourceSchema.optional(),
});

/** One advertised tool as `mcp.tools` reports it, with its filter state. */
export const mcpToolSummarySchema = z.object({
  name: z.string(),
  description: z.string().optional(),
  enabled: z.boolean(),
});

/**
 * The `mcp.tools` result — the single-server tool listing (ADR-004):
 * every advertised tool with its merged-config filter verdict, or the
 * explicit `connected: false` shape for a name with no live connection.
 */
export const mcpToolsResultSchema = z.object({
  connected: z.boolean(),
  tools: z.array(mcpToolSummarySchema),
});

/**
 * The `mcp.sync` result (F-6) — the summary of what the reconcile pass
 * applied, by op kind: connects (`added`), disconnects (`removed`),
 * teardown-and-reconnects (`reconnected`), and in-place config refreshes
 * (`refreshed`). The all-zero summary is the idempotent steady state: a
 * config the pools already match reconciles as a no-op.
 */
export const mcpSyncResultSchema = z.object({
  added: z.number().int(),
  removed: z.number().int(),
  reconnected: z.number().int(),
  refreshed: z.number().int(),
});

export const mcpContract = {
  list: { input: z.tuple([]), output: z.array(mcpServerInfoSchema) },
  /**
   * The no-workspace-context global view (spec §4.4): every user-pool entry
   * plus the calling context's workspace-pool entries, each with its
   * pool/source provenance and **no shadowing** — the app-level config
   * page sees every entry in every pool.
   */
  status: { input: z.tuple([]), output: z.array(mcpServerInfoSchema) },
  reconnect: { input: z.tuple([z.string()]), output: noResult },
  /**
   * Single-server tool listing with per-tool filter state (ADR-004):
   * every tool the connected server advertises, each annotated with the
   * merged config's `enabledTools`/`disabledTools` verdict at call time
   * (the raw filter fields only, adjudication A5). A not-connected name
   * answers the explicit `connected: false` shape with an empty array —
   * a named miss, never an empty-list imposture.
   */
  tools: { input: z.tuple([z.string()]), output: mcpToolsResultSchema },
  /**
   * The idempotent reconcile entry (F-6): re-run the single reconcile
   * kernel over both pools and answer the summary of what it applied —
   * the convergence call for manual/external edits to the config layers
   * (config writes and the file watcher ride the same kernel).
   */
  sync: { input: z.tuple([]), output: mcpSyncResultSchema },
} satisfies ServiceContract;

// ---------------------------------------------------------------------------
// mcpConfigService — the layered config read/write surface (`mcp.config.*`)
// ---------------------------------------------------------------------------

/** Write/read scope: the user file, or the workspace's project layer. */
export const mcpConfigScopeSchema = z.enum(['user', 'workspace']);

/** The tool-filter patch `mcp.config.setToolFilter` writes. */
export const mcpToolFilterSchema = z.object({
  enabledTools: z.array(z.string()).optional(),
  disabledTools: z.array(z.string()).optional(),
});

/** The per-server effective state a layered view reports. */
export const mcpEffectiveConfigSchema = z.object({
  enabled: z.boolean(),
  enabledTools: z.array(z.string()).optional(),
  disabledTools: z.array(z.string()).optional(),
  startupTimeoutMs: mcpTimeoutMsSchema.optional(),
  toolTimeoutMs: mcpTimeoutMsSchema.optional(),
});

/**
 * The `mcp.config.test` arguments: `config` for a direct test of a
 * not-yet-saved entry, or `scope` + `name` to test a persisted entry
 * (resolved from the addressed layer and expanded per A5).
 */
export const mcpConfigTestArgsSchema = z.object({
  scope: mcpConfigScopeSchema.optional(),
  name: z.string().optional(),
  config: mcpServerConfigSchema.optional(),
});

/** The `mcp.config.test` result — the test-connection backend's answer. */
export const mcpTestResultSchema = z.object({
  status: z.enum(['ok', 'error']),
  toolCount: z.number().int(),
  error: z.string().optional(),
});

/**
 * The `mcp.config.preview` result (F-4) — the adjudication A5 sole
 * sanctioned outlet for resolved values: the target config with every
 * `${VAR}` / `${env:VAR}` string value expanded against the engine
 * process environment, plus the names of the variables that stayed
 * undefined (they expand to the empty string — the compatible
 * semantics). Outside this response body resolved values appear on no
 * surface: nothing is cached, emitted, or logged.
 */
export const mcpConfigPreviewResultSchema = z.object({
  config: mcpServerConfigSchema,
  undefined_vars: z.array(z.string()),
});

/** One layered-view entry: the stored config plus the effective state. */
export const mcpConfigEntrySchema = z.object({
  name: z.string(),
  source: mcpConfigSourceSchema,
  /**
   * Content-addressed revision of the source layer file (FNV-1a 64 over the
   * raw bytes, hex; `"absent"` when no layer file backs the entry) — the
   * optimistic-lock token `mcp.config.set`/`remove`/`setToolFilter`
   * side-carry as `expectedRevision` (F-3, ADR-005/ADR-008).
   */
  revision: z.string(),
  /** Present on plugin-sourced entries (runtime name `plugin-<id>:<name>`). */
  runtimeName: z.string().optional(),
  /**
   * The stored config verbatim — `${VAR}` references preserved. May be a
   * filter-only override entry (no transport) when the source layer holds
   * an override whose connection family stays with the shadowed layer.
   */
  config: mcpStoredServerConfigSchema,
  effective: mcpEffectiveConfigSchema,
});

export const mcpConfigContract = {
  /**
   * Layered view: no scope = the merged view (each entry annotated with its
   * source layer); `user` = the user file as stored; `workspace` = the
   * project-root + project layers merged.
   */
  list: {
    input: z.tuple([mcpConfigScopeSchema.optional()]),
    output: z.array(mcpConfigEntrySchema),
  },
  set: {
    /**
     * An optional trailing `expectedRevision` makes the write optimistic
     * (F-3): the engine rejects it with `mcp.config.conflict` when the
     * addressed layer no longer carries that revision; omitting it keeps
     * the historical last-writer-wins behavior (N5).
     */
    input: z.tuple([
      mcpConfigScopeSchema,
      z.string(),
      mcpServerConfigSchema,
      z.string().optional(),
    ]),
    output: noResult,
  },
  remove: {
    input: z.tuple([mcpConfigScopeSchema, z.string(), z.string().optional()]),
    output: noResult,
  },
  setToolFilter: {
    /**
     * Target-layer semantics (ADR-006): an absent scope patches the
     * highest-priority *writable* layer defining the name (project
     * local → user); a name defined only in the project root's `.mcp.json`
     * is read-only (A2 — never machine-written); `workspace` patches the
     * project local layer or, when the name is not defined there, creates
     * a **filter-only override entry** (only `enabledTools`/
     * `disabledTools`, no transport — the A7 merge keeps the connection
     * family with the shadowed layer); `user` targets the user file. The
     * trailing `expectedRevision` is the F-3 optimistic lock, meaning for
     * `setToolFilter` exactly what it means for `set`/`remove`.
     */
    input: z.tuple([
      z.string(),
      mcpToolFilterSchema,
      mcpConfigScopeSchema.optional(),
      z.string().optional(),
    ]),
    output: noResult,
  },
  /** Fresh-connect test of a config (direct, or a persisted scope+name). */
  test: {
    input: z.tuple([mcpConfigTestArgsSchema]),
    output: mcpTestResultSchema,
  },
  /**
   * The resolved-value preview (F-4): the config a connect would see —
   * every `${VAR}` reference expanded against the engine environment —
   * plus the undefined-variable list. The adjudication A5 explicit
   * exception and its only outlet: resolved values are legitimate in this
   * response and nowhere else (nothing cached, emitted, or logged).
   */
  preview: {
    input: z.tuple([mcpConfigTestArgsSchema]),
    output: mcpConfigPreviewResultSchema,
  },
} satisfies ServiceContract;
