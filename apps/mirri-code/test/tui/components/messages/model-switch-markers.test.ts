import { visibleWidth } from '@mirri-ai/pi-tui';
import { describe, expect, it } from 'vitest';

import {
  ModelSwitchMarkerComponent,
  parseModelSwitchPatch,
} from '#/tui/components/messages/model-switch-markers';

const ANSI_SGR = /\u001B\[[0-9;]*m/g;
function strip(lines: string[]): string {
  return lines.join('\n').replaceAll(ANSI_SGR, '');
}

function alias(overrides: Record<string, unknown> = {}) {
  return { provider: 'kimi', model: 'model-x', maxContextSize: 100, ...overrides };
}

describe('ModelSwitchMarkerComponent', () => {
  it('renders the divider line "Model switched: A → B" when a switch is displayed', () => {
    // Given a mid-turn model switch from alias model-a to alias model-b
    const marker = new ModelSwitchMarkerComponent('model-a', 'model-b');

    // When the divider renders at a normal terminal width
    const lines = marker.render(80);

    // Then it is the single-line bullet + label divider, in the web copy's
    // wording (spec 11 §2.5 A2; en locale "Model switched: {from} → {to}")
    expect(strip(lines)).toBe('\n● Model switched: model-a → model-b');
  });

  it('maps aliases onto display names from the catalog, falling back to the model id', () => {
    // Given aliases where one has a catalog displayName and the other has none
    const marker = new ModelSwitchMarkerComponent('fast', 'smart', {
      fast: alias({ model: 'model-fast', displayName: 'Fast Model' }),
      smart: alias({ model: 'model-smart' }),
    });

    // When the divider renders
    const text = strip(marker.render(80));

    // Then the catalog display name is used, and an alias without one falls
    // back to the model id, then the alias text (M4 is not an identifier —
    // the mapping is a presentation-end concern)
    expect(text).toContain('Model switched: Fast Model → model-smart');
  });

  it('falls back to the alias text when the catalog does not know the model', () => {
    // Given aliases absent from the catalog entirely
    const marker = new ModelSwitchMarkerComponent('model-a', 'model-b');

    // When the divider renders
    const text = strip(marker.render(80));

    // Then the alias text is shown verbatim
    expect(text).toContain('Model switched: model-a → model-b');
  });

  it('keeps the divider within the terminal width for long model names', () => {
    // Given a switch whose display text overflows the terminal
    const marker = new ModelSwitchMarkerComponent(
      'a-very-long-model-alias',
      'another-very-long-model-alias',
    );

    // When rendered at a narrow width
    const width = 40;
    const lines = marker.render(width);

    // Then every line fits and the label is truncated
    for (const line of lines) {
      expect(visibleWidth(line)).toBeLessThanOrEqual(width);
    }
    expect(strip(lines)).toContain('Model switched: a-very-long-model-ali…');
    expect(strip(lines)).not.toContain('→');
  });

  it('renders an empty line at zero width', () => {
    // Given a switch divider
    const marker = new ModelSwitchMarkerComponent('a', 'b');

    // When rendered with no usable width
    // Then it degrades to the swarm-marker blank-line spelling
    expect(marker.render(0)).toEqual(['']);
  });
});

describe('parseModelSwitchPatch', () => {
  it('accepts a well-formed modelSwitched patch member', () => {
    // Given the translated rust event's patch member
    // When parsed
    // Then the from/to pair is extracted
    expect(parseModelSwitchPatch({ from: 'a', to: 'b' })).toEqual({ from: 'a', to: 'b' });
  });

  it('rejects malformed patch members', () => {
    // Given malformed patch members
    // When parsed
    // Then none of them yield a switch (the divider never renders garbage)
    expect(parseModelSwitchPatch(undefined)).toBeUndefined();
    expect(parseModelSwitchPatch('model-a')).toBeUndefined();
    expect(parseModelSwitchPatch({ from: 'a' })).toBeUndefined();
    expect(parseModelSwitchPatch({ from: '', to: 'b' })).toBeUndefined();
    expect(parseModelSwitchPatch({ from: 'a', to: 3 })).toBeUndefined();
  });
});
