// apps/mirri-web/src/lib/skillsManagement.test.ts
// Unit tests for the Skills-panel pure view logic (B8): filtering + extra-dir
// list normalization.

import { describe, expect, it } from 'vitest';
import { filterSkills, normalizeExtraDirs } from './skillsManagement';
import type { AppSkill } from '../api/types';

describe('filterSkills', () => {
  it('matches rows when the query hits the name or description, case-insensitively', () => {
    const rows: AppSkill[] = [
      { name: 'pdf', description: 'Handles PDFs', source: 'extra' },
      { name: 'greet', description: 'Warm greetings', source: 'project' },
    ];
    expect(filterSkills(rows, 'PDF').map((r) => r.name)).toEqual(['pdf']);
    expect(filterSkills(rows, 'warm').map((r) => r.name)).toEqual(['greet']);
  });

  it('keeps disabled rows and sorts by name when the query is blank (the panel shows state)', () => {
    const rows: AppSkill[] = [
      { name: 'zeta', description: '', source: 'user' },
      { name: 'alpha', description: '', source: 'user', enabled: false },
    ];
    expect(filterSkills(rows, '').map((r) => r.name)).toEqual(['alpha', 'zeta']);
  });
});

describe('normalizeExtraDirs', () => {
  it('trims, drops empties, and dedupes while preserving first-seen order when the input is messy', () => {
    expect(normalizeExtraDirs([' /a ', '', '/a', '/b'])).toEqual(['/a', '/b']);
  });

  it('answers an empty array when the input is all-empty (saving clears the key)', () => {
    expect(normalizeExtraDirs(['', '  '])).toEqual([]);
  });
});
