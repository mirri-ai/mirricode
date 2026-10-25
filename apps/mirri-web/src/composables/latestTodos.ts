// apps/mirri-web/src/composables/latestTodos.ts
// Derives the CURRENT todo list from a session transcript. The model manages
// todos via the TodoList tool: every write carries the FULL list in
// `input.todos` (an empty array clears it), and a call without `todos` is a
// read-only query. So the newest toolUse that carries a `todos` array is the
// current state — older calls and queries don't matter.

import type { AppMessage } from '../api/types';
import type { TodoView } from '../types';
import { latestTodosFromContent } from '../lib/todoStatus';

export { toStatus } from '../lib/todoStatus';

export function latestTodos(messages: AppMessage[]): TodoView[] {
  for (let i = messages.length - 1; i >= 0; i--) {
    const msg = messages[i]!;
    if (msg.role !== 'assistant') continue;
    const todos = latestTodosFromContent(msg.content);
    if (todos !== undefined) return todos;
  }
  return [];
}
