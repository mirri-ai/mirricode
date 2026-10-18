// apps/mirri-web/src/composables/useComposerDraft.ts
import { ref, watch } from 'vue';
import { draftStorageKey, safeGetString, safeRemove, safeSetString } from '../lib/storage';

export interface ComposerDraftDeps {
  /** Active session id — scopes the persisted draft (getter for reactivity). */
  sessionId: () => string | undefined;
}

/**
 * The composer's per-session unsent-draft persistence.
 *
 * The draft is kept in localStorage keyed by session, so switching away and
 * back (or a page refresh) restores whatever the user was typing for that
 * session; it is cleared when the draft is sent/steered. This composable owns
 * the draft string state (`text`) and the load/save/switch watchers; the
 * editor host (Composer.vue) parses the string into the TipTap doc on load and
 * serializes back on every update — the string, not the doc, is the persisted
 * form, so drafts stay compatible across versions.
 */
export function useComposerDraft(deps: ComposerDraftDeps) {
  const { sessionId } = deps;

  function loadDraft(sid: string | undefined): string {
    return safeGetString(draftStorageKey(sid)) ?? '';
  }
  function saveDraft(sid: string | undefined, value: string): void {
    const key = draftStorageKey(sid);
    if (value) safeSetString(key, value);
    else safeRemove(key);
  }

  const text = ref(loadDraft(sessionId()));

  // Persist the live draft whenever it changes (empty clears the entry). The
  // editor host keeps `text` in lockstep with the serialized doc, so this
  // watcher is the single persistence sink.
  watch(text, (value) => {
    saveDraft(sessionId(), value);
  });

  // Switching sessions: stash the draft under the OLD session, then load the new
  // session's draft into the box.
  watch(sessionId, (newSid, oldSid) => {
    if (newSid === oldSid) return;
    saveDraft(oldSid, text.value);
    text.value = loadDraft(newSid);
  });

  /** Imperatively load text into the box for editing (used by "edit & resend the
      last message" after an undo, or by the dock queue panel when the user edits
      a queued prompt). The editor host applies + focuses with the caret at the
      end. */
  function loadForEdit(value: string): void {
    text.value = value;
  }

  /**
   * Synchronously clear the persisted draft for the current session.
   * Call this right after clearing the editor on send/steer; relying on the
   * text watcher is unsafe because the Composer may unmount before the watcher
   * flushes (e.g. when the optimistic user message replaces the empty-session
   * composer), causing the next mount to reload the stale draft.
   */
  function clearDraft(): void {
    saveDraft(sessionId(), '');
  }

  return { text, loadForEdit, clearDraft };
}
