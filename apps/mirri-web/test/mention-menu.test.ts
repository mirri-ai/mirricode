import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useMentionMenu } from '../src/composables/useMentionMenu';
import type { TextSource } from '../src/composables/useTextSource';
import type { FileCandidate, SearchFilesResult } from '../src/api/types';

/** A TextSource over a mutable {text, caret} pair (mock editor host). */
function makeTextSource(
  field: { text: string; caret: number },
  existingPaths: Set<string> = new Set(),
): TextSource & {
  insertFileMention: NonNullable<TextSource['insertFileMention']>;
  /** Test-only tap: the insertFileMention calls received so far. */
  __inserted: Array<{ display: string; path: string; kind: string; root?: string }>;
} {
  const inserted: Array<{ display: string; path: string; kind: string; root?: string }> = [];
  const source: TextSource & {
    __inserted: typeof inserted;
  } = {
    getText: () => field.text.slice(0, field.caret),
    getCaret: () => field.caret,
    replaceRange(start, end, insert) {
      field.text = field.text.slice(0, start) + insert + field.text.slice(end);
      field.caret = start + insert.length;
    },
    insertFileMention(options) {
      inserted.push(options);
      field.text += `[${options.display}]`;
      field.caret = field.text.length;
    },
    hasFileMention(path) {
      return existingPaths.has(path);
    },
    focus: () => {},
    __inserted: inserted,
  };
  return source as never;
}

type TestSource = ReturnType<typeof makeTextSource>;

function setup(
  initialText: string,
  searchFiles?: (q: string) => Promise<SearchFilesResult>,
  existingPaths: Set<string> = new Set(),
) {
  const field = { text: initialText, caret: initialText.length };
  const textSource = makeTextSource(field, existingPaths);
  const mention = useMentionMenu({
    textSource,
    searchFiles: () => searchFiles,
  });
  return { field, textSource, mention };
}

function result(items: FileCandidate[], indexState: SearchFilesResult['indexState'] = 'ready'): SearchFilesResult {
  return { items, truncated: false, indexState };
}

function candidate(overrides: Partial<FileCandidate> = {}): FileCandidate {
  return {
    path: 'src/components/Composer.vue',
    name: 'Composer.vue',
    kind: 'file',
    score: 0.9,
    matchPositions: [0],
    root: 'webapp',
    ...overrides,
  };
}

describe('useMentionMenu — update', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('should stay closed when there is no @token', async () => {
    const searchFiles = vi.fn().mockResolvedValue(result([]));
    const { mention } = setup('hello', searchFiles);
    mention.update();
    await vi.advanceTimersByTimeAsync(200);
    expect(mention.open.value).toBe(false);
    expect(searchFiles).not.toHaveBeenCalled();
  });

  it('should stay closed when searchFiles is not provided', async () => {
    const { mention } = setup('@a');
    mention.update();
    await vi.advanceTimersByTimeAsync(200);
    expect(mention.open.value).toBe(false);
  });

  it('should open with search results after the debounce', async () => {
    const searchFiles = vi.fn().mockResolvedValue(result([candidate()]));
    const { mention } = setup('@a', searchFiles);
    mention.update();
    expect(mention.open.value).toBe(false); // debounced, not yet
    await vi.advanceTimersByTimeAsync(200);
    expect(searchFiles).toHaveBeenCalledWith('a');
    expect(mention.open.value).toBe(true);
    expect(mention.items.value).toHaveLength(1);
    expect(mention.loading.value).toBe(false);
    expect(mention.active.value).toBe(0);
    expect(mention.indexState.value).toBe('ready');
  });

  it('should clear items and stop loading when the search throws', async () => {
    const searchFiles = vi.fn().mockRejectedValue(new Error('boom'));
    const { mention } = setup('@a', searchFiles);
    mention.update();
    await vi.advanceTimersByTimeAsync(200);
    expect(mention.items.value).toEqual([]);
    expect(mention.loading.value).toBe(false);
  });

  it('should cancel stale searches when the query changes quickly', async () => {
    let resolveFirst: (r: SearchFilesResult) => void = () => {};
    const first = new Promise<SearchFilesResult>((res) => {
      resolveFirst = res;
    });
    const searchFiles = vi
      .fn()
      .mockImplementationOnce(() => first)
      .mockResolvedValueOnce(result([candidate({ path: 'second.ts', name: 'second.ts' })]));
    const field = { text: '@a', caret: 2 };
    const source = makeTextSource(field);
    const menu = useMentionMenu({ textSource: source, searchFiles: () => searchFiles });
    menu.update();
    await vi.advanceTimersByTimeAsync(200);
    // Query changed before the first answer arrived (same menu, new chars).
    field.text = '@ab';
    field.caret = 3;
    menu.update();
    await vi.advanceTimersByTimeAsync(200);
    // The stale (first) result lands late — it must not overwrite.
    resolveFirst(result([candidate({ path: 'stale.ts', name: 'stale.ts' })]));
    await first;
    expect(menu.items.value[0]!.path).toBe('second.ts');
    expect(menu.open.value).toBe(true);
  });

  it('should not reopen on caret move back into the same dismissed token', async () => {
    const searchFiles = vi.fn().mockResolvedValue(result([candidate()]));
    const { field, mention } = setup('@com', searchFiles);
    mention.update();
    await vi.advanceTimersByTimeAsync(200);
    expect(mention.open.value).toBe(true);
    // Esc — the composer calls dismiss() (uiux §3 memory).
    mention.dismiss();
    expect(mention.open.value).toBe(false);
    // Caret move within the same token (no new chars): update() keeps it closed.
    field.caret = 2;
    mention.update();
    await vi.advanceTimersByTimeAsync(200);
    expect(mention.open.value).toBe(false);
    // Typing a new character reopens.
    field.text = '@comx';
    field.caret = 5;
    mention.update();
    await vi.advanceTimersByTimeAsync(200);
    expect(mention.open.value).toBe(true);
  });

  it('should not terminate a quoted token at spaces while the quote is open', async () => {
    const searchFiles = vi.fn().mockResolvedValue(result([candidate()]));
    const { mention } = setup('找 @"a b', searchFiles);
    mention.update();
    await vi.advanceTimersByTimeAsync(200);
    expect(searchFiles).toHaveBeenCalledWith('a b');
    expect(mention.open.value).toBe(true);
  });

  it('should terminate the quoted token after the closing quote and following space', async () => {
    const searchFiles = vi.fn().mockResolvedValue(result([candidate()]));
    // Caret sits after "x" — past the closed quote + space, so there is no
    // active token under the caret at all.
    const { mention } = setup('找 @"a b" x', searchFiles);
    mention.update();
    await vi.advanceTimersByTimeAsync(200);
    expect(mention.open.value).toBe(false);
    expect(searchFiles).not.toHaveBeenCalled();
  });

  it('should pass kind and matchPositions through to items', async () => {
    const searchFiles = vi
      .fn()
      .mockResolvedValue(result([candidate({ kind: 'directory', matchPositions: [2, 5] })], 'building'));
    const { mention } = setup('@comp', searchFiles);
    mention.update();
    await vi.advanceTimersByTimeAsync(200);
    expect(mention.items.value[0]).toMatchObject({ kind: 'directory', matchPositions: [2, 5] });
    expect(mention.indexState.value).toBe('building');
  });
});

describe('useMentionMenu — select', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('should keep the menu open and rewrite the token when drilling into a directory', async () => {
    const searchFiles = vi.fn().mockResolvedValue(result([]));
    const { field, mention } = setup('找 @src/com', searchFiles);
    mention.update();
    await vi.advanceTimersByTimeAsync(200);
    const dir = candidate({ path: 'src/components', name: 'components', kind: 'directory' });
    mention.select(dir, 'drill');
    expect(mention.open.value).toBe(true);
    expect(field.text).toBe('找 @src/components/');
    expect(mention.crumbs.value).toContain('src');
    // The rewritten token re-triggers a search for the drilled prefix.
    expect(searchFiles).toHaveBeenLastCalledWith('src/components/');
  });

  it('should close the menu and insert a chip when picking a file', async () => {
    const searchFiles = vi.fn().mockResolvedValue(result([]));
    const { textSource, mention } = setup('找 @src/com', searchFiles);
    mention.update();
    await vi.advanceTimersByTimeAsync(200);
    const file = candidate({ path: 'src/Composer.vue', name: 'Composer.vue', kind: 'file' });
    mention.select(file, 'pick');
    expect(mention.open.value).toBe(false);
    expect(textSource.__inserted).toHaveLength(1);
    expect(textSource.__inserted[0]).toMatchObject({ path: 'src/Composer.vue', kind: 'file', root: 'webapp' });
  });

  it('should pick folders when the pick action is used on a directory', async () => {    const searchFiles = vi.fn().mockResolvedValue(result([]));
    const { textSource, mention } = setup('找 @src', searchFiles);
    mention.update();
    await vi.advanceTimersByTimeAsync(200);
    mention.select(candidate({ path: 'src/utils', name: 'utils', kind: 'directory' }), 'pick');
    expect(mention.open.value).toBe(false);
    expect(textSource.__inserted[0]).toMatchObject({ kind: 'folder' });
  });

  it('should be a no-op when there is no @token', () => {
    const { field, mention } = setup('hello');
    mention.select(candidate(), 'pick');
    expect(field.text).toBe('hello');
  });

  it('should not insert a second chip when the path is already mentioned', async () => {
    const searchFiles = vi.fn().mockResolvedValue(result([]));
    const file = candidate({ path: 'src/Composer.vue', name: 'Composer.vue', kind: 'file' });
    // Given the document already references src/Composer.vue
    const { field, textSource, mention } = setup('找 @comp', searchFiles, new Set(['src/Composer.vue']));
    mention.update();
    await vi.advanceTimersByTimeAsync(200);
    // When the same file is picked again
    mention.select(file, 'pick');
    // Then the menu closes and the token is removed, but no chip is inserted
    expect(mention.open.value).toBe(false);
    expect(textSource.__inserted).toHaveLength(0);
    expect(field.text).not.toContain('@comp');
  });

  it('should pick a file when the pick action is requested on a file', async () => {
    // The E9 uiux §3 keyboard contract: Enter (pick) on a FILE inserts the
    // chip — only directories distinguish drill from pick.
    const searchFiles = vi.fn().mockResolvedValue(result([]));
    const { textSource, mention } = setup('找 @src', searchFiles);
    mention.update();
    await vi.advanceTimersByTimeAsync(200);
    mention.select(candidate({ path: 'src/a.ts', name: 'a.ts', kind: 'file' }), 'pick');
    expect(textSource.__inserted).toHaveLength(1);
    expect(textSource.__inserted[0]).toMatchObject({ path: 'src/a.ts', kind: 'file' });
  });

  it('should rewrite the token to the breadcrumb prefix when a crumb is chosen', async () => {
    const searchFiles = vi.fn().mockResolvedValue(result([]));
    const { field, mention } = setup('找 @src/com', searchFiles);
    mention.update();
    await vi.advanceTimersByTimeAsync(200);
    mention.select(candidate({ path: 'src/components', name: 'components', kind: 'directory' }), 'drill');
    // One more level deep, then jump back via the first crumb.
    mention.select(candidate({ path: 'src/components/ui', name: 'ui', kind: 'directory' }), 'drill');
    expect(field.text).toBe('找 @src/components/ui/');
    mention.goCrumb(0);
    expect(field.text).toBe('找 @src/');
    expect(mention.crumbs.value).toEqual([]);
    expect(mention.open.value).toBe(true);
  });
});
