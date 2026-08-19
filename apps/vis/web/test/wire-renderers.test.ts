import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import { WIRE_RENDERERS, rendererFor } from '../src/components/wire/renderers';
import { renderHeadline } from '../src/components/wire/WireHeadline';
import type { AgentRecord } from '../src/types';

/** The v2 engine's generated wire manifest is the authoritative list of record
 *  types that can appear in a `wire.jsonl`. Parsing its index keeps this test
 *  honest without importing agent-core-v2 (vis only depends on agent-core). */
function persistedRecordTypesFromV2Manifest(): string[] {
  const manifestPath = fileURLToPath(
    new URL('../../../../packages/agent-core-v2/docs/wire-manifest.d.ts', import.meta.url),
  );
  const indexEntry = /^\/\/\s{3}([\w.]+)\s+\S+\s+persisted\s/;
  return readFileSync(manifestPath, 'utf8')
    .split('\n')
    .map((line) => indexEntry.exec(line)?.[1])
    .filter((type): type is string => type !== undefined);
}

/** Collapse a rendered headline into the plain text a reader would see, so
 *  assertions describe the visible row rather than React's element shape. */
function flattenRenderedText(node: unknown): string {
  if (node === null || node === undefined || typeof node === 'boolean') return '';
  if (typeof node === 'string' || typeof node === 'number') return String(node);
  if (Array.isArray(node)) return node.map(flattenRenderedText).join('');
  const props = (node as { props?: { children?: unknown } }).props;
  if (props === undefined) return Object.values(node as object).map(flattenRenderedText).join('');
  return flattenRenderedText(props.children);
}

/** v2 record types that `AgentRecordEvents` (agent-core, v1) does not declare
 *  yet, so vis cannot register a renderer for them and falls back to the
 *  generic `(unknown record type: …)` row. This list is a known-gap ledger for
 *  the v1/v2 protocol drift: it may only ever shrink. Closing an entry means
 *  adding it to `AgentRecordEvents`, giving it a restore branch, and adding a
 *  renderer — then deleting it from here. */
const RECORD_TYPES_AWAITING_V1_UNION_ENTRY: readonly string[] = [
  'interaction.request',
  'interaction.resolved',
  'interruptionReminder.recorded',
  'plan.revision',
  'profile.bind',
  'task.started',
  'task.terminated',
  'tools.reset_active_tools',
];

describe('wire renderer registry', () => {
  it('should render a headline for every record type the v2 engine persists', () => {
    const unrendered = persistedRecordTypesFromV2Manifest().filter(
      (type) => rendererFor(type) === undefined,
    );

    expect(unrendered).toEqual(RECORD_TYPES_AWAITING_V1_UNION_ENTRY);
  });

  it('should show turn outcome and duration when a turn ends', () => {
    const record = {
      type: 'turn.ended',
      turnId: 25,
      reason: 'completed',
      durationMs: 30511,
      time: 1786430938046,
    } as const satisfies Extract<AgentRecord, { type: 'turn.ended' }>;

    const headline = renderHeadline(record);
    const rendered = flattenRenderedText(headline);

    expect(rendered).toContain('turn 25');
    expect(rendered).toContain('30.5s');
    expect(rendered).toContain('completed');
    expect(rendered).not.toContain('unknown record type');
  });

  it('should fall back to a generic headline when the record type is outside the union', () => {
    const headline = renderHeadline({ type: 'goal.continuation' } as unknown as AgentRecord);

    expect(flattenRenderedText(headline)).toContain('unknown record type: goal.continuation');
  });

  it('should label every registered renderer so no badge renders empty', () => {
    const unlabeled = Object.entries(WIRE_RENDERERS)
      .filter(([, renderer]) => renderer.label.length === 0)
      .map(([type]) => type);

    expect(unlabeled).toEqual([]);
  });
});
