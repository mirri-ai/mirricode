// apps/mirri-web/src/lib/skillsManagement.ts
// Pure TS — no Vue. Skills-panel view logic: filtering + extra-dir list
// normalization (dedupe / trim / drop empty / preserve order).

import type { AppSkill } from '../api/types';

/** Filter by case-insensitive substring on name/description, name-first sort.
 *  Disabled rows are kept — the panel shows the state instead of hiding it. */
export function filterSkills(
  skills: ReadonlyArray<AppSkill>,
  query: string,
): AppSkill[] {
  const q = query.trim().toLowerCase();
  const rows = q
    ? skills.filter(
        (s) => s.name.toLowerCase().includes(q) || s.description.toLowerCase().includes(q),
      )
    : [...skills];
  return rows.toSorted((a, b) => a.name.localeCompare(b.name));
}

/** Normalize a user-entered extra-dir list: trim, drop empties, dedupe
 *  case-sensitively (paths), preserve first-seen order. */
export function normalizeExtraDirs(
  dirs: ReadonlyArray<string>,
): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const raw of dirs) {
    const dir = raw.trim();
    if (dir === '' || seen.has(dir)) continue;
    seen.add(dir);
    out.push(dir);
  }
  return out;
}

/** The known source tiers the panel maps onto badge labels (wire vocabulary);
 *  sources outside this list render verbatim so new engine scopes stay
 *  visible. */
export const SKILL_SOURCE_ORDER = ['project', 'user', 'extra', 'builtin'] as const;
