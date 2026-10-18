/**
 * Cross-engine consistency acceptance (R2-4): the TS `parseAgentFileText` and
 * the rust `parse_agent_file_text` must agree field-by-field on the shared
 * fixture corpus in `test/fixtures/cross-engine/corpus/`.
 *
 * This suite parses every corpus file and compares a normalized JSON form
 * against `test/fixtures/cross-engine/expected-outputs.json` — the SAME file
 * the rust side (`rust/crates/mirri-core/tests/agent_md_cross_engine_tests.rs`)
 * compares against. Regenerate expectations with:
 *
 *   UPDATE_CROSS_ENGINE_SNAPSHOT=1 npx vitest run
 *
 * Normalization convention (see the fixture README for the full contract):
 * - success: `{ ok: true, def: { … } }` with the eleven AgentFileDef payload
 *   fields, keys in alphabetical order, absent optional fields as explicit
 *   `null` (never omitted);
 * - failure: `{ ok: false, error: "<message>" }`;
 * - `source` is excluded (both sides parse without a discovery source);
 * - each fixture is parsed with the synthetic path `agents/<file-name>`.
 */

import { describe, it, expect } from 'vitest';
import { readdirSync, readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseAgentFileText, AgentFileParseError } from '#/parse';
import type { AgentFileDef } from '#/schema';

const fixtureRoot = fileURLToPath(new URL('./fixtures/cross-engine', import.meta.url));
const corpusDir = join(fixtureRoot, 'corpus');
const expectedPath = join(fixtureRoot, 'expected-outputs.json');
const updateSnapshot = process.env['UPDATE_CROSS_ENGINE_SNAPSHOT'] === '1';

function listCorpus(): string[] {
  return readdirSync(corpusDir)
    .filter((name) => name.endsWith('.md'))
    .toSorted();
}

/** Normalizes a parsed def: alphabetical keys, undefined → explicit null. */
function normalizeDef(def: AgentFileDef): Record<string, unknown> {
  return {
    ok: true,
    def: {
      capabilitiesRequired: def.capabilitiesRequired ?? null,
      defaultModel: def.defaultModel ?? null,
      description: def.description,
      disallowedTools: def.disallowedTools ?? null,
      enabled: def.enabled ?? null,
      filePath: def.filePath ?? null,
      name: def.name,
      prompt: def.prompt,
      subagents: def.subagents ?? null,
      tools: def.tools ?? null,
      whenToUse: def.whenToUse ?? null,
    },
  };
}

function parseFixture(fileName: string): Record<string, unknown> {
  const text = readFileSync(join(corpusDir, fileName), 'utf8');
  const path = `agents/${fileName}`;
  try {
    return normalizeDef(parseAgentFileText({ path, text }));
  } catch (error) {
    if (error instanceof AgentFileParseError) {
      return { ok: false, error: error.message };
    }
    throw error;
  }
}

function buildActual(): Record<string, unknown> {
  const actual: Record<string, unknown> = {};
  for (const fileName of listCorpus()) {
    actual[fileName] = parseFixture(fileName);
  }
  return actual;
}

describe('cross-engine fixture corpus (R2-4)', () => {
  it('parses every fixture identically to the shared expected-outputs.json', () => {
    const actual = buildActual();

    if (updateSnapshot) {
      mkdirSync(dirname(expectedPath), { recursive: true });
      writeFileSync(expectedPath, `${JSON.stringify(actual, null, 2)}\n`, 'utf8');
    }

    const expected: Record<string, unknown> = JSON.parse(
      readFileSync(expectedPath, 'utf8'),
    );

    // Compare fixture-by-fixture so a mismatch names the offending file.
    const expectedFiles = Object.keys(expected).toSorted();
    const actualFiles = Object.keys(actual).toSorted();
    expect(actualFiles, 'corpus files covered by expected-outputs.json').toEqual(
      expectedFiles,
    );
    for (const fileName of expectedFiles) {
      expect(
        actual[fileName],
        `fixture ${fileName}: TS output differs from expected-outputs.json (if intentional, regenerate with UPDATE_CROSS_ENGINE_SNAPSHOT=1)`,
      ).toEqual(expected[fileName]);
    }
  });
});

/**
 * Independent semantic assertions on key corpus samples. These exist
 * alongside (not instead of) the shared snapshot: they state the expected
 * parsing behavior explicitly, so a regression that would need the snapshot
 * regenerated (UPDATE_CROSS_ENGINE_SNAPSHOT=1) still fails here.
 */
describe('corpus samples (semantic assertions)', () => {
  function parseCorpus(fileName: string): AgentFileDef {
    const text = readFileSync(join(corpusDir, fileName), 'utf8');
    return parseAgentFileText({ path: `agents/${fileName}`, text });
  }

  it('should parse every supported frontmatter field when given the full-fields sample', () => {
    const def = parseCorpus('full-fields.md');

    expect(def.name).toBe('full-fields');
    expect(def.description).toBe('A profile exercising every supported frontmatter field.');
    expect(def.prompt).toBe('You are a full-surface test agent.\n\nSecond paragraph of the prompt.');
    expect(def.whenToUse).toBe('Use for full-surface testing of the agent profile parser.');
    expect(def.defaultModel).toBe('claude-sonnet');
    expect(def.tools).toEqual(['Read', 'Grep', 'Glob']);
    expect(def.disallowedTools).toEqual(['Write', 'Edit']);
    expect(def.subagents).toEqual(['coder', 'explore']);
    expect(def.capabilitiesRequired).toEqual(['code.read', 'code.explore']);
    expect(def.enabled).toBe(false);
  });

  it('should leave every optional field unset when given the minimal sample', () => {
    const def = parseCorpus('minimal.md');

    expect(def.name).toBe('minimal');
    expect(def.description).toBe('The smallest valid agent profile file.');
    expect(def.prompt).toBe('You are a minimal agent.');
    expect(def.whenToUse).toBeUndefined();
    expect(def.defaultModel).toBeUndefined();
    expect(def.tools).toBeUndefined();
    expect(def.disallowedTools).toBeUndefined();
    expect(def.subagents).toBeUndefined();
    expect(def.capabilitiesRequired).toBeUndefined();
    expect(def.enabled).toBeUndefined();
  });

  it('should resolve snake_case field aliases when the camelCase key is absent', () => {
    const def = parseCorpus('snake-case-aliases.md');

    expect(def.defaultModel).toBe('claude-haiku');
    expect(def.disallowedTools).toEqual(['Bash', 'WebFetch']);
    expect(def.capabilitiesRequired).toEqual(['code.read']);
  });

  it('should split, trim and de-empty comma-separated list strings when given the comma-lists sample', () => {
    const def = parseCorpus('comma-lists.md');

    expect(def.tools).toEqual(['Read', 'Grep', 'Glob']);
    expect(def.disallowedTools).toEqual(['Write', 'Edit']);
  });

  it('should trim scalar strings but keep block-list items verbatim when given the roundtrip-hostile sample', () => {
    const def = parseCorpus('roundtrip-hostile.md');

    // The quoted scalar keeps its meaning but the surrounding padding is
    // trimmed away by the string normalization.
    expect(def.whenToUse).toBe('padded with leading and trailing spaces');
    // A block-list item, even one that parses as a quoted string, is kept
    // exactly as written — including its padding and YAML type-like text.
    expect(def.tools).toEqual([
      'tool:with:colons',
      '#leading-hash',
      ' padded item ',
      'true',
      'null',
      '42',
    ]);
    expect(def.defaultModel).toBe('claude-sonnet');
  });

  it('should parse the plain user profile sample into its explicit field values', () => {
    const text = readFileSync(
      join(fixtureRoot, '..', 'cross-engine-list', 'user-agents', 'plain.md'),
      'utf8',
    );
    const def = parseAgentFileText({ path: 'agents/plain.md', text });

    expect(def.name).toBe('plain');
    expect(def.description).toBe('A plain user profile with no conflicts.');
    expect(def.prompt).toBe('You are a plain user-level agent.');
    expect(def.filePath).toBe('agents/plain.md');
    // No conflicts → no optional field is set by the file.
    expect(def.enabled).toBeUndefined();
    expect(def.tools).toBeUndefined();
    expect(def.disallowedTools).toBeUndefined();
    expect(def.whenToUse).toBeUndefined();
    expect(def.defaultModel).toBeUndefined();
    expect(def.subagents).toBeUndefined();
    expect(def.capabilitiesRequired).toBeUndefined();
  });
});
