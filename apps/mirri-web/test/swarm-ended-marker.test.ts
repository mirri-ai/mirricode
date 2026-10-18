// apps/mirri-web/test/swarm-ended-marker.test.ts
// 「蜂群已结束」divider 的判定与消息构造（对齐 TUI swarm-markers 'ended'，
// 见 session-event-handler.ts:598-619 的呈现事实；v3 引擎无自动退出，由
// web 端以 主回合结束 + 无活动子代理 等价推导——R-6/R-7）。
import { describe, expect, it } from 'vitest';
import {
  SWARM_ENDED_MARKER_METADATA_KEY,
  shouldAppendSwarmEndedMarker,
  buildSwarmEndedMarkerMessage,
} from '../src/lib/swarmEndedMarker';

describe('shouldAppendSwarmEndedMarker', () => {
  it('Given a task-entry session whose main turn just ended and no running subagents Then append', () => {
    expect(
      shouldAppendSwarmEndedMarker({
        swarmEntry: 'task',
        turnActive: false,
        runningSubagentCount: 0,
      }),
    ).toBe(true);
  });

  it('Given the main turn is still active Then do not append', () => {
    expect(
      shouldAppendSwarmEndedMarker({ swarmEntry: 'task', turnActive: true, runningSubagentCount: 0 }),
    ).toBe(false);
  });

  it('Given swarm subagents are still running after the main turn Then do not append', () => {
    expect(
      shouldAppendSwarmEndedMarker({ swarmEntry: 'task', turnActive: false, runningSubagentCount: 2 }),
    ).toBe(false);
  });

  it('Given the session was not entered via /swarm <task> Then do not append', () => {
    expect(
      shouldAppendSwarmEndedMarker({ swarmEntry: undefined, turnActive: false, runningSubagentCount: 0 }),
    ).toBe(false);
  });
});

describe('buildSwarmEndedMarkerMessage', () => {
  it('Given a session and seq Then the marker id is seq-derived (replay-safe) and carries the metadata key', () => {
    const msg = buildSwarmEndedMarkerMessage('sess_1', 42);
    expect(msg.id).toBe('swarm_ended_sess_1_42');
    expect(msg.sessionId).toBe('sess_1');
    expect(msg.role).toBe('assistant');
    expect(msg.metadata?.[SWARM_ENDED_MARKER_METADATA_KEY]).toBe(true);
  });
});
