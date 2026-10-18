// apps/mirri-web/test/rule-facet.test.ts
// Given the engine's rule pattern DSL (rust `permission/rule_match.rs`), the
// web preview mirror must decompose the same shapes: bare tool names, tool
// globs, optional `!` negation, and `Tool(arg)` arg patterns (empty parens =
// none). It is a PREVIEW only — the authoritative validation lives in the
// engine (parse_pattern); the panel uses this to show what a rule matches
// and to pre-check drafts before submitting them.

import { describe, expect, it } from 'vitest';

import { parseRuleFacet } from '../src/lib/ruleFacet';

describe('parseRuleFacet (preview mirror of rust rule_match)', () => {
  it('should parse a bare tool name', () => {
    expect(parseRuleFacet('Bash')).toEqual({ tool: 'Bash', negated: false });
  });

  it('should parse tool with arg pattern', () => {
    expect(parseRuleFacet('Write(/foo/**)')).toEqual({ tool: 'Write', argPattern: '/foo/**', negated: false });
  });

  it('should parse empty parens as no arg pattern', () => {
    expect(parseRuleFacet('Tool()')).toEqual({ tool: 'Tool', negated: false });
  });

  it('should parse negation and glob tool names', () => {
    expect(parseRuleFacet('!mcp__github__*')).toEqual({ tool: 'mcp__github__*', negated: true });
  });

  it('should answer null for an unclosed paren', () => {
    expect(parseRuleFacet('Write(/foo')).toBeNull();
  });

  it('should answer null for the empty pattern', () => {
    expect(parseRuleFacet('')).toBeNull();
    expect(parseRuleFacet('   ')).toBeNull();
  });

  it('should answer null when no tool name is left (engine rejects nameless tools)', () => {
    expect(parseRuleFacet('()')).toBeNull();
    expect(parseRuleFacet('!')).toBeNull();
  });

  it('should keep whitespace inside parens as part of the arg pattern', () => {
    expect(parseRuleFacet('Read( /a/** )')).toEqual({ tool: 'Read', argPattern: '/a/**', negated: false });
  });
});
