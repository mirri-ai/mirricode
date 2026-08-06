/**
 * `workspace` domain — process-wide catalog of known workspaces.
 *
 * Defines the `IWorkspaceService` used by the program side to remember the
 * folders the user has opened (backed by the app's own persistence). This is
 * a host-side catalog, not a session-scoped description of one Agent's active
 * work directory. App-scoped.
 */

import { createDecorator, type ServiceIdentifier } from '#/_base/di/instantiation';

export interface Workspace {
  readonly id: string;
  readonly root: string;
  readonly name: string;
  readonly createdAt: number;
  readonly lastOpenedAt: number;
}

export interface WorkspaceUpdate {
  readonly name?: string;
}

/** Workspace with its total session count (including archived sessions). */
export interface WorkspaceWithSessionCount extends Workspace {
  readonly sessionCount: number;
}

export interface IWorkspaceService {
  readonly _serviceBrand: undefined;

  list(): Promise<readonly Workspace[]>;
  /**
   * Return the full (un-deduped) catalog snapshot: every known id spelling,
   * including legacy siblings a single physical directory may be split into.
   * Backed by the same process-wide cache as `list`, so it never re-reads
   * `workspaces.json` once the catalog is loaded. Read-side consumers that
   * need every id (alias enumeration) use this instead of re-reading the file.
   */
  getSnapshot(): Promise<{ readonly byId: ReadonlyMap<string, Workspace>; readonly workspaces: readonly Workspace[] }>;
  /**
   * Drop the read cache so the next `list`/`getSnapshot` re-reads the persisted
   * catalog. Manual refresh calls this so an external writer (a v1 daemon
   * sharing the HOME) is observed on the next request.
   */
  invalidateCache(): void;
  /**
   * List workspaces with their session counts in a single pass.
   *
   * Reads `workspaces.json` and `session_index.jsonl` exactly once each,
   * then computes session counts in memory by grouping session-index entries
   * by `workspaceRootKey`. Avoids the N+1 I/O of calling `count()` per
   * workspace — no `state.json` files are read.
   */
  listWithSessionCounts(): Promise<readonly WorkspaceWithSessionCount[]>;
  get(id: string): Promise<Workspace | undefined>;
  createOrTouch(root: string, name?: string): Promise<Workspace>;
  update(id: string, patch: WorkspaceUpdate): Promise<Workspace | undefined>;
  delete(id: string): Promise<void>;
}

export const IWorkspaceService: ServiceIdentifier<IWorkspaceService> =
  createDecorator<IWorkspaceService>('workspaceService');
