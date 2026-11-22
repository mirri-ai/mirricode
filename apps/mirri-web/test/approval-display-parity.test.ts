// apps/mirri-web/test/approval-display-parity.test.ts
// Cross-language contract parity (ADR-011): the same fixture drives the
// rust serialization test and the protocol schema parse test. Every
// canonical wire kind must land on its declared block kind — a new
// display kind without an entry here fails the build instead of
// silently degrading to a blank card.

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import { describe, expect, it } from 'vitest';

import type { AppApprovalRequest } from '../src/api/types';
import { buildApprovalBlock } from '../src/lib/approvalDisplay';

function approval(display: unknown, action = 'execute Write', toolName = 'Write'): AppApprovalRequest {
  return {
    approvalId: 'req-1',
    sessionId: 's1',
    turnId: 1,
    toolCallId: 'c1',
    toolName,
    action,
    display,
    createdAt: '2026-09-16T00:00:00Z',
  };
}

describe('buildApprovalBlock (protocol dialect)', () => {
  it('should map every fixture wire sample to its declared block kind when the shared fixture drives the coverage check', () => {
    const samples: Record<string, unknown>[] = JSON.parse(
      readFileSync(
        fileURLToPath(
          new URL('../../../rust/crates/mirri-core/test-fixtures/tool-input-display.json', import.meta.url),
        ),
        'utf8',
      ),
    );
    const expectedBlockKind: Record<string, string | undefined> = {
      command: 'shell',
      file_io: 'fileop',
      diff: 'diff',
      search: 'search',
      url_fetch: 'url',
      agent_call: 'invocation',
      skill_call: 'invocation',
      todo_list: 'todo',
      // Declared degradation: no dedicated renderer exists and the panel
      // tools never reach an approval; the generic card still carries the
      // raw payload (Task 4).
      background_task: 'generic',
      task_stop: 'invocation',
      plan_review: 'plan_review',
      goal_start: 'goal',
      scheduled_task_create: 'scheduled_task',
      generic: 'generic',
    };
    for (const sample of samples) {
      const kind = String(sample['kind']);
      const block = buildApprovalBlock(approval(sample));
      expect(block.kind, `display kind ${kind}`).toBe(expectedBlockKind[kind]);
    }
  });
});
