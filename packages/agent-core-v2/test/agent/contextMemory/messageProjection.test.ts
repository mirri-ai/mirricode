import { describe, expect, it } from 'vitest';
import { toProtocolMessage } from '#/agent/contextMemory/messageProjection';

describe('toProtocolMessage with_skills projection (ADR-006)', () => {
  it('should project with_skills onto the wire message', () => {
    const msg = {
      role: 'user' as const,
      content: [{ type: 'text' as const, text: '[review] the code' }],
      toolCalls: [],
      with_skills: [{ name: 'review', target: '/skills/review/SKILL.md', trigger: 'user-inline' }],
    };
    const out = toProtocolMessage('s1', 0, msg, 0);
    expect(out.with_skills).toEqual([
      { name: 'review', target: '/skills/review/SKILL.md', trigger: 'user-inline' },
    ]);
  });

  it('should omit with_skills when the context message has none', () => {
    const msg = {
      role: 'user' as const,
      content: [{ type: 'text' as const, text: 'plain' }],
      toolCalls: [],
    };
    const out = toProtocolMessage('s1', 0, msg, 0);
    expect(out.with_skills).toBeUndefined();
  });
});