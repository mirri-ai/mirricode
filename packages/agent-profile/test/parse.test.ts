import { describe, it, expect } from 'vitest';
import { parseAgentFileText, AgentFileParseError } from '#/parse';

describe('parseAgentFileText', () => {
  it('should parse frontmatter and body when given a valid .md file', () => {
    const text = '---\nname: code-reviewer\ndescription: Code review specialist\n---\nYou are a code reviewer.';
    const result = parseAgentFileText({ path: '/agents/code-reviewer.md', text });
    expect(result.name).toBe('code-reviewer');
    expect(result.description).toBe('Code review specialist');
    expect(result.prompt).toBe('You are a code reviewer.');
  });

  it('should derive name from filename when frontmatter name is missing', () => {
    const text = '---\ndescription: Test agent\n---\nYou are a test agent.';
    const result = parseAgentFileText({ path: '/agents/my-agent.md', text });
    expect(result.name).toBe('my-agent');
  });

  it('should throw AgentFileParseError when name is not kebab-case', () => {
    const text = '---\nname: Bad_Name\ndescription: Test\n---\nBody.';
    expect(() => parseAgentFileText({ path: '/agents/bad.md', text })).toThrow(AgentFileParseError);
  });

  it('should throw AgentFileParseError when description is missing', () => {
    const text = '---\nname: reviewer\n---\nYou are a reviewer.';
    expect(() => parseAgentFileText({ path: '/agents/reviewer.md', text })).toThrow(AgentFileParseError);
  });

  it('should throw AgentFileParseError when frontmatter is missing entirely', () => {
    const text = 'Just some text without frontmatter.';
    expect(() => parseAgentFileText({ path: '/agents/no-fm.md', text })).toThrow(AgentFileParseError);
  });

  it('should parse .yaml file with prompt field as prompt body', () => {
    const text = 'name: reviewer\ndescription: Test\nprompt: You are a reviewer.\n';
    const result = parseAgentFileText({ path: '/agents/reviewer.yaml', text });
    expect(result.name).toBe('reviewer');
    expect(result.prompt).toBe('You are a reviewer.');
  });

  it('should map legacy systemPromptTemplate field to prompt when parsing .yaml', () => {
    const text = 'name: reviewer\ndescription: Test\nsystemPromptTemplate: You review code.\n';
    const result = parseAgentFileText({ path: '/agents/reviewer.yaml', text });
    expect(result.prompt).toBe('You review code.');
  });

  it('should use promptVars.roleAdditional as prompt when neither prompt nor systemPromptTemplate is set', () => {
    const text = [
      'name: reviewer',
      'description: Test',
      'promptVars:',
      '  roleAdditional: You are a code review specialist.',
    ].join('\n');
    const result = parseAgentFileText({ path: '/agents/reviewer.yaml', text });
    expect(result.prompt).toBe('You are a code review specialist.');
  });

  it('should fall back to default prompt when nothing is set', () => {
    const text = 'name: reviewer\ndescription: Test\n';
    const result = parseAgentFileText({ path: '/agents/reviewer.yaml', text });
    expect(result.prompt).toBe('You are a helpful assistant.');
  });

  it('should ignore legacy fields (extends, override, promptVars, capabilities, disallowedTools) in .yaml', () => {
    const text = [
      'name: reviewer',
      'description: Test',
      'extends: agent',
      'override: true',
      'promptVars:',
      '  roleAdditional: extra',
      'capabilities:',
      '  - code.read',
      'disallowedTools:',
      '  - Write',
      'prompt: You are a reviewer.',
    ].join('\n');
    const result = parseAgentFileText({ path: '/agents/reviewer.yaml', text });
    expect(result.prompt).toBe('You are a reviewer.');
    expect(result).not.toHaveProperty('extends');
    expect(result).not.toHaveProperty('override');
    expect(result).not.toHaveProperty('promptVars');
    expect(result).not.toHaveProperty('capabilities');
    expect(result).not.toHaveProperty('disallowedTools');
  });

  it('should convert legacy subagents Record to string array when parsing .yaml', () => {
    const text = [
      'name: agent',
      'description: Test',
      'subagents:',
      '  coder:',
      '    description: A coder',
      '  explore:',
      '    description: An explorer',
      'prompt: You are an agent.',
    ].join('\n');
    const result = parseAgentFileText({ path: '/agents/agent.yaml', text });
    expect(result.subagents).toEqual(['coder', 'explore']);
  });

  it('should parse .yml file same as .yaml', () => {
    const text = 'name: reviewer\ndescription: Test\nprompt: You are a reviewer.\n';
    const result = parseAgentFileText({ path: '/agents/reviewer.yml', text });
    expect(result.name).toBe('reviewer');
    expect(result.prompt).toBe('You are a reviewer.');
  });

  it('should parse tools and capabilitiesRequired fields from .md frontmatter', () => {
    const text = '---\nname: reviewer\ndescription: Test\ntools:\n  - Read\n  - Grep\ncapabilitiesRequired:\n  - code.read\n---\nYou are a reviewer.';
    const result = parseAgentFileText({ path: '/agents/reviewer.md', text });
    expect(result.tools).toEqual(['Read', 'Grep']);
    expect(result.capabilitiesRequired).toEqual(['code.read']);
  });

  it('should fill filePath and source from options when provided', () => {
    const text = '---\nname: reviewer\ndescription: Test\n---\nYou are a reviewer.';
    const result = parseAgentFileText({ path: '/agents/reviewer.md', text, source: 'user' });
    expect(result.filePath).toBe('/agents/reviewer.md');
    expect(result.source).toBe('user');
  });
});
