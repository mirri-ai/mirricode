// apps/mirri-web/src/composables/useMentionMenu.ts
import { ref } from 'vue';
import type { FileCandidate, SearchFilesResult } from '../api/types';
import type { TextSource } from './useTextSource';

export interface MentionMenuDeps {
  /** The editor-adapter the @token is read from and rewritten on select. */
  textSource: TextSource;
  /** File search for the @-query (getter; undefined disables the menu). */
  searchFiles: () => ((q: string) => Promise<SearchFilesResult>) | undefined;
  /** E9 capability: false = the backend has no structured-reference wire
   *  (v1/v2) — picking a candidate inserts the plain-text `@path` spelling
   *  instead of a fileMention chip (the reference channel is v3-only). */
  supportsFileReferences?: () => boolean;
}

/** The action a menu activation requested (E9 uiux §3 keyboard contract). */
export type MentionSelectAction = 'pick' | 'drill';

interface MentionToken {
  /** The query after `@` (no @, no quote marks). */
  token: string;
  start: number;
  end: number;
}

const DEBOUNCE_MS = 150;

/**
 * `@` file-mention menu (E9 rewrite): quoted-token detection, debounced search
 * with stale-result cancellation, Esc dismissal memory, drill/pick dual
 * actions, and breadcrumb jumps.
 *
 * The composer keeps the keydown orchestration (arrow keys, Enter/Tab/Escape)
 * because it also juggles the slash menu and history recall; this composable
 * owns everything else:
 * - `update()` scans the token under the caret. `@"a b"` (unclosed quote)
 *   keeps spaces inside the token; a closed quote terminates it. A dismissed
 *   token stays closed until the user actually types again (Esc memory).
 * - `select(item, 'drill')` rewrites the token to `@dir/` and keeps the menu
 *   open for the next path segment; crumbs record the walked segments.
 * - `select(item, 'pick')` removes the token and inserts a `fileMention`
 *   chip through `textSource.insertFileMention` (only inserted chips count
 *   as references — a hand-typed `@xxx` never does).
 *
 * When `searchFiles` reports no `indexState` capability field the result is
 * treated as the v2 degradation: the menu falls back to plain-text insertion
 * (see the plan's v2 fallback note, mirroring the requirement §10 registry).
 */
export function useMentionMenu(deps: MentionMenuDeps) {
  const { textSource, searchFiles, supportsFileReferences } = deps;

  const open = ref(false);
  const items = ref<FileCandidate[]>([]);
  const active = ref(0);
  const loading = ref(false);
  const indexState = ref<SearchFilesResult['indexState']>('ready');
  const drilled = ref(false);
  const crumbs = ref<string[]>([]);

  // Debounce timer + per-request sequence guard: a result from request #n is
  // dropped when request #n+1 has already been issued (fast typing).
  let timer: ReturnType<typeof setTimeout> | null = null;
  let requestSeq = 0;
  // Esc memory (uiux §1.2/§3): the dismissed token (start + query at
  // dismissal). A token starting at the same offset whose query is a PREFIX
  // of the dismissed query is the same token seen with the caret inside it —
  // it stays closed; anything else (typing extended/changed it) reopens.
  let dismissed: { start: number; token: string } | null = null;

  /**
   * Find the @token under the cursor in the current text value. Returns null
   * if none. `@"a b"` keeps spaces while the quote is unclosed; after the
   * closing quote the token ends.
   */
  function getMentionToken(): MentionToken | null {
    const val = textSource.getText();
    const pos = textSource.getCaret();
    // Fast path: the word before the caret (no spaces).
    let start = pos - 1;
    while (start >= 0 && !/\s/.test(val[start]!)) {
      start--;
    }
    start++;
    const tokenPart = val.slice(start, pos);
    if (tokenPart.startsWith('@')) {
      const rest = tokenPart.slice(1);
      if (rest.startsWith('"')) {
        // Quoted form @"a b": while the quote is open, spaces belong to the
        // token; once closed, the token is the quoted span minus the quotes.
        return { token: rest.replaceAll('"', ''), start, end: pos };
      }
      return { token: rest, start, end: pos };
    }
    // The word before the caret contains a space — but an OPEN quoted token
    // may span spaces: find the last `@"` before the caret and check whether
    // its quote is still open here.
    const at = val.lastIndexOf('@"', pos - 1);
    if (at !== -1) {
      const span = val.slice(at, pos);
      if (countChar(span, '"') % 2 === 1) {
        return { token: span.slice(2), start: at, end: pos };
      }
    }
    return null;
  }

  /** Search now for the current query (shared by update() and drill/goCrumb). */
  function searchNow(query: string): void {
    const search = searchFiles();
    if (!search) return;
    const seq = ++requestSeq;
    loading.value = true;
    open.value = true;
    active.value = 0;
    search(query)
      .then((r) => {
        if (seq !== requestSeq) return; // stale — a newer query is in flight
        items.value = r.items;
        indexState.value = r.indexState ?? 'ready';
      })
      .catch(() => {
        if (seq !== requestSeq) return;
        items.value = [];
        indexState.value = 'ready';
      })
      .finally(() => {
        if (seq === requestSeq) loading.value = false;
      });
  }

  function update(): void {
    const mt = getMentionToken();
    const search = searchFiles();
    if (!mt || !search) {
      open.value = false;
      loading.value = false;
      requestSeq++; // cancel any in-flight result
      return;
    }
    // Esc memory: the same token (same offset, query a prefix of the
    // dismissed query = caret sitting inside it) stays closed; a changed or
    // extended query reopens.
    if (
      dismissed &&
      dismissed.start === mt.start &&
      (dismissed.token === mt.token || dismissed.token.startsWith(mt.token))
    ) {
      open.value = false;
      return;
    }
    dismissed = null;
    if (timer !== null) clearTimeout(timer);
    timer = setTimeout(() => searchNow(mt.token), DEBOUNCE_MS);
  }

  /** Close the menu and remember the dismissed token (Esc branch). */
  function dismiss(): void {
    const mt = getMentionToken();
    if (mt) dismissed = { start: mt.start, token: mt.token };
    open.value = false;
    loading.value = false;
    requestSeq++;
  }

  /** Remove the token and insert the chip (pick action). A chip for the
   *  same path already in the document is NOT inserted again — the pick
   *  just closes the menu (review Minor 1). v2 (no structured references)
   *  degrades to the plain-text `@path` spelling — the chip form is a
   *  v3 contract, a hand-typed spelling is honest plain text there. */
  function pick(item: FileCandidate, mt: MentionToken): void {
    const structured = supportsFileReferences?.() !== false && textSource.insertFileMention !== undefined;
    if (structured) {
      if (textSource.hasFileMention?.(item.path)) {
        textSource.replaceRange(mt.start, mt.end, '');
      } else {
        textSource.replaceRange(mt.start, mt.end, '');
        textSource.insertFileMention?.({
          display: item.name,
          path: item.path,
          kind: item.kind === 'directory' ? 'folder' : 'file',
          ...(item.root ? { root: item.root } : undefined),
        });
      }
    } else {
      textSource.replaceRange(mt.start, mt.end, `@${item.path}${item.kind === 'directory' ? '/' : ''}`);
    }
    open.value = false;
    drilled.value = false;
    crumbs.value = [];
    dismissed = null;
  }

  /** Rewrite the token to the directory prefix and keep searching (drill). */
  function drill(item: FileCandidate, mt: MentionToken): void {
    // The rewritten prefix `@dir/` — the segments before the current token's
    // last segment become the breadcrumb trail.
    const prevSegments = mt.token.split('/').filter((s) => s.length > 0);
    crumbs.value = prevSegments.slice(0, -1);
    textSource.replaceRange(mt.start, mt.end, `@${item.path}/`);
    drilled.value = true;
    open.value = true;
    // Re-search the drilled prefix immediately (no debounce: this is a
    // structural rewrite, not typing).
    searchNow(`${item.path}/`);
  }

  /** Select the item: 'pick' inserts the chip, 'drill' descends a directory. */
  function select(item: FileCandidate, action: MentionSelectAction = 'pick'): void {
    const mt = getMentionToken();
    if (!mt) return;
    if (action === 'drill' && item.kind === 'directory') {
      drill(item, mt);
      return;
    }
    pick(item, mt);
  }

  /** Jump the token back to the breadcrumb at level `index` (0 = root). */
  function goCrumb(index: number): void {
    const mt = getMentionToken();
    if (!mt) return;
    const segments = mt.token.split('/').filter((s) => s.length > 0);
    const prefixSegments = segments.slice(0, Math.max(0, index + 1));
    const prefix = prefixSegments.length > 0 ? `${prefixSegments.join('/')}/` : '';
    textSource.replaceRange(mt.start, mt.end, `@${prefix}`);
    crumbs.value = segments.slice(0, Math.max(0, index)).slice(0, index);
    drilled.value = prefixSegments.length > 0;
    open.value = true;
    searchNow(prefix);
  }

  return {
    open,
    items,
    active,
    loading,
    indexState,
    drilled,
    crumbs,
    dismiss,
    goCrumb,
    update,
    select,
  };
}

function countChar(s: string, ch: string): number {
  let n = 0;
  for (const c of s) if (c === ch) n++;
  return n;
}
