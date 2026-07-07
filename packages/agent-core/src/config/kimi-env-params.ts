import { type ChatProvider, type ThinkingEffort } from '@mirri-ai/kosong';
import { AnthropicChatProvider } from '@mirri-ai/kosong/providers/anthropic';

type Env = Readonly<Record<string, string | undefined>>;

/**
 * Apply provider-specific sampling params from the environment.
 * No-op for non-Kimi providers (Kimi provider has been removed).
 */
export function applyKimiEnvSamplingParams(
  provider: ChatProvider,
  _env: Env = process.env,
): ChatProvider {
  return provider;
}

/**
 * Force a specific thinking effort via environment variable.
 * No-op for non-Kimi providers (Kimi provider has been removed).
 */
export function applyKimiEnvThinkingEffort(
  provider: ChatProvider,
  _thinkingEffort: ThinkingEffort,
  _env: Env = process.env,
): ChatProvider {
  return provider;
}

const KEEP_OFF_VALUES = new Set(['0', 'false', 'no', 'off', 'none', 'null']);

type KeepResolution =
  | { readonly specified: false }
  | { readonly specified: true; readonly value: string | undefined };

function parseKeepValue(raw: string | undefined): KeepResolution {
  const trimmed = raw?.trim();
  if (trimmed === undefined || trimmed.length === 0) return { specified: false };
  if (KEEP_OFF_VALUES.has(trimmed.toLowerCase())) return { specified: true, value: undefined };
  return { specified: true, value: trimmed };
}

/**
 * Resolve the Preserved Thinking passthrough with precedence env > config > default "all".
 * Only meaningful while thinking is on.
 */
export function resolveThinkingKeep(
  env: Env,
  configKeep: string | undefined,
  thinkingEffort: ThinkingEffort,
): string | undefined {
  if (thinkingEffort === 'off') return undefined;
  const fromEnv = parseKeepValue(env['MIRRICODE_MODEL_THINKING_KEEP']);
  if (fromEnv.specified) return fromEnv.value;
  const fromConfig = parseKeepValue(configKeep);
  if (fromConfig.specified) return fromConfig.value;
  return 'all';
}

/**
 * Apply the Preserved Thinking passthrough to a chat provider.
 * No-op for non-Kimi providers (Kimi provider has been removed).
 */
export function applyKimiEnvThinkingKeep(
  provider: ChatProvider,
  _thinkingEffort: ThinkingEffort,
  _env: Env = process.env,
  _configKeep?: string,
): ChatProvider {
  return provider;
}

/**
 * Apply the Anthropic equivalent of Preserved Thinking — a `context_management`
 * `clear_thinking_20251015` edit carrying `keep` — to an Anthropic chat
 * provider. Non-Anthropic providers are returned unchanged.
 */
export function applyAnthropicThinkingKeep(
  provider: ChatProvider,
  thinkingEffort: ThinkingEffort,
  env: Env = process.env,
  configKeep?: string,
): ChatProvider {
  if (!(provider instanceof AnthropicChatProvider)) return provider;
  const keep = resolveThinkingKeep(env, configKeep, thinkingEffort);
  if (keep === undefined) return provider;
  return provider.withThinkingKeep(keep);
}
