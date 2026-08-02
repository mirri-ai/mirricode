/**
 * `media` domain — capability-driven media projection.
 *
 * These tests are the executable specification for `mediaCapabilityProjector`:
 * they pin down what happens when the bound model cannot accept a modality
 * present in the conversation. The projector must (1) leave the request
 * untouched when the model supports every modality, (2) replace each
 * unsupported media part with a placeholder that carries the persisted file
 * path so a sub-agent can read it, (3) use a short fallback when no path was
 * recorded, and (4) reject protocol-style ids that are not filesystem paths.
 */

import { describe, expect, it } from 'vitest';

import type { Message } from '#/kosong/contract/message';

import {
  mediaCapabilityGapPlaceholder,
  mediaInputCapabilityOf,
  projectMediaForModelCapability,
  type MediaInputCapability,
} from '#/agent/media/mediaCapabilityProjector';

const ALL_MODALITIES_SUPPORTED: MediaInputCapability = {
  image_in: true,
  video_in: true,
  audio_in: true,
};

const NO_MODALITIES_SUPPORTED: MediaInputCapability = {
  image_in: false,
  video_in: false,
  audio_in: false,
};

function textMessage(text: string): Message {
  return {
    role: 'user',
    content: [{ type: 'text', text }],
    toolCalls: [],
  };
}

function imageMessage(persistedPath: string | undefined): Message {
  return {
    role: 'user',
    content: [
      { type: 'text', text: 'describe this' },
      {
        type: 'image_url',
        imageUrl: { url: 'data:image/png;base64,abc', id: persistedPath },
      },
    ],
    toolCalls: [],
  };
}

describe('mediaInputCapabilityOf', () => {
  it('extracts only the three input modality flags from a full capability record', () => {
    const view = mediaInputCapabilityOf({
      image_in: true,
      video_in: false,
      audio_in: true,
      thinking: true,
      tool_use: true,
      max_context_tokens: 128000,
    });
    expect(view).toEqual({ image_in: true, video_in: false, audio_in: true });
  });
});

describe('projectMediaForModelCapability', () => {
  it('returns the same array reference when the model supports every modality', () => {
    const messages = [imageMessage('/tmp/original.png')];
    expect(projectMediaForModelCapability(messages, ALL_MODALITIES_SUPPORTED)).toBe(messages);
  });

  it('returns the same array reference when no message contains media', () => {
    const messages = [textMessage('hello'), textMessage('world')];
    expect(projectMediaForModelCapability(messages, NO_MODALITIES_SUPPORTED)).toBe(messages);
  });

  it('replaces an image part with a placeholder containing the persisted file path', () => {
    const messages = [imageMessage('/home/user/.mirri-code/sessions/x/media-originals/abc.png')];
    const projected = projectMediaForModelCapability(messages, NO_MODALITIES_SUPPORTED);

    expect(projected).not.toBe(messages);
    const replaced = projected[0]!.content;
    expect(replaced).toHaveLength(2);
    expect(replaced[1]).toMatchObject({
      type: 'text',
      text: expect.stringContaining('/home/user/.mirri-code/sessions/x/media-originals/abc.png'),
    });
    expect(replaced[1]).toMatchObject({
      type: 'text',
      text: expect.stringContaining('current model has no image input'),
    });
    expect(replaced[1]).toMatchObject({
      type: 'text',
      text: expect.stringContaining('ReadMediaFile'),
    });
    expect(replaced[1]).toMatchObject({
      type: 'text',
      text: expect.stringContaining('media-reader'),
    });
  });

  it('uses the short fallback when the image part has no persisted path', () => {
    const messages = [imageMessage(undefined)];
    const projected = projectMediaForModelCapability(messages, NO_MODALITIES_SUPPORTED);

    const replaced = projected[0]!.content[1]!;
    expect(replaced).toMatchObject({
      type: 'text',
      text: '[image omitted: current model has no image input]',
    });
  });

  it('uses the short fallback when the id is a protocol identifier, not a filesystem path', () => {
    const messages = [imageMessage('ms://file-1')];
    const projected = projectMediaForModelCapability(messages, NO_MODALITIES_SUPPORTED);

    const replaced = projected[0]!.content[1]!;
    expect(replaced).toMatchObject({
      type: 'text',
      text: '[image omitted: current model has no image input]',
    });
  });

  it('does not mutate the input messages array or its messages', () => {
    const messages = [imageMessage('/tmp/original.png')];
    const snapshot = JSON.stringify(messages);
    projectMediaForModelCapability(messages, NO_MODALITIES_SUPPORTED);
    expect(JSON.stringify(messages)).toBe(snapshot);
  });

  it('leaves non-media messages as the same reference when others are adapted', () => {
    const first = textMessage('unrelated');
    const second = imageMessage('/tmp/img.png');
    const messages = [first, second];
    const projected = projectMediaForModelCapability(messages, NO_MODALITIES_SUPPORTED);

    expect(projected[0]).toBe(first); // untouched
    expect(projected[1]).not.toBe(second); // replaced
  });

  it('only drops the modality the model lacks, leaving others intact', () => {
    const onlyImageUnsupported: MediaInputCapability = {
      image_in: false,
      video_in: true,
      audio_in: true,
    };
    const messages: Message[] = [
      {
        role: 'user',
        content: [
          { type: 'image_url', imageUrl: { url: 'data:image/png;base64,a', id: '/tmp/a.png' } },
          { type: 'audio_url', audioUrl: { url: 'data:audio/wav;base64,b', id: '/tmp/b.wav' } },
        ],
        toolCalls: [],
      },
    ];
    const projected = projectMediaForModelCapability(messages, onlyImageUnsupported);

    const content = projected[0]!.content;
    expect(content[0]).toMatchObject({ type: 'text', text: expect.stringContaining('image omitted') });
    expect(content[1]).toMatchObject({ type: 'audio_url' });
  });
});

describe('mediaCapabilityGapPlaceholder', () => {
  it('builds the full sub-agent guidance when a file path is present', () => {
    const placeholder = mediaCapabilityGapPlaceholder('image', '/tmp/saved.png');
    expect(placeholder).toContain('[image omitted: current model has no image input]');
    expect(placeholder).toContain('The original image has been saved to: /tmp/saved.png');
    expect(placeholder).toContain('media-reader');
    expect(placeholder).toContain('ReadMediaFile');
    expect(placeholder).toContain('Available models');
  });

  it('produces the same guidance shape for video and audio kinds', () => {
    for (const kind of ['video', 'audio'] as const) {
      const placeholder = mediaCapabilityGapPlaceholder(kind, `/tmp/saved.${kind}`);
      expect(placeholder).toContain(`[${kind} omitted: current model has no ${kind} input]`);
      expect(placeholder).toContain(`The original ${kind} has been saved to: /tmp/saved.${kind}`);
      expect(placeholder).toContain('ReadMediaFile');
    }
  });

  it('throws when called with a null media kind', () => {
    expect(() => mediaCapabilityGapPlaceholder(null, '/tmp/x')).toThrow(/media kind/);
  });
});
