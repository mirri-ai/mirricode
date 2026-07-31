import type { ContentPart } from '@mirri-ai/kosong';

import type { PromptOrigin } from '../../agent/context/types';
import type { HookResult } from './types';

export function renderHookResult(event: string, message: string): string {
  return `<hook_result hook_event="${event}">\n${message}\n</hook_result>`;
}

export interface RenderedHookResult {
  readonly event: string;
  readonly message: string;
  readonly text: string;
  readonly additionalContexts: readonly string[];
}

export function renderUserPromptHookResult(
  results: readonly HookResult[] | undefined,
): RenderedHookResult | undefined {
  const allowed = results?.filter((result) => result.action !== 'block') ?? [];
  const valid = allowed.filter(isValidHookResult);
  const messages = valid.map(userPromptHookMessage).filter(isNonEmptyString);
  const additionalContexts = valid
    .map((result) => result.additionalContext?.trim())
    .filter(isNonEmptyString);
  if (messages.length === 0 && additionalContexts.length === 0) return undefined;
  const displayMessage = messages.join('\n\n');
  return {
    event: 'UserPromptSubmit',
    message: displayMessage,
    text: messages.length > 0
      ? messages.map((message) => renderHookResult('UserPromptSubmit', message)).join('\n')
      : '',
    additionalContexts,
  };
}

export function renderUserPromptHookBlockResult(
  results: readonly HookResult[] | undefined,
): RenderedHookResult | undefined {
  const block = results?.find((result) => result.action === 'block');
  if (block === undefined) return undefined;
  const additionalContexts: readonly string[] = block.additionalContext?.trim()
    ? [block.additionalContext.trim()]
    : [];
  const message = block.message?.trim();
  if (message !== undefined && message.length > 0) {
    return {
      event: 'UserPromptSubmit',
      message,
      text: renderHookResult('UserPromptSubmit', message),
      additionalContexts,
    };
  }
  const reason = block.reason?.trim();
  const result =
    reason === undefined || reason.length === 0 ? 'Blocked by UserPromptSubmit hook' : reason;
  return {
    event: 'UserPromptSubmit',
    message: result,
    text: renderHookResult('UserPromptSubmit', result),
    additionalContexts,
  };
}

/**
 * Collects `additionalContext` strings from allowed, valid hook results.
 * Returns `undefined` when there is nothing to inject.
 */
export function collectAdditionalContext(
  results: readonly HookResult[] | undefined,
): string | undefined {
  if (results === undefined || results.length === 0) return undefined;
  const contexts = results
    .filter((result) => result.action !== 'block' && isValidHookResult(result))
    .map((result) => result.additionalContext?.trim())
    .filter(isNonEmptyString);
  if (contexts.length === 0) return undefined;
  return contexts.join('\n');
}

/**
 * Injects collected `additionalContext` from hook results as a user message
 * with `hook_additional_context` origin. No-op when no context is available.
 */
export function injectHookAdditionalContext(
  context: {
    appendUserMessage(content: readonly ContentPart[], origin: PromptOrigin): void;
  },
  results: readonly HookResult[] | undefined,
  event: string,
): void {
  const merged = collectAdditionalContext(results);
  if (merged === undefined) return;
  context.appendUserMessage(
    [{ type: 'text', text: merged }],
    { kind: 'hook_additional_context', event },
  );
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

function isValidHookResult(result: HookResult): boolean {
  if (result.timedOut === true) return false;
  if (result.exitCode !== undefined && result.exitCode !== 0) return false;
  return true;
}
