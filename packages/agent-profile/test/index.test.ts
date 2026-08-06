import { describe, it, expect } from 'vitest';
import { AgentFileParseError, AGENT_FILE_EXTENSIONS, AGENT_NAME_PATTERN } from '../src';

describe('agent-profile', () => {
  it('exports AgentFileParseError', () => {
    const err = new AgentFileParseError('test error');
    expect(err).toBeInstanceOf(Error);
    expect(err.name).toBe('AgentFileParseError');
    expect(err.message).toBe('test error');
  });

  it('exports AGENT_FILE_EXTENSIONS', () => {
    expect(AGENT_FILE_EXTENSIONS).toEqual(['.md', '.yaml', '.yml']);
  });

  it('exports AGENT_NAME_PATTERN', () => {
    expect(AGENT_NAME_PATTERN.test('code-reviewer')).toBe(true);
    expect(AGENT_NAME_PATTERN.test('Invalid')).toBe(false);
  });
});
