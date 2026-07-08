import { type ChatProvider } from '@mirri-ai/kosong';
import { AnthropicChatProvider } from '@mirri-ai/kosong/providers/anthropic';
import { describe, expect, it } from 'vitest';

import { applyAnthropicThinkingKeep } from '../../src/config/mirri-env-params';

function anthropic(): AnthropicChatProvider {
  return new AnthropicChatProvider({ model: 'claude-sonnet-4-6', apiKey: 'k' });
}

interface AnthropicKeepState {
  contextManagement?: { edits: Array<{ type: string; keep?: string }> };
  betaFeatures?: string[];
}

function anthropicState(provider: ChatProvider): AnthropicKeepState {
  return Reflect.get(provider as object, '_generationKwargs') as AnthropicKeepState;
}

describe('applyAnthropicThinkingKeep', () => {
  it('injects context_management keep="all" by default when thinking is on', () => {
    const out = applyAnthropicThinkingKeep(anthropic(), 'high', {});
    expect(anthropicState(out).contextManagement).toEqual({
      edits: [{ type: 'clear_thinking_20251015', keep: 'all' }],
    });
    expect(anthropicState(out).betaFeatures).toContain('context-management-2025-06-27');
  });

  it('injects keep from env when thinking is on', () => {
    const out = applyAnthropicThinkingKeep(anthropic(), 'high', { MIRRICODE_MODEL_THINKING_KEEP: 'all' });
    expect(anthropicState(out).contextManagement?.edits[0]?.keep).toBe('all');
  });

  it('injects keep from config when env is unset', () => {
    const out = applyAnthropicThinkingKeep(anthropic(), 'high', {}, 'all');
    expect(anthropicState(out).contextManagement?.edits[0]?.keep).toBe('all');
  });

  it('env takes precedence over config', () => {
    const out = applyAnthropicThinkingKeep(
      anthropic(),
      'high',
      { MIRRICODE_MODEL_THINKING_KEEP: 'all' },
      'off',
    );
    expect(anthropicState(out).contextManagement?.edits[0]?.keep).toBe('all');
  });

  it.each(['off', 'false', '0', 'no', 'none', 'null', 'OFF', 'None'])(
    'env off-value %s disables keep even when config enables it',
    (off) => {
      const out = applyAnthropicThinkingKeep(
        anthropic(),
        'high',
        { MIRRICODE_MODEL_THINKING_KEEP: off },
        'all',
      );
      expect(anthropicState(out).contextManagement).toBeUndefined();
    },
  );

  it.each(['off', 'none', 'null'])('config off-value %s disables keep by default', (off) => {
    const out = applyAnthropicThinkingKeep(anthropic(), 'high', {}, off);
    expect(anthropicState(out).contextManagement).toBeUndefined();
  });

  it('blank env falls through to config', () => {
    const out = applyAnthropicThinkingKeep(
      anthropic(),
      'high',
      { MIRRICODE_MODEL_THINKING_KEEP: '  ' },
      'off',
    );
    expect(anthropicState(out).contextManagement).toBeUndefined();
  });

  it('does NOT inject context_management when thinking is off', () => {
    const out = applyAnthropicThinkingKeep(anthropic(), 'off', { MIRRICODE_MODEL_THINKING_KEEP: 'all' });
    expect(anthropicState(out).contextManagement).toBeUndefined();
  });

  it('does not duplicate the context-management beta on repeated calls', () => {
    const out = applyAnthropicThinkingKeep(
      applyAnthropicThinkingKeep(anthropic(), 'high', {}),
      'high',
      {},
    );
    const betas = anthropicState(out).betaFeatures ?? [];
    expect(betas.filter((b) => b === 'context-management-2025-06-27')).toHaveLength(1);
  });

  it('leaves non-anthropic providers untouched', () => {
    const stub = { name: 'stub' } as unknown as ChatProvider;
    expect(applyAnthropicThinkingKeep(stub, 'high', { MIRRICODE_MODEL_THINKING_KEEP: 'all' })).toBe(stub);
  });
});
