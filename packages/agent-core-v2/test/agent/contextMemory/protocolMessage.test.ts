import { describe, expect, it } from 'vitest';
import { messageSchema } from '#/agent/contextMemory/protocolMessage';

describe('messageSchema with_skills field (ADR-006)', () => {
  const baseMessage = {
    id: 'm1',
    session_id: 's1',
    role: 'user',
    content: [{ type: 'text', text: '[review] the code' }],
    created_at: '2026-10-17T00:00:00.000Z',
  };

  it('should parse a message carrying with_skills entries', () => {
    const parsed = messageSchema.parse({
      ...baseMessage,
      with_skills: [{ name: 'review', target: '/skills/review/SKILL.md', trigger: 'user-inline' }],
    });
    expect(parsed.with_skills).toEqual([
      { name: 'review', target: '/skills/review/SKILL.md', trigger: 'user-inline' },
    ]);
  });

  it('should parse a message without with_skills (legacy compatibility)', () => {
    const parsed = messageSchema.parse(baseMessage);
    expect(parsed.with_skills).toBeUndefined();
  });

  it('should reject with_skills entries missing required members', () => {
    expect(() =>
      messageSchema.parse({
        ...baseMessage,
        with_skills: [{ name: 'review', target: '/skills/review/SKILL.md' }],
      }),
    ).toThrow();
  });
});
