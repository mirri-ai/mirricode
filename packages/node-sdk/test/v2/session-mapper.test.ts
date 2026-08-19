import { describe, expect, it } from 'vitest';

import type { SessionSummary as V1SessionSummary } from '#/types';
import {
  isoToEpochMs,
  v2MetaToSessionMeta,
  v2SummaryToSessionSummary,
} from '#/v2/session-mapper';

describe('v2SummaryToSessionSummary', () => {
  it('should map v2 index summary fields onto the v1 summary and merge filesystem facts', () => {
    const summary = {
      id: 'sess-1',
      workspaceId: 'ws-1',
      cwd: '/work',
      title: 'My Session',
      lastPrompt: 'hello',
      createdAt: 1_720_000_000_000,
      updatedAt: 1_720_000_100_000,
      archived: false,
      custom: { theme: 'dark' },
    } satisfies Parameters<typeof v2SummaryToSessionSummary>[0];

    const result = v2SummaryToSessionSummary(summary, {
      workDir: '/work',
      sessionDir: '/data/sessions/sess-1',
      additionalDirs: ['/extra'],
    });

    expect(result).toEqual({
      id: 'sess-1',
      title: 'My Session',
      lastPrompt: 'hello',
      workDir: '/work',
      sessionDir: '/data/sessions/sess-1',
      createdAt: 1_720_000_000_000,
      updatedAt: 1_720_000_100_000,
      archived: false,
      metadata: { theme: 'dark' },
      additionalDirs: ['/extra'],
    });
  });

  it('should leave metadata undefined when v2 custom is absent', () => {
    const summary = {
      id: 'sess-2',
      workspaceId: 'ws-1',
      createdAt: 1,
      updatedAt: 2,
      archived: false,
    };
    const result = v2SummaryToSessionSummary(summary, {
      workDir: '/work',
      sessionDir: '/data/sessions/sess-2',
    });
    expect(result.metadata).toBeUndefined();
    expect(result.additionalDirs).toBeUndefined();
  });
});

describe('v2MetaToSessionMeta', () => {
  it('should convert epoch-ms timestamps to ISO and cwd to workDir', () => {
    const meta = {
      id: 'sess-3',
      createdAt: 1_720_000_000_000,
      updatedAt: 1_720_000_100_000,
      archived: false,
      cwd: '/work',
    };

    const result = v2MetaToSessionMeta(meta);

    expect(result.createdAt).toBe(new Date(1_720_000_000_000).toISOString());
    expect(result.updatedAt).toBe(new Date(1_720_000_100_000).toISOString());
    expect(result.workDir).toBe('/work');
  });

  it('should fill v1-required defaults when v2 document never set them', () => {
    const meta = { id: 'sess-4', createdAt: 1, updatedAt: 2, archived: false };

    const result = v2MetaToSessionMeta(meta);

    expect(result.title).toBe('');
    expect(result.isCustomTitle).toBe(false);
    expect(result.agents).toEqual({});
    expect(result.custom).toEqual({});
  });

  it('should map v2 agent meta onto v1 defaults for parentless main agent', () => {
    const meta = {
      id: 'sess-5',
      createdAt: 1,
      updatedAt: 2,
      archived: false,
      agents: { main: { type: 'main' as const, homedir: '/home/u' } },
    };

    const result = v2MetaToSessionMeta(meta);

    expect(result.agents['main']).toEqual({
      homedir: '/home/u',
      type: 'main',
      parentAgentId: null,
    });
  });
});

describe('isoToEpochMs', () => {
  it('should convert an ISO timestamp to epoch milliseconds', () => {
    expect(isoToEpochMs('2024-07-03T00:00:00.000Z')).toBe(1_719_964_800_000);
  });
});