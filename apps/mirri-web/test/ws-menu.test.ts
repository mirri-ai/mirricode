// apps/mirri-web/test/ws-menu.test.ts
// Regression guard (audit 05): the workspace kebab menu only has two items
// (copy path / remove workspace) — a decorative separator there renders as a
// blank row for both sighted users and screen readers.

import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';

const sidebarSource = readFileSync(
  join(import.meta.dirname, '../src/components/Sidebar.vue'),
  'utf8',
);

/** Extract a `<Menu>…</Menu>` block that carries the given class. */
function menuBlock(source: string, className: string): string {
  const open = source.indexOf(`class="${className}"`);
  if (open === -1) throw new Error(`menu with class "${className}" not found`);
  const start = source.lastIndexOf('<Menu', open);
  const end = source.indexOf('</Menu>', open);
  return source.slice(start, end);
}

describe('workspace kebab menu', () => {
  it('should render no separator row when the two-item workspace menu markup is inspected', () => {
    const block = menuBlock(sidebarSource, 'ws-menu');
    expect(block, 'ws-menu still contains a separator:\n' + block).not.toContain('separator');
  });
});
