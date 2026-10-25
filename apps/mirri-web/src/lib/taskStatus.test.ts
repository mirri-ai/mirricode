import { describe, expect, it } from 'vitest';
import { applyTerminalToTask, deriveMemberPhase, isTerminalStatus, isMemberLive, markTaskCancelled, terminalStatusToPhase } from './taskStatus';
import type { AppTask } from '../api/types';

describe('deriveMemberPhase (ADR-010 ③ 统一派生点)', () => {
  it('should return the terminal mapping when the task status is terminal', () => {
    expect(deriveMemberPhase({ status: 'completed', subagentPhase: 'working' })).toBe('completed');
    expect(deriveMemberPhase({ status: 'failed', subagentPhase: 'working' })).toBe('failed');
    expect(deriveMemberPhase({ status: 'cancelled', subagentPhase: 'backoff' })).toBe('aborted');
  });

  it('should fall back to subagentPhase when the task is still running', () => {
    expect(deriveMemberPhase({ status: 'running', subagentPhase: 'suspended' })).toBe('suspended');
    expect(deriveMemberPhase({ status: 'running', subagentPhase: 'awaiting' })).toBe('awaiting');
    expect(deriveMemberPhase({ status: 'running', subagentPhase: 'completed' })).toBe('completed');
  });

  it('should treat a running row without subagentPhase as working', () => {
    expect(deriveMemberPhase({ status: 'running' })).toBe('working');
  });
});

describe('applyTerminalToTask (ADR-010 ① 原子更新)', () => {
  it('should set status and subagentPhase in one update when a task completes', () => {
    const task = { id: 't1', status: 'running', subagentPhase: 'working' } as AppTask;
    const next = applyTerminalToTask(task, 'completed');
    expect(next.status).toBe('completed');
    expect(next.subagentPhase).toBe('completed');
  });

  it('should map cancelled to the aborted phase in the same update', () => {
    const task = { id: 't1', status: 'running', subagentPhase: 'backoff' } as AppTask;
    const next = applyTerminalToTask(task, 'cancelled');
    expect(next.status).toBe('cancelled');
    expect(next.subagentPhase).toBe('aborted');
  });
});

describe('markTaskCancelled (ADR-010 ① 用户停止乐观写)', () => {
  it('should mark the task cancelled with the aborted phase in one update (ADR-010 ①)', () => {
    const tasks = [
      { id: 'a', status: 'running', subagentPhase: 'working' },
      { id: 'b', status: 'running', subagentPhase: 'working' },
    ] as AppTask[];
    const next = markTaskCancelled(tasks, 'a');
    expect(next[0]!.status).toBe('cancelled');
    expect(next[0]!.subagentPhase).toBe('aborted');
    expect(next[1]!.status).toBe('running');
  });
});

describe('isMemberLive (ADR-010 ③ B-1 实时流开关收编)', () => {
  it('should treat a member as not live when its status is terminal (B-1: 实时流开关收编)', () => {
    expect(isMemberLive({ status: 'running' })).toBe(true);
    expect(isMemberLive({ status: 'completed' })).toBe(false);
    expect(isMemberLive({ status: 'cancelled' })).toBe(false);
  });
});

describe('isTerminalStatus / terminalStatusToPhase', () => {
  it('should treat only running as non-terminal', () => {
    expect(isTerminalStatus('running')).toBe(false);
    expect(isTerminalStatus('completed')).toBe(true);
    expect(isTerminalStatus('failed')).toBe(true);
    expect(isTerminalStatus('cancelled')).toBe(true);
    expect(terminalStatusToPhase('cancelled')).toBe('aborted');
  });
});
