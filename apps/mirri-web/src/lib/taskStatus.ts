// ADR-010 成员状态单一事实字段：终态↔阶段映射与统一派生点。
// 终态映射固定（ADR-010 ①）：completed→completed、cancelled→aborted、failed→failed。
import type { AppSubagentPhase, AppTask, AppTaskStatus } from '../api/types';

export function isTerminalStatus(status: AppTaskStatus): boolean {
  return status !== 'running';
}

export function terminalStatusToPhase(status: AppTaskStatus): AppSubagentPhase {
  if (status === 'completed') return 'completed';
  if (status === 'cancelled') return 'aborted';
  return 'failed';
}

/** ADR-010 ③ 唯一派生点：终态映射优先，未终态取中间阶段，缺省 working。 */
export function deriveMemberPhase(task: Pick<AppTask, 'status' | 'subagentPhase'>): AppSubagentPhase {
  if (isTerminalStatus(task.status)) return terminalStatusToPhase(task.status);
  return task.subagentPhase ?? 'working';
}

/** ADR-010 ① 终态-阶段原子更新：写终态必同步映射阶段。 */
export function applyTerminalToTask<T extends AppTask>(task: T, status: AppTaskStatus): T {
  return { ...task, status, subagentPhase: terminalStatusToPhase(status) };
}

/** 用户停止的本地乐观写（ADR-010 ① 第 4 写路径）：原子置 cancelled + aborted。 */
export function markTaskCancelled<T extends AppTask>(tasks: T[], taskId: string): T[] {
  return tasks.map((t) => (t.id === taskId ? applyTerminalToTask(t, 'cancelled') : t));
}

/** 成员实时流开关（ADR-010 ③ 读端收编）：非终态即 live。 */
export function isMemberLive(member: { status: AppTaskStatus }): boolean {
  return !isTerminalStatus(member.status);
}
