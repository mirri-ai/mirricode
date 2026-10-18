// apps/mirri-web/src/lib/sessionSearch.ts
// Pure mapping for the search dialog's server-backed mode: the rust
// engine's `session.search` literal hits (cross-session title + message
// text) mapped onto the row shape the dialog already renders — so the
// client-side title filter and the server-backed global search share one
// presentation path.

import type { AppSessionSearchHit, AppSessionSearchPage } from '../api/types';
import type { Session } from '../types';
import { formatMessageTime } from './formatMessageTime';

/** One dialog row (the shape the existing client-side filter produces). */
export interface SessionSearchRow {
  session: Session;
  /** Title matched the query (controls title highlighting). */
  inTitle: boolean;
  /** Workspace name matched the query (controls workspace highlighting). */
  inWorkspace: boolean;
  /** Snippet preview under the title (empty when absent). */
  snippetText: string;
}

/** The page size the dialog requests from `session.search` (one page is
 *  the dialog's whole result surface — the cap the engine clamps to). */
export const SESSION_SEARCH_PAGE_SIZE = 50;

/** Whether the query should go to the server (the engine rejects
 *  sub-2-character literal queries with invalid params — the client filter
 *  keeps those local instead of surfacing the error). */
export function isServerSearchableQuery(query: string): boolean {
  return query.trim().length >= 2;
}

/** Map one `session.search` hit onto a dialog row: the hit carries the
 *  session identity + snippet, so the row's `Session` object is a lean
 *  view (id/title/time/workspace) — navigation still works because the
 *  row's id is what `select` emits. Title-role hits highlight the title;
 *  workspace names are not part of the engine's search space. */
export function sessionSearchHitToRow(hit: AppSessionSearchHit): SessionSearchRow {
  const iso = new Date(hit.time).toISOString();
  return {
    session: {
      id: hit.sessionId,
      title: hit.sessionTitle,
      time: formatMessageTime(iso),
      busy: false,
      workspaceId: hit.workspaceId || undefined,
      updatedAt: iso,
    },
    inTitle: hit.role === 'title',
    inWorkspace: false,
    snippetText: hit.snippet,
  };
}

/** Map a whole `session.search` page onto dialog rows. */
export function sessionSearchPageToRows(page: AppSessionSearchPage): SessionSearchRow[] {
  return page.hits.map(sessionSearchHitToRow);
}
