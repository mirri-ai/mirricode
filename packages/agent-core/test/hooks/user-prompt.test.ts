import { describe, expect, it } from 'vitest';

import type { HookResult } from '../../src/session/hooks/types';
import {
  renderUserPromptHookBlockResult,
  renderUserPromptHookResult,
} from '../../src/session/hooks/user-prompt';

describe('renderUserPromptHookResult', () => {
  it('should return undefined when results are empty', () => {
    expect(renderUserPromptHookResult(undefined)).toBeUndefined();
    expect(renderUserPromptHookResult([])).toBeUndefined();
  });

  it('should return undefined when all results are blocked', () => {
    const results: readonly HookResult[] = [
      { action: 'block', reason: 'denied', exitCode: 2 },
    ];
    expect(renderUserPromptHookResult(results)).toBeUndefined();
  });

  it('should render message from hook result', () => {
    const results: readonly HookResult[] = [
      { action: 'allow', message: 'hello', exitCode: 0 },
    ];
    const rendered = renderUserPromptHookResult(results)!;
    expect(rendered.event).toBe('UserPromptSubmit');
    expect(rendered.message).toBe('hello');
    expect(rendered.text).toContain('hello');
    expect(rendered.additionalContexts).toEqual([]);
  });

  it('should render stdout when message is empty', () => {
    const results: readonly HookResult[] = [
      { action: 'allow', stdout: 'stdout content', exitCode: 0 },
    ];
    const rendered = renderUserPromptHookResult(results)!;
    expect(rendered.message).toBe('stdout content');
  });

  it('should collect additionalContext from allowed results', () => {
    const results: readonly HookResult[] = [
      { action: 'allow', message: 'msg1', additionalContext: 'ctx1', exitCode: 0 },
      { action: 'allow', message: 'msg2', additionalContext: 'ctx2', exitCode: 0 },
    ];
    const rendered = renderUserPromptHookResult(results)!;
    expect(rendered.additionalContexts).toEqual(['ctx1', 'ctx2']);
  });

  it('should return result with only additionalContext when no message', () => {
    const results: readonly HookResult[] = [
      { action: 'allow', additionalContext: 'context only', exitCode: 0 },
    ];
    const rendered = renderUserPromptHookResult(results)!;
    expect(rendered.message).toBe('');
    expect(rendered.text).toBe('');
    expect(rendered.additionalContexts).toEqual(['context only']);
  });

  it('should skip additionalContext from blocked results', () => {
    const results: readonly HookResult[] = [
      { action: 'block', reason: 'denied', additionalContext: 'blocked ctx', exitCode: 2 },
      { action: 'allow', message: 'allowed', additionalContext: 'allowed ctx', exitCode: 0 },
    ];
    const rendered = renderUserPromptHookResult(results)!;
    expect(rendered.additionalContexts).toEqual(['allowed ctx']);
    expect(rendered.message).toBe('allowed');
  });

  it('should trim and filter empty additionalContext values', () => {
    const results: readonly HookResult[] = [
      { action: 'allow', message: 'msg', additionalContext: '  ', exitCode: 0 },
    ];
    const rendered = renderUserPromptHookResult(results)!;
    expect(rendered.additionalContexts).toEqual([]);
  });

  it('should skip timed-out results from both message and additionalContext', () => {
    const results: readonly HookResult[] = [
      { action: 'allow', timedOut: true, additionalContext: 'timeout ctx' },
    ];
    expect(renderUserPromptHookResult(results)).toBeUndefined();
  });

  it('should skip non-zero exit code results', () => {
    const results: readonly HookResult[] = [
      { action: 'allow', exitCode: 1, additionalContext: 'error ctx' },
    ];
    expect(renderUserPromptHookResult(results)).toBeUndefined();
  });
});

describe('renderUserPromptHookBlockResult', () => {
  it('should return undefined when no blocked result', () => {
    const results: readonly HookResult[] = [
      { action: 'allow', message: 'ok', exitCode: 0 },
    ];
    expect(renderUserPromptHookBlockResult(results)).toBeUndefined();
  });

  it('should render block message with additionalContexts', () => {
    const results: readonly HookResult[] = [
      { action: 'block', message: 'blocked!', additionalContext: 'block ctx', exitCode: 2 },
    ];
    const rendered = renderUserPromptHookBlockResult(results)!;
    expect(rendered.message).toBe('blocked!');
    expect(rendered.additionalContexts).toEqual(['block ctx']);
  });

  it('should return empty additionalContexts when block has no additionalContext', () => {
    const results: readonly HookResult[] = [
      { action: 'block', reason: 'nope', exitCode: 2 },
    ];
    const rendered = renderUserPromptHookBlockResult(results)!;
    expect(rendered.additionalContexts).toEqual([]);
  });
});
