import { describe, expect, it } from 'vitest';

import type { AppApprovalRequest } from '../api/types';
import { buildApprovalBlock } from './approvalDisplay';

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
  it('should show the write target path when the engine sends a file_io display', () => {
    const block = buildApprovalBlock(approval({ kind: 'file_io', path: '/tmp/a.md', operation: 'write' }));
    expect(block).toMatchObject({ kind: 'fileop', op: 'write', path: '/tmp/a.md' });
  });

  it('should show the goal objective when the engine sends a goal_start display', () => {
    const block = buildApprovalBlock(
      approval({ kind: 'goal_start', objective: 'ship the release', completion_criterion: 'all tests green' }, 'execute CreateGoal', 'CreateGoal'),
    );
    expect(block).toMatchObject({ kind: 'goal', objective: 'ship the release', completionCriterion: 'all tests green' });
  });

  it('should show the agent name when the engine sends an agent_call display', () => {
    const block = buildApprovalBlock(
      approval({ kind: 'agent_call', agent_name: 'coder', prompt: 'fix it' }, 'execute Agent', 'Agent'),
    );
    expect(block).toMatchObject({ kind: 'invocation', name: 'coder' });
  });

  it('should show the skill name when the engine sends a skill_call display', () => {
    const block = buildApprovalBlock(
      approval({ kind: 'skill_call', skill_name: 'commit' }, 'execute Skill', 'Skill'),
    );
    expect(block).toMatchObject({ kind: 'invocation', name: 'commit' });
  });

  it('should build diff lines from before/after when the engine sends a diff display', () => {
    const block = buildApprovalBlock(approval({ kind: 'diff', path: '/a.rs', before: 'old', after: 'new' }, 'execute Edit', 'Edit'));
    expect(block.kind).toBe('diff');
    const diffBlock = block as { kind: 'diff'; diff: { kind: string }[] };
    expect(diffBlock.diff).toHaveLength(2);
    expect(diffBlock.diff[0]).toMatchObject({ kind: 'rem' });
    expect(diffBlock.diff[1]).toMatchObject({ kind: 'add' });
  });

  it('should show the search query when the engine sends a search display', () => {
    const block = buildApprovalBlock(
      approval({ kind: 'search', query: 'foo', scope: 'src' }, 'execute Grep', 'Grep'),
    );
    expect(block).toMatchObject({ kind: 'search', query: 'foo', scope: 'src' });
  });

  it('should fall back to the generic action summary when the display payload is missing', () => {
    const block = buildApprovalBlock(approval(undefined));
    expect(block).toMatchObject({ kind: 'generic', summary: 'execute Write' });
  });
});