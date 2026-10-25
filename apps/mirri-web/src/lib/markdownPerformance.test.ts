import { describe, expect, it } from 'vitest';
import { markdownRenderPlanFor } from './markdownPerformance';

describe('streaming code render plan (event-delivery ADR-004)', () => {
  it('should use the pre renderer while streaming even when the text is heavy', () => {
    const heavyText = '```rust\n' + 'let x = 1;\n'.repeat(5000) + '```';
    expect(markdownRenderPlanFor(true, heavyText).codeRenderer).toBe('pre');
  });

  it('should use the heaviness plan once the turn has settled', () => {
    const light = markdownRenderPlanFor(false, 'hello');
    expect(light.codeRenderer).toBe('shiki');
    const heavyText = '```rust\n' + 'let x = 1;\n'.repeat(5000) + '```';
    expect(markdownRenderPlanFor(false, heavyText).codeRenderer).toBe('pre');
  });
});
