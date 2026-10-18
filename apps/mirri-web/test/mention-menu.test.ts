import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ref } from 'vue';
import { useMentionMenu } from '../src/composables/useMentionMenu';
import type { TextSource } from '../src/composables/useTextSource';
import type { FileItem } from '../src/types';

/** A TextSource over a mutable {text, caret} pair (mock editor host). */
function makeTextSource(initial: { text: string; caret: number }): TextSource {
  return {
    getText: () => initial.text.slice(0, initial.caret),
    getCaret: () => initial.caret,
    replaceRange(start, end, insert) {
      initial.text = initial.text.slice(0, start) + insert + initial.text.slice(end);
      initial.caret = start + insert.length;
    },
    focus: () => {},
  };
}

function setup(initialText = '', searchFiles?: (q: string) => Promise<FileItem[]>) {
  const field = { text: initialText, caret: initialText.length };
  const textSource = makeTextSource(field);
  const mention = useMentionMenu({
    textSource,
    searchFiles: () => searchFiles,
  });
  return { field, mention };
}

describe('useMentionMenu — update', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('stays closed when there is no @token', async () => {
    const searchFiles = vi.fn().mockResolvedValue([]);
    const { mention } = setup('hello', searchFiles);
    mention.update();
    await vi.advanceTimersByTimeAsync(200);
    expect(mention.open.value).toBe(false);
    expect(searchFiles).not.toHaveBeenCalled();
  });

  it('stays closed when searchFiles is not provided', async () => {
    const { mention } = setup('@a');
    mention.update();
    await vi.advanceTimersByTimeAsync(200);
    expect(mention.open.value).toBe(false);
  });

  it('opens with search results after the debounce', async () => {
    const searchFiles = vi.fn().mockResolvedValue([{ path: 'src/a.ts', name: 'a.ts' }]);
    const { mention } = setup('@a', searchFiles);
    mention.update();
    expect(mention.open.value).toBe(false); // debounced, not yet
    await vi.advanceTimersByTimeAsync(200);
    expect(searchFiles).toHaveBeenCalledWith('a');
    expect(mention.open.value).toBe(true);
    expect(mention.items.value).toEqual([{ path: 'src/a.ts', name: 'a.ts' }]);
    expect(mention.loading.value).toBe(false);
    expect(mention.active.value).toBe(0);
  });

  it('clears items and stops loading when the search throws', async () => {
    const searchFiles = vi.fn().mockRejectedValue(new Error('boom'));
    const { mention } = setup('@a', searchFiles);
    mention.update();
    await vi.advanceTimersByTimeAsync(200);
    expect(mention.items.value).toEqual([]);
    expect(mention.loading.value).toBe(false);
  });
});

describe('useMentionMenu — select', () => {
  it('replaces the @token with the chosen path', () => {
    const { field, mention } = setup('hello @a');
    mention.select({ path: 'src/a.ts', name: 'a.ts' });
    expect(field.text).toBe('hello src/a.ts');
    expect(mention.open.value).toBe(false);
  });

  it('is a no-op when there is no @token', () => {
    const { field, mention } = setup('hello');
    mention.select({ path: 'src/a.ts', name: 'a.ts' });
    expect(field.text).toBe('hello');
  });
});
