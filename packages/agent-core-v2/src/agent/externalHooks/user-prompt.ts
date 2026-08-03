import type { ContextMessage } from '#/agent/contextMemory/types';

import type { HookResult } from './types';

export function renderHookResult(event: string, message: string): string {
  return `<hook_result hook_event="${event}">\n${message}\n</hook_result>`;
}

export interface RenderedHookResult {
  readonly event: string;
  readonly message: string;
  readonly text: string;
}

export function renderUserPromptHookResult(
  results: readonly HookResult[] | undefined,
): RenderedHookResult | undefined {
  const messages =
    results
      ?.filter((result) => result.action !== 'block')
      ?.map(userPromptHookMessage)
      .filter(isNonEmptyString) ??
    [];
  if (messages.length === 0) return undefined;
  const displayMessage = messages.join('\n\n');
  return {
    event: 'UserPromptSubmit',
    message: displayMessage,
    text: messages.map((message) => renderHookResult('UserPromptSubmit', message)).join('\n'),
  };
}

export function renderUserPromptHookBlockResult(
  results: readonly HookResult[] | undefined,
): RenderedHookResult | undefined {
  const block = results?.find((result) => result.action === 'block');
  if (block === undefined) return undefined;
  const message = block.message?.trim();
  if (message !== undefined && message.length > 0) {
    return {
      event: 'UserPromptSubmit',
      message,
      text: renderHookResult('UserPromptSubmit', message),
    };
  }
  const reason = block.reason?.trim();
  const result =
    reason === undefined || reason.length === 0 ? 'Blocked by UserPromptSubmit hook' : reason;
  return {
    event: 'UserPromptSubmit',
    message: result,
    text: renderHookResult('UserPromptSubmit', result),
  };
}

function userPromptHookMessage(result: HookResult): string | undefined {
  if (result.timedOut === true || (result.exitCode !== undefined && result.exitCode !== 0)) {
    return undefined;
  }
  const message = result.message?.trim();
  if (message !== undefined && message.length > 0) return message;
  const stdout = result.stdout?.trim();
  return stdout === undefined || stdout.length === 0 ? undefined : stdout;
}

function isNonEmptyString(value: string | undefined): value is string {
  return value !== undefined && value.length > 0;
}

/**
 * Collects `additionalContext` strings from hook results.
 *
 * By default only allowed, valid results are collected (block results are
 * skipped). Pass `includeBlock: true` to also collect `additionalContext`
 * from block results — used on the PreToolUse deny-decision path where the
 * hook's context message should still be surfaced to the user even though the
 * tool call was blocked.
 *
 * Returns `undefined` when there is nothing to inject.
 */
export function collectAdditionalContext(
  results: readonly HookResult[] | undefined,
  options: { includeBlock?: boolean } = {},
): string | undefined {
  if (results === undefined || results.length === 0) return undefined;
  const contexts = results
    .filter((result) => {
      if (!isValidHookResult(result)) return false;
      if (result.action === 'block' && !options.includeBlock) return false;
      return true;
    })
    .map((result) => result.additionalContext?.trim())
    .filter(isNonEmptyString);
  if (contexts.length === 0) return undefined;
  return contexts.join('\n');
}

/**
 * Injects collected `additionalContext` from hook results as a user message
 * with `hook_additional_context` origin. No-op when no context is available.
 *
 * Pass `includeBlock: true` to also include `additionalContext` from block
 * results (used on the PreToolUse deny-decision path).
 */
export function injectHookAdditionalContext(
  context: {
    append(...messages: readonly ContextMessage[]): void;
  },
  results: readonly HookResult[] | undefined,
  event: string,
  options: { includeBlock?: boolean } = {},
): void {
  const merged = collectAdditionalContext(results, options);
  if (merged === undefined) return;
  context.append({
    role: 'user',
    content: [{ type: 'text', text: merged }],
    toolCalls: [],
    origin: { kind: 'hook_additional_context', event },
  });
}

function isValidHookResult(result: HookResult): boolean {
  if (result.timedOut === true) return false;
  if (result.exitCode !== undefined && result.exitCode !== 0) return false;
  return true;
}
