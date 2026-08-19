/**
 * Queued-prompt model + persisted sidecar for `queuedBySession`.
 *
 * A queued prompt is a user intent parked while the session is busy. Two
 * kinds:
 *  - `text`: a plain message (with optional attachments); flushed via
 *    submitPrompt when the turn ends.
 *  - `skill`: a user-slash skill activation (`/name args`); flushed via the
 *    skill-activation endpoint when the turn ends.
 *
 * The per-session queue is persisted to localStorage so a page reload or a
 * session re-open re-renders the queued items instead of silently dropping
 * them. Persistence is best-effort: storage failures are swallowed (the
 * in-memory queue is the source of truth during the session).
 */

import { safeGetJson, safeRemove, safeSetJson } from './storage';

export type QueuedPrompt =
  | { readonly kind: 'text'; readonly text: string; readonly attachments?: PromptAttachment[] }
  | { readonly kind: 'skill'; readonly skillName: string; readonly args?: string };

export interface PromptAttachment {
  readonly fileId: string;
  readonly kind: 'image' | 'video' | 'file';
  readonly name?: string;
  readonly mediaType?: string;
  readonly size?: number;
}

/** Storage keys: per session so switching sessions never conflates queues. */
export function queuedPromptsStorageKey(sessionId: string): string {
  return `mirri-web.queued.${sessionId}`;
}

export function loadQueuedPrompts(sessionId: string): QueuedPrompt[] | null {
  return safeGetJson<QueuedPrompt[]>(queuedPromptsStorageKey(sessionId));
}

export function saveQueuedPrompts(sessionId: string, prompts: readonly QueuedPrompt[]): void {
  safeSetJson(queuedPromptsStorageKey(sessionId), prompts);
}

export function removeQueuedPrompts(sessionId: string): void {
  safeRemove(queuedPromptsStorageKey(sessionId));
}