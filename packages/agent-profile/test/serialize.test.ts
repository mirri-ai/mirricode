import { describe, it, expect } from 'vitest';
import { serializeAgentFile } from '#/serialize';
import { parseAgentFileText } from '#/parse';
import type { AgentFileDef } from '#/schema';

describe('serializeAgentFile', () => {
  it('should produce .md frontmatter format when given a complete AgentFileDef', () => {
    const def: AgentFileDef = {
      name: 'code-reviewer',
      description: 'Code review specialist',
      prompt: 'You are a code reviewer.',
    };
    const text = serializeAgentFile(def);
    expect(text.startsWith('---\n')).toBe(true);
    expect(text).toContain('name: code-reviewer');
    expect(text).toContain('description: Code review specialist');
    expect(text).toContain('You are a code reviewer.');
  });

  it('should omit optional fields when they are undefined', () => {
    const def: AgentFileDef = {
      name: 'simple',
      description: 'Simple agent',
      prompt: 'You are simple.',
    };
    const text = serializeAgentFile(def);
    expect(text).not.toContain('whenToUse');
    expect(text).not.toContain('defaultModel');
    expect(text).not.toContain('tools');
    expect(text).not.toContain('subagents');
    expect(text).not.toContain('capabilitiesRequired');
  });

  it('should serialize all optional fields when present', () => {
    const def: AgentFileDef = {
      name: 'full',
      description: 'Full agent',
      whenToUse: 'Use when needed',
      defaultModel: 'claude-sonnet',
      tools: ['Read', 'Grep'],
      subagents: ['coder'],
      capabilitiesRequired: ['code.read'],
      prompt: 'You are full.',
    };
    const text = serializeAgentFile(def);
    expect(text).toContain('whenToUse: Use when needed');
    expect(text).toContain('defaultModel: claude-sonnet');
    expect(text).toContain('Read');
    expect(text).toContain('Grep');
    expect(text).toContain('coder');
    expect(text).toContain('code.read');
  });

  it('should round-trip: serialize → parse preserves all fields', () => {
    const original: AgentFileDef = {
      name: 'round-trip',
      description: 'Round trip test',
      whenToUse: 'Use for testing',
      defaultModel: 'test-model',
      tools: ['Read', 'Write'],
      subagents: ['coder', 'explore'],
      capabilitiesRequired: ['code.read', 'code.write'],
      prompt: 'You are a round-trip test agent.',
    };
    const text = serializeAgentFile(original);
    const reparsed = parseAgentFileText({ path: '/agents/round-trip.md', text });
    expect(reparsed.name).toBe(original.name);
    expect(reparsed.description).toBe(original.description);
    expect(reparsed.whenToUse).toBe(original.whenToUse);
    expect(reparsed.defaultModel).toBe(original.defaultModel);
    expect(reparsed.tools).toEqual(original.tools);
    expect(reparsed.subagents).toEqual(original.subagents);
    expect(reparsed.capabilitiesRequired).toEqual(original.capabilitiesRequired);
    expect(reparsed.prompt).toBe(original.prompt);
  });

  it('should serialize prompt as body, not as frontmatter field', () => {
    const def: AgentFileDef = {
      name: 'test',
      description: 'Test',
      prompt: 'You are a test agent.',
    };
    const text = serializeAgentFile(def);
    // The prompt should appear after the closing fence, not inside it.
    const closeIndex = text.indexOf('---', 3);
    const frontmatter = text.substring(3, closeIndex);
    expect(frontmatter).not.toContain('prompt:');
    const body = text.substring(closeIndex + 3).trimStart();
    expect(body).toBe('You are a test agent.');
  });
});
