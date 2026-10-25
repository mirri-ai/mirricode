/**
 * Base-prompt template mirror guard: the TS `system.md` and the rust
 * `system_prompt.md` are documented mirrors (the rust file's module docs
 * declare the policy — placeholder syntax differs, content must not).
 * Both engines emit the template through the `base_prompt_default_template`
 * wire field, so a content drift means the two engines hand the settings
 * page different default templates — a cross-engine inconsistency, not a
 * cosmetic one.
 *
 * This suite fails when the two files drift apart (ignoring the one
 * sanctioned difference: `{{var}}` rust vs `${var}` TS placeholder
 * spelling). Whichever file is edited, mirror the change in the other.
 */

import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const tsTemplatePath = fileURLToPath(
  new URL('../../src/app/agentProfileCatalog/system.md', import.meta.url),
);
// packages/agent-core-v2/test/lint -> packages/agent-core-v2 -> packages -> repo root
const repoRoot = join(dirname(fileURLToPath(import.meta.url)), '..', '..', '..', '..');
const rustTemplatePath = join(
  repoRoot,
  'rust/crates/mirri-core/src/scope/system_prompt.md',
);

/** Normalize the rust template's `{{var}}` spelling to the TS `${var}` one. */
function normalizeRustPlaceholders(text: string): string {
  return text.replace(/\{\{(\w+)\}\}/g, '${$1}');
}

describe('base prompt template mirror', () => {
  it('should match the rust mirror exactly when placeholder spelling is normalized', () => {
    const ts = readFileSync(tsTemplatePath, 'utf8');
    const rust = readFileSync(rustTemplatePath, 'utf8');

    expect(normalizeRustPlaceholders(rust)).toBe(ts);
  });
});
