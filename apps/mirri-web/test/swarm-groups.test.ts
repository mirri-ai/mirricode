import { describe, expect, it } from 'vitest';
import type { AgentStep, AppTask, NarrativeSegment } from '../src/api/types';
import {
  buildSwarmGroups,
  countSwarmMembers,
  swarmMembersByToolCall,
} from '../src/composables/swarmGroups';
import { buildMemberTimeline } from '../src/lib/swarmCardRows';

function seg(id: string, kind: NarrativeSegment['kind'], text: string, seq: number): NarrativeSegment {
  return { id, kind, text, seq };
}

function step(
  id: string,
  name: string,
  arg: string,
  seq: number,
  status: AgentStep['status'] = 'ok',
): AgentStep {
  return { id, name, arg, status, outputLines: [], seq };
}

function subagentTask(
  id: string,
  parentToolCallId: string | undefined,
  opts: {
    swarmIndex?: number;
    status?: AppTask['status'];
    subagentPhase?: AppTask['subagentPhase'];
    segments?: NarrativeSegment[];
    outputLines?: string[];
    model?: string;
    steps?: AgentStep[];
  } = {},
): AppTask {
  return {
    id,
    sessionId: 'session-1',
    kind: 'subagent',
    description: `subagent ${id}`,
    status: opts.status ?? 'running',
    createdAt: '2026-01-01T00:00:00.000Z',
    parentToolCallId,
    swarmIndex: opts.swarmIndex,
    segments: opts.segments,
    steps: opts.steps,
    outputLines: opts.outputLines,
    subagentPhase: opts.subagentPhase ?? 'working',
    model: opts.model,
  };
}

function bashTask(id: string): AppTask {
  return {
    id,
    sessionId: 'session-1',
    kind: 'bash',
    description: `bash ${id}`,
    status: 'running',
    createdAt: '2026-01-01T00:00:00.000Z',
  };
}

describe('buildSwarmGroups', () => {
  it('emits a group only when two or more members share a swarmIndex', () => {
    const groups = buildSwarmGroups([
      subagentTask('a', 'swarm-1', { swarmIndex: 1 }),
      subagentTask('b', 'swarm-1', { swarmIndex: 2 }),
    ]);
    expect(groups).toHaveLength(1);
    expect(groups[0]?.id).toBe('swarm-1');
    expect(groups[0]?.members.map((m) => m.id)).toEqual(['a', 'b']);
  });

  it('filters single-member groups (used for the badge counter)', () => {
    const groups = buildSwarmGroups([subagentTask('a', 'swarm-1', { swarmIndex: 1 })]);
    expect(groups).toHaveLength(0);
  });

  it('ignores subagents without a swarmIndex', () => {
    const groups = buildSwarmGroups([
      subagentTask('a', 'swarm-1'),
      subagentTask('b', 'swarm-1'),
    ]);
    expect(groups).toHaveLength(0);
  });
});

describe('countSwarmMembers', () => {
  it('counts completed + failed as done across groups', () => {
    const groups = buildSwarmGroups([
      subagentTask('a', 'swarm-1', { swarmIndex: 1, subagentPhase: 'completed', status: 'completed' }),
      subagentTask('b', 'swarm-1', { swarmIndex: 2, subagentPhase: 'failed', status: 'failed' }),
      subagentTask('c', 'swarm-2', { swarmIndex: 1, subagentPhase: 'working' }),
      subagentTask('d', 'swarm-2', { swarmIndex: 2, subagentPhase: 'queued' }),
    ]);
    expect(countSwarmMembers(groups)).toEqual({ done: 2, total: 4 });
  });
});

describe('swarmMembersByToolCall', () => {
  it('keeps single-member swarms so a resume-only AgentSwarm gets live progress', () => {
    const map = swarmMembersByToolCall([subagentTask('a', 'swarm-1', { swarmIndex: 1 })]);
    expect(map.get('swarm-1')?.map((m) => m.id)).toEqual(['a']);
  });

  it('groups every subagent with the same parentToolCallId, ignoring swarmIndex', () => {
    const map = swarmMembersByToolCall([
      subagentTask('b', 'swarm-1'),
      subagentTask('a', 'swarm-1'),
      subagentTask('c', 'swarm-2'),
    ]);
    expect(map.get('swarm-1')?.map((m) => m.id)).toEqual(['a', 'b']);
    expect(map.get('swarm-2')?.map((m) => m.id)).toEqual(['c']);
  });

  it('ignores non-subagent tasks and subagents without a parentToolCallId', () => {
    const map = swarmMembersByToolCall([
      bashTask('b-1'),
      subagentTask('orphan', undefined),
      subagentTask('a', 'swarm-1'),
    ]);
    expect([...map.keys()]).toEqual(['swarm-1']);
  });

  it('should derive each member activity text from the last text segment when segments exist', () => {
    // Given members whose latest narrative segment is text
    const map = swarmMembersByToolCall([
      subagentTask('a', 'swarm-1', {
        segments: [
          seg('a-seg-0', 'thinking', 'planning', 0),
          seg('a-seg-1', 'text', 'line one\nCalling Read: a.ts', 1),
        ],
      }),
      subagentTask('b', 'swarm-1', { outputLines: ['tool line'] }),
    ]);
    const rows = map.get('swarm-1') ?? [];

    // Then the member carries the derived text-segment tail (no legacy
    // flat-field fallback — the row derives straight from segments)
    expect(rows[0]).toMatchObject({ id: 'a', text: 'line one\nCalling Read: a.ts' });
    expect(rows[1]).toMatchObject({ id: 'b', outputLines: ['tool line'] });
  });

  it('should leave the member without a text tail when it has no segments', () => {
    // Given a legacy-engine / snapshot-restored member with no segments (the
    // retired single-field accumulation no longer provides a fallback)
    const map = swarmMembersByToolCall([
      subagentTask('a', 'swarm-1', { outputLines: ['tool line'] }),
    ]);

    // Then the member carries no text tail — the activity line falls through
    // outputLines / summary instead
    expect(map.get('swarm-1')?.[0]).toMatchObject({ id: 'a', outputLines: ['tool line'] });
  });
});

describe('buildSwarmGroups preserves streamed text', () => {
  it('should derive group member text from the last text segment when segments exist', () => {
    // Given two group members whose latest segment is text
    const groups = buildSwarmGroups([
      subagentTask('a', 'swarm-1', {
        swarmIndex: 1,
        segments: [seg('a-seg-0', 'text', 'first line', 0)],
      }),
      subagentTask('b', 'swarm-1', {
        swarmIndex: 2,
        segments: [seg('b-seg-0', 'text', 'second line', 0)],
      }),
    ]);

    // Then each member shows its own derived tail
    expect(groups[0]?.members.map((m) => m.text)).toEqual(['first line', 'second line']);
  });

  it('should leave group members without a text tail when they have no segments', () => {
    // Given group members with only progress lines (the retired single-field
    // accumulation no longer provides a fallback)
    const groups = buildSwarmGroups([
      subagentTask('a', 'swarm-1', { swarmIndex: 1, outputLines: ['first line'] }),
      subagentTask('b', 'swarm-1', { swarmIndex: 2, outputLines: ['second line'] }),
    ]);

    // Then the members degrade to their output lines for the activity fallback
    expect(groups[0]?.members.map((m) => m.outputLines?.at(-1))).toEqual([
      'first line',
      'second line',
    ]);
  });
});

describe('swarm members carry the resolved model', () => {
  it('carries task.model onto each swarm member', () => {
    const map = swarmMembersByToolCall([
      subagentTask('a', 'swarm-1', { swarmIndex: 1, model: 'claude-sonnet' }),
      subagentTask('b', 'swarm-1', { swarmIndex: 2 }),
    ]);
    const rows = map.get('swarm-1') ?? [];
    expect(rows[0]).toMatchObject({ id: 'a', model: 'claude-sonnet' });
    expect(rows[1]).toMatchObject({ id: 'b', model: undefined });
  });

  it('carries task.model into group members so progress rows can show it', () => {
    const groups = buildSwarmGroups([
      subagentTask('a', 'swarm-1', { swarmIndex: 1, model: 'deepseek-v4' }),
      subagentTask('b', 'swarm-1', { swarmIndex: 2 }),
    ]);
    expect(groups[0]?.members.map((m) => m.model)).toEqual(['deepseek-v4', undefined]);
  });
});

describe('swarm members carry the collapsed-row timeline (ADR-012 ①)', () => {
  it('should derive the timeline on the tool-call grouping point from the member steps and segments', () => {
    // Given a subagent whose tools and narrative arrive interleaved
    const map = swarmMembersByToolCall([
      subagentTask('a', 'swarm-1', {
        steps: [step('tc-1', 'Read', '{"path":"a.ts"}', 1)],
        segments: [
          seg('a-seg-0', 'thinking', 'planning', 0),
          seg('a-seg-1', 'text', 'Opening line\nstill streaming', 2),
        ],
      }),
    ]);

    // Then the member carries the same arrival-ordered view the pure
    // derivation produces — tools, text and reasoning on one sequence
    const memberTimeline = map.get('swarm-1')?.[0]?.timeline;
    expect(memberTimeline?.elements.map((e) => [e.kind, e.seq])).toEqual([
      ['thinking', 0],
      ['tool', 1],
      ['text', 2],
    ]);
    expect(memberTimeline?.headTruncated).toBe(false);
  });

  it('should derive the timeline on the group-building point from the member steps and segments', () => {
    // Given the same subagent task fed through both constructor points
    const a = subagentTask('a', 'swarm-1', {
      swarmIndex: 1,
      steps: [step('a-tc-0', 'Grep', '{"pattern":"TODO"}', 0)],
      segments: [seg('a-seg-0', 'text', 'second member line', 1)],
    });
    const b = subagentTask('b', 'swarm-1', { swarmIndex: 2 });

    const groups = buildSwarmGroups([a, b]);
    const inline = swarmMembersByToolCall([a, b]);

    // Then both points derive the identical view — the isomorphic contract,
    // and reference-stable through the memo (ADR-012 ③)
    const grouped = groups[0]?.members[0]?.timeline;
    expect(grouped).toBe(inline.get('swarm-1')?.[0]?.timeline);
    expect(grouped).toEqual(buildMemberTimeline(a.steps, a.segments));
    expect(grouped?.elements.map((e) => e.kind)).toEqual(['tool', 'text']);
  });

  it('should derive an empty timeline when the member has no steps and no segments', () => {
    // Given a member that has only settled progress lines (no live narrative)
    const map = swarmMembersByToolCall([
      subagentTask('a', 'swarm-1', { outputLines: ['tool output line'] }),
    ]);

    // Then the timeline is present but empty — an explicit "nothing arrived",
    // not a missing source
    expect(map.get('swarm-1')?.[0]?.timeline).toEqual({ elements: [], headTruncated: false });
  });

  it('should still derive the timeline when the member is terminal and not fake a running step status', () => {
    // Given a completed member whose last step finished ok
    const map = swarmMembersByToolCall([
      subagentTask('a', 'swarm-1', {
        status: 'completed',
        subagentPhase: 'completed',
        steps: [step('tc-1', 'Read', '{"path":"a.ts"}', 0, 'ok')],
        segments: [seg('a-seg-0', 'text', 'Done reading', 1)],
      }),
    ]);

    // Then the timeline still derives from the retained evidence — a settled
    // member keeps showing what it did — and the step status is passed through
    // verbatim (never re-stamped as running to look alive)
    const row = map.get('swarm-1')?.[0];
    expect(row?.phase).toBe('completed');
    expect(row?.timeline.elements.map((e) => [e.kind, e.status])).toEqual([
      ['tool', 'ok'],
      ['text', undefined],
    ]);
  });
});
