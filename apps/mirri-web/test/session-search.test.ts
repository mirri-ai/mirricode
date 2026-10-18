// apps/mirri-web/test/session-search.test.ts
// Given the rust engine's `session.search` literal hits (snake_case wire,
// title + message text across sessions, ~80-char snippet windows), the pure
// sessionSearch lib must map them onto the search dialog's row shape —
// lean Session views (id/title/time/workspace) for navigation, the
// server-side snippet for the preview, title-role hits flagging title
// highlighting — and must only send server queries the engine accepts
// (sub-2-character literal queries answer invalid_params).

import { describe, expect, it } from 'vitest';

import {
  isServerSearchableQuery,
  sessionSearchHitToRow,
  sessionSearchPageToRows,
} from '../src/lib/sessionSearch';
import type { AppSessionSearchPage } from '../src/api/types';

describe('sessionSearch hit mapping', () => {
  it('should map a message hit onto a navigable row when the hit is user-role', () => {
    // Given a user-role message hit from the engine
    const hit = {
      sessionId: 's-1',
      workspaceId: 'ws-1',
      sessionTitle: 'Deploy notes',
      agentId: 'main',
      role: 'user',
      snippet: '…the needle was found…',
      time: 1735689600000,
      turn: 3,
    };

    // When mapped onto a dialog row
    const row = sessionSearchHitToRow(hit);

    // Then the row carries the session identity for navigation, the
    // server-side snippet, and no title/workspace highlighting
    expect(row.session.id).toBe('s-1');
    expect(row.session.title).toBe('Deploy notes');
    expect(row.session.workspaceId).toBe('ws-1');
    expect(row.session.busy).toBe(false);
    expect(row.snippetText).toBe('…the needle was found…');
    expect(row.inTitle).toBe(false);
    expect(row.inWorkspace).toBe(false);
    // And the epoch time becomes a display string plus an ISO updatedAt
    expect(row.session.time).toBeTruthy();
    expect(row.session.updatedAt).toBe(new Date(hit.time).toISOString());
  });

  it('should flag title highlighting when the hit is title-role', () => {
    // Given a title-role hit (title matches carry no turn ordinal)
    const hit = {
      sessionId: 's-2',
      workspaceId: 'ws-1',
      sessionTitle: 'The runbook',
      agentId: '',
      role: 'title',
      snippet: 'The runbook',
      time: 1735689600000,
    };

    // When mapped
    const row = sessionSearchHitToRow(hit);

    // Then title highlighting is on and the empty agent id degrades to an
    // absent workspace id on the lean view
    expect(row.inTitle).toBe(true);
    expect(row.session.workspaceId).toBe('ws-1');
  });

  it('should map a whole page in order when several hits return', () => {
    // Given a page with three hits
    const page: AppSessionSearchPage = {
      mode: 'literal',
      hits: [1, 2, 3].map((n) => ({
        sessionId: `s-${n}`,
        workspaceId: 'ws-1',
        sessionTitle: `Session ${n}`,
        agentId: 'main',
        role: 'user',
        snippet: `hit ${n}`,
        time: 1735689600000 + n,
        turn: n,
      })),
    };

    // When the page maps to rows
    const rows = sessionSearchPageToRows(page);

    // Then every hit becomes a row, in the server's time-desc order
    expect(rows.map((row) => row.session.id)).toEqual(['s-1', 's-2', 's-3']);
  });
});

describe('sessionSearch query gating', () => {
  it('should only offer server search when the query reaches two characters', () => {
    // Given queries of varying lengths
    // When gated for server searchability
    // Then only >=2-char trimmed queries pass (the engine rejects shorter
    // literal queries with invalid_params)
    expect(isServerSearchableQuery('')).toBe(false);
    expect(isServerSearchableQuery('a')).toBe(false);
    expect(isServerSearchableQuery(' a ')).toBe(false);
    expect(isServerSearchableQuery('ab')).toBe(true);
    expect(isServerSearchableQuery(' needle ')).toBe(true);
  });
});
