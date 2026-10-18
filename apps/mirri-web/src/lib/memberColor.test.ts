// apps/mirri-web/src/lib/memberColor.test.ts
import { describe, expect, it } from 'vitest';
import { MEMBER_PALETTE, assignMemberColors } from './memberColor';

describe('assignMemberColors', () => {
  it('should return the same color for the same member id across calls', () => {
    const a = assignMemberColors(['agent-1', 'agent-2', 'agent-3']);
    const b = assignMemberColors(['agent-1', 'agent-2', 'agent-3']);
    expect([...a.entries()]).toEqual([...b.entries()]);
  });

  it('should assign distinct colors within a batch while the palette allows', () => {
    const colors = assignMemberColors(['a', 'b', 'c', 'd']);
    expect(new Set(colors.values()).size).toBe(4);
  });

  it('should reuse palette entries without throwing when ids exceed the palette size', () => {
    const ids = Array.from({ length: MEMBER_PALETTE.length + 5 }, (_, i) => `id-${i}`);
    const colors = assignMemberColors(ids);
    expect(colors.size).toBe(ids.length);
    expect([...colors.values()].every((c) => MEMBER_PALETTE.includes(c))).toBe(true);
  });

  it('should only emit colors from the palette', () => {
    const colors = assignMemberColors(['x', 'y']);
    for (const c of colors.values()) expect(MEMBER_PALETTE).toContain(c);
  });

  it('should honor an explicit color choice for a member when preferred', () => {
    const colors = assignMemberColors(['m1'], { preferred: { m1: 'var(--color-success)' } });
    expect(colors.get('m1')).toBe('var(--color-success)');
  });
});
