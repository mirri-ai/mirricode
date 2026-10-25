// apps/mirri-web/src/lib/todoStatus.ts
// Shared todo-list normalization + content scanning, used by BOTH the
// message-history derivation (latestTodos.ts), the session-snapshot mapper
// (api/daemon/ws-rpc-api.ts), and the event reducer's live fold — so the
// three sources can never disagree on a status spelling or a write's
// "did this call carry a full list?" test.

import type { TodoView } from '../types';
import { normalizeToolName } from './toolMeta';

export function toStatus(raw: unknown): TodoView['status'] {
  if (raw === 'in_progress') return 'in_progress';
  // Mirri's TodoList says 'done'; Claude-style TodoWrite says 'completed'.
  if (raw === 'done' || raw === 'completed') return 'done';
  return 'pending';
}

/** Map one parsed todo-item row onto the app `TodoView` shape. The row's
 *  title may live under `title` (Mirri) or `content` (Claude);
 *  status normalizes via [`toStatus`]. Returns `null` for rows without a
 *  usable title (dropped — the derivation's longstanding behavior). */
export function toTodoView(raw: unknown): TodoView | null {
  const item = (raw ?? {}) as Record<string, unknown>;
  const title =
    typeof item['title'] === 'string'
      ? item['title']
      : typeof item['content'] === 'string'
        ? item['content']
        : '';
  if (!title) return null;
  return { title, status: toStatus(item['status']) };
}

/** Extract the todo list from a TodoList/TodoWrite tool-use frame's input —
 *  `undefined` when the frame is a read-only query or not a todo tool. */
export function todosFromToolInput(input: unknown, toolName: string): TodoView[] | undefined {
  if (normalizeToolName(toolName) !== 'todo') return undefined;
  if (typeof input === 'string') {
    try {
      input = JSON.parse(input);
    } catch {
      return undefined;
    }
  }
  const obj = (input ?? {}) as { todos?: unknown };
  if (!Array.isArray(obj.todos)) return undefined; // read-only query — no state
  const views: TodoView[] = [];
  for (const raw of obj.todos) {
    const view = toTodoView(raw);
    if (view !== null) views.push(view);
  }
  return views;
}

/** Scan message CONTENT (the `AppMessageContent[]` union's toolUse parts,
 *  newest first) for the newest TodoList write — the exact scan the
 *  `latestTodos` message derivation performs, but over a content array
 *  instead of a message list. `undefined` = no todo-carrying write found;
 *  an empty array = an explicit clear (a valid, distinct state). */
export function latestTodosFromContent(content: ReadonlyArray<{ type: string; toolName?: string; input?: unknown }>): TodoView[] | undefined {
  for (let j = content.length - 1; j >= 0; j--) {
    const c = content[j]!;
    if (c.type !== 'toolUse') continue;
    if (c.toolName === undefined) continue;
    const todos = todosFromToolInput(c.input, c.toolName);
    if (todos !== undefined) return todos;
  }
  return undefined;
}
