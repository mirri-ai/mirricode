import type { ToolCall } from '#/kosong/contract/message';
import { describe, expect, it } from 'vitest';

import type { ResolvedToolExecutionHookContext } from '#/agent/toolExecutor/toolHooks';
import { DenyAllPermissionPolicy } from '#/agent/permissionPolicy/policies/deny-all';
import { ToolAccesses } from '#/tool/toolContract';

const signal = new AbortController().signal;

function policyContext(toolName: string, args: unknown): ResolvedToolExecutionHookContext {
  return {
    turnId: '0',
    stepNumber: 1,
    signal,
    llm: {},
    args,
    toolCall: {
      type: 'function',
      id: `call_${toolName}`,
      name: toolName,
      arguments: JSON.stringify(args),
    } satisfies ToolCall,
    toolCalls: [
      {
        type: 'function',
        id: `call_${toolName}`,
        name: toolName,
        arguments: JSON.stringify(args),
      },
    ],
    execution: {
      accesses: ToolAccesses.none(),
      approvalRule: toolName,
      execute: async () => ({ output: '' }),
    },
  } as unknown as ResolvedToolExecutionHookContext;
}

describe('DenyAllPermissionPolicy', () => {
  const policy = new DenyAllPermissionPolicy();

  it('should return undefined when not enabled', () => {
    expect(policy.evaluate(policyContext('Bash', { command: 'ls' }))).toBeUndefined();
  });

  it('should deny every tool call when enabled', () => {
    policy.enable();
    try {
      expect(policy.evaluate(policyContext('Bash', { command: 'ls' }))).toEqual({
        kind: 'deny',
        message: 'Tool calls are disabled.',
        reason: { source: 'side_question' },
      });
    } finally {
      policy.disable();
    }
  });

  it('should deny with custom message when enabled with one', () => {
    policy.enable('Side questions cannot use tools.');
    try {
      expect(policy.evaluate(policyContext('Read', { path: '/a.ts' }))).toEqual({
        kind: 'deny',
        message: 'Side questions cannot use tools.',
        reason: { source: 'side_question' },
      });
    } finally {
      policy.disable();
    }
  });

  it('should block every tool type when enabled', () => {
    policy.enable();
    try {
      const tools = [
        ['Bash', { command: 'ls' }],
        ['Write', { path: '/a.ts', content: 'x' }],
        ['Edit', { path: '/a.ts', old_string: 'a', new_string: 'b' }],
        ['Read', { path: '/a.ts' }],
        ['Grep', { pattern: 'TODO' }],
        ['Agent', { prompt: 'review this' }],
      ] as const;

      for (const [toolName, args] of tools) {
        const result = policy.evaluate(policyContext(toolName, args));
        expect(result).toBeDefined();
        expect(result!.kind).toBe('deny');
      }
    } finally {
      policy.disable();
    }
  });

  it('should pass through again after disable', () => {
    policy.enable();
    policy.disable();
    expect(policy.evaluate(policyContext('Bash', { command: 'ls' }))).toBeUndefined();
  });
});
