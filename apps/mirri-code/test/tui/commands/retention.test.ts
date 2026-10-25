import { ErrorCodes, MirriError } from '@mirri-ai/mirri-code-sdk';
import type { RetentionListEntry } from '@mirri-ai/mirri-code-sdk';
import { describe, expect, it, vi } from 'vitest';

import type { SlashCommandHost } from '#/tui/commands/dispatch';
import {
  handleRetentionCommand,
  retentionRowLabel,
  retentionRowsForPicker,
} from '#/tui/commands/retention';

const ENTER = '\r';

function stripAnsi(text: string): string {
  return text.replaceAll(/\u001B\[[0-9;]*m/g, '');
}

interface TestPicker {
  handleInput(data: string): void;
  render(width: number): string[];
}

function wireEntry(overrides: Partial<RetentionListEntry>): RetentionListEntry {
  return {
    session_id: 'ses_a',
    title: 'Archived task',
    workspace: 'ws-1',
    archived_at: '2026-01-01T00:00:00Z',
    compressed_at: null,
    current_size_bytes: 1024,
    saved_bytes: null,
    compressed_days: null,
    ...overrides,
  } as RetentionListEntry;
}

function makeHost(
  views: {
    items?: readonly RetentionListEntry[];
    archived?: { items: readonly RetentionListEntry[] };
    compressed?: { items: readonly RetentionListEntry[] };
  },
  overrides: {
    deleteResult?: { ok: boolean; reclaimed_bytes: number };
    deleteError?: Error;
    listError?: Error;
  } = {},
) {
  const { listError, deleteError } = overrides;
  // A single payload spreads to both views — most cases only care that
  // "the list" is non-empty, not which view served it.
  const both = views.items !== undefined ? { items: views.items } : undefined;
  const empty: { items: readonly RetentionListEntry[] } = { items: [] };
  const archived = views.archived ?? both ?? empty;
  const compressed = views.compressed ?? both ?? empty;
  const harness = {
    listRetentionSessions: listError
      ? vi.fn(async (): Promise<never> => {
          throw listError;
        })
      : vi.fn(async (view: 'archived' | 'compressed') =>
          view === 'archived' ? { items: [...archived.items] } : { items: [...compressed.items] },
        ),
    deleteRetentionSession: deleteError
      ? vi.fn(async (): Promise<never> => {
          throw deleteError;
        })
      : vi.fn(async () => overrides.deleteResult ?? { ok: true, reclaimed_bytes: 4096 }),
    restoreRetentionSession: vi.fn(async () => undefined),
  };
  const host = {
    harness,
    showError: vi.fn(),
    showStatus: vi.fn(),
    mountEditorReplacement: vi.fn(),
    restoreEditor: vi.fn(),
  } as unknown as SlashCommandHost;
  return { host, harness };
}

function mountedPicker(host: SlashCommandHost, call = 0): TestPicker {
  const mock = host.mountEditorReplacement as ReturnType<typeof vi.fn>;
  return mock.mock.calls[call]?.[0] as TestPicker;
}

function renderedPicker(host: SlashCommandHost, call = 0): string {
  return stripAnsi(mountedPicker(host, call).render(100).join('\n'));
}

describe('retentionRowsForPicker', () => {
  it('marks a row compressed only when the wire entry carries compressed_at', () => {
    const rows = retentionRowsForPicker([
      wireEntry({
        session_id: 'ses_compressed',
        compressed_at: '2026-02-01T00:00:00Z',
        compressed_days: 12,
      }),
      wireEntry({ session_id: 'ses_plain' }),
    ]);

    expect(rows.find((row) => row.session_id === 'ses_compressed')?.compressed).toBe(true);
    expect(rows.find((row) => row.session_id === 'ses_plain')?.compressed).toBe(false);
  });

  it('falls back to the session id when the title is blank', () => {
    const rows = retentionRowsForPicker([wireEntry({ session_id: 'ses_x', title: '   ' })]);

    expect(rows[0]?.title).toBe('ses_x');
  });
});

describe('retentionRowLabel', () => {
  it('appends the compressed marker with its day count for compressed rows', () => {
    const label = retentionRowLabel({
      session_id: 'ses_a',
      title: 'Task',
      compressed: true,
      compressed_days: 12,
    });

    expect(label).toBe('Task · ses_a · compressed 12d');
  });

  it('omits the compressed marker for plain rows', () => {
    const label = retentionRowLabel({ session_id: 'ses_a', title: 'Task', compressed: false });

    expect(label).toBe('Task · ses_a');
  });
});

describe('handleRetentionCommand', () => {
  it('given rows in both views, when /retention, then the picker lists every row with its label', async () => {
    const { host } = makeHost({
      archived: {
        items: [wireEntry({ session_id: 'ses_plain', title: 'Recent task' })],
      },
      compressed: {
        items: [
          wireEntry({
            session_id: 'ses_compressed',
            title: 'Old task',
            archived_at: undefined,
            compressed_at: '2026-02-01T00:00:00Z',
            compressed_days: 12,
          }),
        ],
      },
    });

    await handleRetentionCommand(host);

    expect(host.mountEditorReplacement).toHaveBeenCalledOnce();
    const text = renderedPicker(host);
    expect(text).toContain('Old task · ses_compressed · compressed 12d');
    expect(text).toContain('Recent task · ses_plain');
  });

  it('given a session present in both views, when /retention, then it appears once with its archive context', async () => {
    const both = wireEntry({
      session_id: 'ses_both',
      title: 'Archived then compressed',
      archived_at: '2026-01-01T00:00:00Z',
      compressed_at: '2026-02-01T00:00:00Z',
      compressed_days: 12,
    });
    const { host, harness } = makeHost({
      archived: { items: [both] },
      compressed: { items: [{ ...both, archived_at: undefined }] },
    });

    await handleRetentionCommand(host);

    expect(host.mountEditorReplacement).toHaveBeenCalledOnce();
    const text = renderedPicker(host);
    expect(text.match(/ses_both/g)).toHaveLength(1);
    expect(text).toContain('Archived then compressed · ses_both · compressed 12d');
    // Both engine views were consulted in the merge.
    expect(harness.listRetentionSessions).toHaveBeenCalledWith('archived');
    expect(harness.listRetentionSessions).toHaveBeenCalledWith('compressed');
  });

  it('given an engine without the retention plane, when /retention, then the legacy-engine message surfaces', async () => {
    const { host } = makeHost(
      { items: [] },
      {
        listError: new MirriError(
          ErrorCodes.NOT_IMPLEMENTED,
          'SDK listRetentionSessions is not available on this engine',
        ),
      },
    );

    await handleRetentionCommand(host);

    expect(host.showError).toHaveBeenCalledOnce();
    expect(vi.mocked(host.showError).mock.calls[0]?.[0]).toContain('legacy engine');
  });

  it('given both views are empty, when /retention, then the no-archived status surfaces and no picker mounts', async () => {
    const { host } = makeHost({ archived: { items: [] }, compressed: { items: [] } });

    await handleRetentionCommand(host);

    expect(host.showStatus).toHaveBeenCalledWith('No archived sessions.');
    expect(host.mountEditorReplacement).not.toHaveBeenCalled();
  });

  it('given only the compressed view has rows, when /retention, then those rows are still reachable', async () => {
    const { host } = makeHost({
      archived: { items: [] },
      compressed: {
        items: [
          wireEntry({
            session_id: 'ses_c',
            title: 'Cold log',
            archived_at: undefined,
            compressed_at: '2026-02-01T00:00:00Z',
            compressed_days: 30,
          }),
        ],
      },
    });

    await handleRetentionCommand(host);

    expect(host.mountEditorReplacement).toHaveBeenCalledOnce();
    const text = renderedPicker(host);
    expect(text).toContain('Cold log · ses_c · compressed 30d');
  });

  it('given a row is picked, when Enter, then the per-session actions list opens', async () => {
    const { host } = makeHost({
      archived: { items: [wireEntry({ session_id: 'ses_a', title: 'Archived task' })] },
    });

    await handleRetentionCommand(host);
    mountedPicker(host).handleInput(ENTER);

    await vi.waitFor(() => {
      expect(host.mountEditorReplacement).toHaveBeenCalledTimes(2);
    });
    const actions = renderedPicker(host, 1);
    expect(actions).toContain('Restore from archive');
    expect(actions).toContain('Delete permanently');
  });

  it('given restore is chosen, when confirmed, then restoreRetentionSession is called with the session id', async () => {
    const { host, harness } = makeHost({
      archived: { items: [wireEntry({ session_id: 'ses_a', title: 'Archived task' })] },
    });

    await handleRetentionCommand(host);
    mountedPicker(host).handleInput(ENTER); // open the actions

    await vi.waitFor(() => {
      expect(host.mountEditorReplacement).toHaveBeenCalledTimes(2);
    });
    mountedPicker(host, 1).handleInput(ENTER); // the first action: restore

    await vi.waitFor(() => {
      expect(harness.restoreRetentionSession).toHaveBeenCalledWith('ses_a');
    });
    expect(host.showStatus).toHaveBeenCalledWith('Session "Archived task" restored from archive.');
  });

  it('given delete is chosen and confirmed, when confirmed, then deleteRetentionSession is called and the reclaimed bytes surface', async () => {
    const { host, harness } = makeHost(
      { items: [wireEntry({ session_id: 'ses_a', title: 'Archived task' })] },
      { deleteResult: { ok: true, reclaimed_bytes: 2048 } },
    );

    await handleRetentionCommand(host);
    mountedPicker(host).handleInput(ENTER); // open the actions

    await vi.waitFor(() => {
      expect(host.mountEditorReplacement).toHaveBeenCalledTimes(2);
    });
    mountedPicker(host, 1).handleInput('\u001B[B'); // move to Delete permanently
    mountedPicker(host, 1).handleInput(ENTER); // pick delete → the confirmation

    await vi.waitFor(() => {
      expect(host.mountEditorReplacement).toHaveBeenCalledTimes(3);
    });
    mountedPicker(host, 2).handleInput(ENTER); // confirm Delete (the first option)

    await vi.waitFor(() => {
      expect(harness.deleteRetentionSession).toHaveBeenCalledWith('ses_a');
    });
    expect(host.showStatus).toHaveBeenCalledWith(
      'Session "Archived task" deleted (2.0 KB reclaimed).',
    );
  });

  it('given a delete failure, when confirmed, then the error surfaces verbatim', async () => {
    const { host } = makeHost(
      { items: [wireEntry({ session_id: 'ses_a', title: 'Archived task' })] },
      { deleteError: new Error('session is busy') },
    );

    await handleRetentionCommand(host);
    mountedPicker(host).handleInput(ENTER); // open the actions

    await vi.waitFor(() => {
      expect(host.mountEditorReplacement).toHaveBeenCalledTimes(2);
    });
    mountedPicker(host, 1).handleInput('\u001B[B'); // move to Delete permanently
    mountedPicker(host, 1).handleInput(ENTER); // pick delete → the confirmation

    await vi.waitFor(() => {
      expect(host.mountEditorReplacement).toHaveBeenCalledTimes(3);
    });
    mountedPicker(host, 2).handleInput(ENTER); // confirm Delete

    await vi.waitFor(() => {
      expect(host.showError).toHaveBeenCalledOnce();
    });
    expect(String(vi.mocked(host.showError).mock.calls[0]?.[0])).toContain('busy');
  });
});
