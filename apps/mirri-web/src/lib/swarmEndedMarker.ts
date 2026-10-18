// apps/mirri-web/src/lib/swarmEndedMarker.ts
// Pure TS — 「蜂群已结束」divider 的判定与消息构造。
//
// 生命周期定位：该 marker 是 web 客户端的**实时转录增强**（live-transcript
// affordance）——v3 引擎既无 task 触发源也无 turn 结束自动退出（plan/batches/
// B10 R-6），结束信号只能由 web 推导，故 marker 不进入服务端 journal：会话
// 快照重载后自然消失（compaction divider 有服务端 summary 支撑、可跨快照，
// 本 marker 无）。若未来 wire 增加 trigger 参数（R-6 末段的扩展锚点），本判定
// 可退回 TUI 同款 status-transition 事件源并服务端持久化。
import type { AppMessage } from '../api/types';
import { SWARM_ENDED_MARKER_METADATA_KEY } from '../api/types';

export { SWARM_ENDED_MARKER_METADATA_KEY };

/** /swarm <task> 打开的"任务蜂群"是否已结束：主回合结束且无活动 swarm
 *  子代理（v3 引擎无 task 触发源、无自动退出，见 plan/batches/B10 R-6）。 */
export function shouldAppendSwarmEndedMarker(input: {
  swarmEntry: 'task' | undefined;
  turnActive: boolean;
  runningSubagentCount: number;
}): boolean {
  return input.swarmEntry === 'task' && !input.turnActive && input.runningSubagentCount === 0;
}

/** 转录 divider 消息（compaction marker 同构：assistant 角色 + metadata 键 +
 *  seq 派生 id，事件重放不会追加重复 marker）。 */
export function buildSwarmEndedMarkerMessage(sessionId: string, seq: number): AppMessage {
  return {
    id: `swarm_ended_${sessionId}_${seq}`,
    sessionId,
    role: 'assistant',
    content: [],
    createdAt: new Date().toISOString(),
    metadata: { [SWARM_ENDED_MARKER_METADATA_KEY]: true },
  };
}
