// apps/mirri-web/src/lib/approvalDisplay.ts
// The single mapper from an approval request's `display` payload onto the
// UI's ApprovalBlock union. The engine's wire dialect is the protocol
// `ToolInputDisplay` tagged union ({ kind: 'file_io' | 'goal_start' | ... },
// docs/specs/rust-core-design/01-protocol-unification.md). Legacy v1-daemon
// aliases (`shell`, `file_content`, `file_op`, `old_text`/`new_text`, `name`)
// are still read so an old backend's snapshots keep rendering.

import type { AppApprovalRequest } from '../api/types';
import type { ApprovalBlock, DiffLine } from '../types';

function buildDiffLines(before: string, after: string): DiffLine[] {
  const lines: DiffLine[] = [];
  before.split('\n').forEach((text, i) => {
    lines.push({ kind: 'rem', gutter: String(i + 1), text: `- ${text}` });
  });
  after.split('\n').forEach((text, i) => {
    lines.push({ kind: 'add', gutter: String(i + 1), text: `+ ${text}` });
  });
  return lines;
}

/** Build ApprovalBlock from AppApprovalRequest (discriminated union) */
export function buildApprovalBlock(a: AppApprovalRequest): ApprovalBlock {
  const d = (a.display ?? {}) as Record<string, unknown>;
  const kind = typeof d['kind'] === 'string' ? d['kind'] : '';
  const str = (key: string): string | undefined =>
    typeof d[key] === 'string' ? (d[key] as string) : undefined;
  // Honest expiry rides every variant (D-13): absent when the engine sent none.
  const expiry = a.expiresAt !== undefined ? { expiresAt: a.expiresAt } : {};
  // The engine approval-rule facet (B9) rides every variant the same way.
  const facet = a.approvalRule !== undefined ? { approvalRule: a.approvalRule } : {};

  // diff — protocol { path, before, after }, legacy { diff } / { old_text, new_text }
  if (kind === 'diff') {
    const path = str('path') ?? '';
    if (Array.isArray(d['diff'])) {
      return { kind: 'diff', path, diff: d['diff'] as DiffLine[], ...expiry, ...facet };
    }
    const before = str('before') ?? str('old_text');
    const after = str('after') ?? str('new_text');
    if (before !== undefined && after !== undefined) {
      return { kind: 'diff', path, diff: buildDiffLines(before, after), ...expiry, ...facet };
    }
    return { kind: 'diff', path, diff: [], ...expiry, ...facet };
  }

  // command — protocol kind `command`, legacy alias `shell`
  if (kind === 'command' || kind === 'shell') {
    return {
      kind: 'shell',
      command: str('command') ?? a.action,
      cwd: str('cwd'),
      danger: str('danger'),
      ...expiry,
      ...facet,
    };
  }

  // file_io — protocol { path, operation }: the write target preview
  if (kind === 'file_io') {
    return {
      kind: 'fileop',
      op: str('operation') ?? 'file',
      path: str('path') ?? '',
      detail: str('detail'),
      ...expiry,
      ...facet,
    };
  }

  // legacy file_content / file — full-content preview
  if (kind === 'file_content' || kind === 'file') {
    return {
      kind: 'file',
      path: str('path') ?? '',
      content: str('content') ?? '',
      language: str('language'),
      ...expiry,
      ...facet,
    };
  }

  // legacy file_op / fileop aliases
  if (kind === 'file_op' || kind === 'fileop') {
    return {
      kind: 'fileop',
      op: str('operation') ?? str('op') ?? kind,
      path: str('path') ?? '',
      detail: str('detail'),
      ...expiry,
      ...facet,
    };
  }

  // url_fetch / url
  if (kind === 'url_fetch' || kind === 'url') {
    return { kind: 'url', url: str('url') ?? a.action, method: str('method'), ...expiry, ...facet };
  }

  // search — protocol { query, scope? }
  if (kind === 'search') {
    return { kind: 'search', query: str('query') ?? a.action, scope: str('scope'), ...expiry, ...facet };
  }

  // agent_call / skill_call — protocol { agent_name | skill_name }, legacy { name }
  if (kind === 'agent_call' || kind === 'skill_call' || kind === 'invocation') {
    return {
      kind: 'invocation',
      kind2: kind,
      name: str('agent_name') ?? str('skill_name') ?? str('name') ?? a.toolName,
      description: str('prompt') ?? str('args') ?? str('description'),
      ...expiry,
      ...facet,
    };
  }

  // task_stop — protocol { task_id, task_description? }
  if (kind === 'task_stop') {
    return {
      kind: 'invocation',
      kind2: 'task_stop',
      name: str('task_id') ?? a.toolName,
      description: str('task_description'),
      ...expiry,
      ...facet,
    };
  }

  // todo / todo_list
  if (kind === 'todo' || kind === 'todo_list') {
    const rawItems = Array.isArray(d['items']) ? d['items'] : [];
    const items = rawItems.map((item: unknown) => {
      const it = (item ?? {}) as Record<string, unknown>;
      return {
        title: typeof it['title'] === 'string' ? it['title'] : '',
        status: typeof it['status'] === 'string' ? it['status'] : 'pending',
      };
    });
    return { kind: 'todo', items, ...expiry, ...facet };
  }

  // plan_review — finalised plan presented at plan-mode exit
  if (kind === 'plan_review') {
    const plan = str('plan') ?? '';
    const path = str('path');
    const rawOptions = Array.isArray(d['options']) ? d['options'] : [];
    const options = rawOptions
      .map((item: unknown): { label: string; description?: string } | null => {
        const it = (item ?? {}) as Record<string, unknown>;
        const label = typeof it['label'] === 'string' ? it['label'] : '';
        if (!label) return null;
        return {
          label,
          description: typeof it['description'] === 'string' ? it['description'] : undefined,
        };
      })
      .filter((o): o is { label: string; description?: string } => o !== null);
    return {
      kind: 'plan_review',
      plan,
      path,
      options: options.length > 0 ? options : undefined,
      ...expiry,
      ...facet,
    };
  }

  // goal_start — protocol { objective, completion_criterion? }
  if (kind === 'goal_start') {
    return {
      kind: 'goal',
      objective: str('objective') ?? a.action,
      completionCriterion: str('completion_criterion'),
      ...expiry,
      ...facet,
    };
  }

  // Unknown daemon display.kind → 'generic' with summary = action
  return { kind: 'generic', summary: a.action, ...expiry, ...facet };
}