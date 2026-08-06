/**
 * `workspaceAliases` domain — workspace id-spelling resolution contract.
 *
 * Defines the App-scoped `IWorkspaceAliases`: the read-side counterpart to the
 * workspace write-path folding. One physical folder may be addressable by
 * several id spellings (legacy split buckets); this service enumerates them so
 * readers can query every sibling session bucket at once. App-scoped.
 */

import { createDecorator, type ServiceIdentifier } from '#/_base/di/instantiation';

export interface IWorkspaceAliases {
  readonly _serviceBrand: undefined;

  resolveAliasIds(id: string): Promise<readonly string[]>;

  /**
   * Drop the cached parsed views (`session_index.jsonl`). Called by manual
   * `refresh`: external writers (v1 daemon sharing the HOME) are only
   * observed once the cache is cleared and re-read on demand.
   */
  clearCaches(): void;

  /**
   * Load the session-index view into the cache ahead of the first request, so
   * the cold-start first `GET /sessions` never blocks on disk I/O. Best-effort;
   * a failure simply leaves the cache empty and the first request re-reads.
   */
  prewarm(): Promise<void>;
}

export const IWorkspaceAliases: ServiceIdentifier<IWorkspaceAliases> =
  createDecorator<IWorkspaceAliases>('workspaceAliases');
