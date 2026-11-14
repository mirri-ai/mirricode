import { describe, expect, it } from 'vitest';
import { thinkingCopyKey } from '../src/lib/modelThinking';
// Raw SFC source, so the thinking control's branch precedence can be asserted
// without mounting the component (the test suite has no component runner).
import ComposerSource from '../src/components/chat/Composer.vue?raw';

describe('thinking control copy', () => {
  it('should point at the always-thinking explanation when the model cannot be turned off', () => {
    expect(thinkingCopyKey({ capabilities: ['thinking', 'always_thinking'] })).toBe(
      'composer.thinkingAlwaysOn',
    );
  });

  it('should stay silent when the model can be turned off', () => {
    expect(thinkingCopyKey({ capabilities: ['thinking'], supportEfforts: ['low'] })).toBeNull();
  });

  it('should stay silent for a model that does not think at all', () => {
    expect(thinkingCopyKey({ capabilities: [] })).toBeNull();
  });

  it('should show effort segments rather than a plain note when an always-on model declares efforts', () => {
    expect(
      thinkingCopyKey({
        capabilities: ['thinking', 'always_thinking'],
        supportEfforts: ['low', 'high', 'max'],
      }),
    ).toBeNull();
  });

  it('should render the always-on note as a tooltip on the effort segments, not a replacement for them', () => {
    // The always-on explanation must not sit in a v-else-if branch that
    // preempts the effort segments (the GLM 5.3 regression).
    const noteBranchIndex = ComposerSource.indexOf('v-else-if="thinkingNoteKey !== null"');
    const segmentsBranchIndex = ComposerSource.indexOf('class="effort-segments"');
    expect(noteBranchIndex).toBeGreaterThan(-1);
    expect(segmentsBranchIndex).toBeGreaterThan(-1);
    expect(ComposerSource).toContain('v-bind:title="thinkingSegmentsTitle"');
  });
});

