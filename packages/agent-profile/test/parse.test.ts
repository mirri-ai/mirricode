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

  it('should accept a name that is exactly AGENT_NAME_MAX_LENGTH characters', () => {
    const name = 'a'.repeat(64);
    const text = `---\nname: ${name}\ndescription: Test\n---\nBody.`;
    const result = parseAgentFileText({ path: '/agents/long.md', text });
    expect(result.name).toBe(name);
  });

  it('should throw AgentFileParseError when name exceeds AGENT_NAME_MAX_LENGTH', () => {
    const name = 'a'.repeat(65);
    const text = `---\nname: ${name}\ndescription: Test\n---\nBody.`;
    expect(() => parseAgentFileText({ path: '/agents/long.md', text })).toThrow(
      'must be at most 64 characters',
    );
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

  it('should ignore legacy fields (extends, override, promptVars, capabilities) in .yaml', () => {
    const text = [
      'name: reviewer',
      'description: Test',
      'extends: agent',
      'override: true',
      'promptVars:',
      '  roleAdditional: extra',
      'capabilities:',
      '  - code.read',
      'prompt: You are a reviewer.',
    ].join('\n');
    const result = parseAgentFileText({ path: '/agents/reviewer.yaml', text });
    expect(result.prompt).toBe('You are a reviewer.');
    expect(result).not.toHaveProperty('extends');
    expect(result).not.toHaveProperty('override');
    expect(result).not.toHaveProperty('promptVars');
    expect(result).not.toHaveProperty('capabilities');
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

  it('should parse disallowedTools from .md frontmatter when present', () => {
    const text =
      '---\nname: reviewer\ndescription: Test\ndisallowedTools:\n  - Write\n  - Edit\n---\nYou are a reviewer.';
    const result = parseAgentFileText({ path: '/agents/reviewer.md', text });
    expect(result.disallowedTools).toEqual(['Write', 'Edit']);
  });

  it('should parse the disallowed_tools snake_case alias when disallowedTools is absent', () => {
    const text =
      '---\nname: reviewer\ndescription: Test\ndisallowed_tools:\n  - Write\n---\nYou are a reviewer.';
    const result = parseAgentFileText({ path: '/agents/reviewer.md', text });
    expect(result.disallowedTools).toEqual(['Write']);
  });

  it('should treat a missing disallowedTools key as undefined', () => {
    const text = '---\nname: reviewer\ndescription: Test\n---\nYou are a reviewer.';
    const result = parseAgentFileText({ path: '/agents/reviewer.md', text });
    expect(result.disallowedTools).toBeUndefined();
  });

  it('should fill filePath and source from options when provided', () => {
    const text = '---\nname: reviewer\ndescription: Test\n---\nYou are a reviewer.';
    const result = parseAgentFileText({ path: '/agents/reviewer.md', text, source: 'user' });
    expect(result.filePath).toBe('/agents/reviewer.md');
    expect(result.source).toBe('user');
  });

  it('should parse enabled: false from the frontmatter', () => {
    const def = parseAgentFileText({
      path: 'reviewer.md',
      source: 'user',
      text: '---\nname: reviewer\ndescription: Reviews code\nenabled: false\n---\nYou review.',
    });
    expect(def.enabled).toBe(false);
  });

  it('should treat a missing enabled key as enabled', () => {
    const def = parseAgentFileText({
      path: 'reviewer.md',
      source: 'user',
      text: '---\nname: reviewer\ndescription: Reviews code\n---\nYou review.',
    });
    expect(def.enabled).toBeUndefined();
  });

  it('should ignore a non-boolean enabled value', () => {
    const def = parseAgentFileText({
      path: 'strict.md',
      source: 'user',
      text: '---\nname: strict\ndescription: d\nenabled: "false"\n---\np',
    });
    expect(def.enabled).toBeUndefined();
  });
});
