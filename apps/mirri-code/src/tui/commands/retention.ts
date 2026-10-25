/**
 * Session retention management — the terminal's minimal equivalent of the
 * web management page (SDR-007's terminal-scope ruling): a per-session
 * picker over the engine's `retention.*` data plane offering restore-from-
 * archive and delete. Batch tiers / stats / policy stay on the web page
 * (the accepted product trade-off).
 */

import { ErrorCodes, isMirriError, type RetentionListEntry } from '@mirri-ai/mirri-code-sdk';

import { ChoicePickerComponent, type ChoiceOption } from '../components/dialogs/choice-picker';
import { formatErrorMessage } from '../utils/event-payload';
import type { SlashCommandHost } from './dispatch';

const LEGACY_ENGINE_MESSAGE =
  'The legacy engine does not support session retention management. Use the web settings page (mirri web).';

function isNotImplementedError(error: unknown): boolean {
  return isMirriError(error) && error.code === ErrorCodes.NOT_IMPLEMENTED;
}

/** A flat row the picker renders — the wire entry plus a display title. */
export interface RetentionPickerRow {
  readonly session_id: string;
  readonly title: string;
  readonly compressed: boolean;
  readonly archived_at?: string | undefined;
  readonly compressed_days?: number | undefined;
}

/** Project one wire entry onto the picker row (display fields only). */
export function retentionRowsForPicker(
  entries: readonly RetentionListEntry[],
): RetentionPickerRow[] {
  return entries.map((entry) => ({
    session_id: entry.session_id,
    title: entry.title.trim().length > 0 ? entry.title : entry.session_id,
    compressed: entry.compressed_at !== undefined && entry.compressed_at !== null,
    archived_at: entry.archived_at,
    compressed_days: entry.compressed_days,
  }));
}

/** One picker option's label: "title · id [· compressed N d]". */
export function retentionRowLabel(row: RetentionPickerRow): string {
  const parts: string[] = [row.title, row.session_id];
  if (row.compressed) {
    const days = row.compressed_days !== undefined ? ` ${String(row.compressed_days)}d` : '';
    parts.push(`compressed${days}`);
  }
  return parts.join(' · ');
}

/**
 * The retained-session browser: the engine's archive and compressed views
 * merged into one list (a compressed session is unreachable from the
 * archived-only view), let the user pick one, then act on it (restore from
 * archive / delete with confirmation). Invoked from the session picker's
 * Ctrl+R and the `/retention` command.
 */
export async function handleRetentionCommand(host: SlashCommandHost): Promise<void> {
  // The two views are independent engine queries — fetch them in parallel
  // so /retention stays one round-trip deep. A session can appear in both
  // (archived earlier, compressed later); the archived entry wins and the
  // duplicate is dropped, because its archived_at carries the extra context.
  let items: readonly RetentionListEntry[];
  try {
    const [archived, compressed] = await Promise.all([
      host.harness.listRetentionSessions('archived'),
      host.harness.listRetentionSessions('compressed'),
    ]);
    const seen = new Set(archived.items.map((entry) => entry.session_id));
    items = [...archived.items, ...compressed.items.filter((entry) => !seen.has(entry.session_id))];
  } catch (error) {
    host.showError(
      isNotImplementedError(error) ? LEGACY_ENGINE_MESSAGE : formatErrorMessage(error),
    );
    return;
  }

  if (items.length === 0) {
    host.showStatus('No archived sessions.');
    return;
  }

  const rows = retentionRowsForPicker(items);
  const options: ChoiceOption[] = rows.map((row) => ({
    value: row.session_id,
    label: retentionRowLabel(row),
  }));

  host.mountEditorReplacement(
    new ChoicePickerComponent({
      title: 'Retained sessions — manage',
      hint: 'Enter manage · Esc cancel',
      options,
      searchable: true,
      onSelect: (sessionId) => {
        const row = rows.find((candidate) => candidate.session_id === sessionId);
        if (row === undefined) return;
        void showSessionActions(host, row);
      },
      onCancel: () => {
        host.restoreEditor();
      },
    }),
  );
}

/** The per-session actions: restore from archive, or delete with confirmation. */
async function showSessionActions(host: SlashCommandHost, row: RetentionPickerRow): Promise<void> {
  const actions: ChoiceOption[] = [
    {
      value: 'restore',
      label: 'Restore from archive',
      description: 'Un-archive the session; it returns to the normal session list.',
    },
    {
      value: 'delete',
      label: 'Delete permanently',
      tone: 'danger',
      description: 'Removes the session and its exclusive attachments. Cannot be undone.',
    },
  ];

  host.mountEditorReplacement(
    new ChoicePickerComponent({
      title: retentionRowLabel(row),
      hint: 'Enter confirm · Esc back',
      options: actions,
      onSelect: (action) => {
        if (action === 'restore') {
          void restoreSession(host, row);
        } else {
          void confirmDelete(host, row);
        }
      },
      onCancel: () => {
        void handleRetentionCommand(host);
      },
    }),
  );
}

/** Restore one session from archive (`session.restore` = resume + flag clear). */
async function restoreSession(host: SlashCommandHost, row: RetentionPickerRow): Promise<void> {
  try {
    await host.harness.restoreRetentionSession(row.session_id);
    host.showStatus(`Session "${row.title}" restored from archive.`);
  } catch (error) {
    host.showError(formatErrorMessage(error));
  }
}

/** The delete confirmation — one explicit step, never a single keypress. */
async function confirmDelete(host: SlashCommandHost, row: RetentionPickerRow): Promise<void> {
  host.mountEditorReplacement(
    new ChoicePickerComponent({
      title: 'Delete session permanently?',
      notice: `This removes "${row.title}" and its exclusive attachments. This cannot be undone.`,
      noticeTone: 'warning',
      options: [
        { value: 'confirm', label: 'Delete', tone: 'danger' },
        { value: 'cancel', label: 'Cancel' },
      ],
      onSelect: (value) => {
        if (value === 'confirm') {
          void deleteSession(host, row);
        } else {
          void showSessionActions(host, row);
        }
      },
      onCancel: () => {
        void showSessionActions(host, row);
      },
    }),
  );
}

async function deleteSession(host: SlashCommandHost, row: RetentionPickerRow): Promise<void> {
  try {
    const result = await host.harness.deleteRetentionSession(row.session_id);
    const reclaimed = result.reclaimed_bytes;
    host.showStatus(
      reclaimed > 0
        ? `Session "${row.title}" deleted (${formatBytes(reclaimed)} reclaimed).`
        : `Session "${row.title}" deleted.`,
    );
  } catch (error) {
    host.showError(formatErrorMessage(error));
  }
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${String(bytes)} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}
