import { describe, expect, it } from 'vitest';

import type { HookResult } from '../../src/session/hooks/types';
import {
  injectHookAdditionalContext,
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

describe('injectHookAdditionalContext', () => {
  it('should inject collected additionalContext as a user message with hook_additional_context origin', () => {
    const messages: Array<{ content: readonly unknown[]; origin: unknown }> = [];
    const mockContext = {
      appendUserMessage(content: readonly unknown[], origin: unknown) {
        messages.push({ content, origin });
      },
    };
    const results: readonly HookResult[] = [
      { action: 'allow', additionalContext: 'hint from hook', exitCode: 0 },
    ];
    injectHookAdditionalContext(mockContext, results, 'UserPromptSubmit');
    expect(messages).toHaveLength(1);
    expect(messages[0]!.content).toEqual([{ type: 'text', text: 'hint from hook' }]);
    expect(messages[0]!.origin).toEqual({ kind: 'hook_additional_context', event: 'UserPromptSubmit' });
  });

  it('should not inject when no additionalContext is present', () => {
    const messages: unknown[] = [];
    const mockContext = {
      appendUserMessage() {
        messages.push({});
      },
    };
    const results: readonly HookResult[] = [
      { action: 'allow', exitCode: 0 },
    ];
    injectHookAdditionalContext(mockContext, results, 'UserPromptSubmit');
    expect(messages).toHaveLength(0);
  });

  it('should not inject when results are undefined or empty', () => {
    const messages: unknown[] = [];
    const mockContext = {
      appendUserMessage() {
        messages.push({});
      },
    };
    injectHookAdditionalContext(mockContext, undefined, 'UserPromptSubmit');
    injectHookAdditionalContext(mockContext, [], 'UserPromptSubmit');
    expect(messages).toHaveLength(0);
  });

  it('should join multiple additionalContext values with newline', () => {
    const messages: Array<{ content: readonly unknown[]; origin: unknown }> = [];
    const mockContext = {
      appendUserMessage(content: readonly unknown[], origin: unknown) {
        messages.push({ content, origin });
      },
    };
    const results: readonly HookResult[] = [
      { action: 'allow', additionalContext: 'ctx1', exitCode: 0 },
      { action: 'allow', additionalContext: 'ctx2', exitCode: 0 },
    ];
    injectHookAdditionalContext(mockContext, results, 'Stop');
    expect(messages).toHaveLength(1);
    expect(messages[0]!.content).toEqual([{ type: 'text', text: 'ctx1\nctx2' }]);
  });

  it('should skip additionalContext from blocked results', () => {
    const messages: Array<{ content: readonly unknown[]; origin: unknown }> = [];
    const mockContext = {
      appendUserMessage(content: readonly unknown[], origin: unknown) {
        messages.push({ content, origin });
      },
    };
    const results: readonly HookResult[] = [
      { action: 'block', reason: 'denied', additionalContext: 'blocked ctx', exitCode: 2 },
      { action: 'allow', additionalContext: 'allowed ctx', exitCode: 0 },
    ];
    injectHookAdditionalContext(mockContext, results, 'PreToolUse');
    expect(messages).toHaveLength(1);
    expect(messages[0]!.content).toEqual([{ type: 'text', text: 'allowed ctx' }]);
  });
});
