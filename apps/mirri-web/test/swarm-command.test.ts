// apps/mirri-web/test/swarm-command.test.ts
// /swarm 斜杠命令的变体解析（裸 toggle / on / off / <task>）——从 App.vue
// 的内联 slice/trim 抽出为纯函数，Task 2/3 在此之上统一确认与 task 语义。
import { describe, expect, it } from 'vitest';
import { parseSwarmCommand } from '../src/lib/swarmCommand';

describe('parseSwarmCommand', () => {
  it('Given a bare /swarm Then the variant is toggle', () => {
    expect(parseSwarmCommand('')).toEqual({ kind: 'toggle' });
  });

  it('Given /swarm on and /swarm off Then the variants are explicit sets', () => {
    expect(parseSwarmCommand('on')).toEqual({ kind: 'on' });
    expect(parseSwarmCommand('off')).toEqual({ kind: 'off' });
  });

  it('Given /swarm <task> Then the variant carries the trimmed task text', () => {
    expect(parseSwarmCommand(' 梳理仓库结构并总结 ')).toEqual({
      kind: 'task',
      task: '梳理仓库结构并总结',
    });
  });

  it('Given an argument that is only whitespace Then the variant falls back to toggle', () => {
    expect(parseSwarmCommand('   ')).toEqual({ kind: 'toggle' });
  });
});
