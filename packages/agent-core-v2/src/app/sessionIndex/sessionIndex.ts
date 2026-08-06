/**
 * `sessionIndex` domain — session index contract.
 *
 * `ISessionIndex` is a domain-specific persistence Store: a backend-neutral
 * query facade over the set of persisted sessions (open or closed). It
 * enumerates sessions and derives session identity (`workspaceId`), returning
 * data (`SessionSummary`) or counts — never filesystem paths or live handles.
 * The index is a read model. Backends are deployment-specific (local
 * filesystem today; database / query store on a server).
 */

import { createDecorator, type ServiceIdentifier } from '#/_base/di/instantiation';
import type { Page } from '#/persistence/interface/queryStore';

export const PARENT_SESSION_ID_KEY = 'parent_session_id';

export const CHILD_SESSION_KIND_KEY = 'child_session_kind';

export const CHILD_SESSION_KIND = 'child';

export interface SessionSummary {
  readonly id: string;
  readonly workspaceId: string;
  readonly cwd?: string;
  readonly title?: string;
  readonly lastPrompt?: string;
  readonly createdAt: number;
  readonly updatedAt: number;
  readonly archived: boolean;
  readonly custom?: Record<string, unknown>;
}

export interface SessionListQuery {
  readonly workspaceIds?: readonly string[];
  readonly sessionId?: string;
  readonly includeArchived?: boolean;
  readonly cursor?: string;
  readonly limit?: number;
  readonly childOf?: string;
}

export interface ISessionIndex {
  readonly _serviceBrand: undefined;

  list(query: SessionListQuery): Promise<Page<SessionSummary>>;
  get(id: string): Promise<SessionSummary | undefined>;
  countActive(workspaceIds: readonly string[]): Promise<number>;
  /**
   * Re-converge the named workspaces from disk into the backing store
   * (upsert new/changed sessions, delete store rows absent from disk).
   * Returns the number of inserted/removed rows. The sidebar manual refresh
   * button calls this; reads themselves never touch disk.
   */
  refreshWorkspace(workspaceIds: readonly string[]): Promise<{ inserted: number; removed: number }>;
}

export const ISessionIndex: ServiceIdentifier<ISessionIndex> =
  createDecorator<ISessionIndex>('sessionIndex');
