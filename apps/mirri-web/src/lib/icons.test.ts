// apps/mirri-web/src/lib/icons.test.ts
import { describe, expect, it } from 'vitest';
import { ICONS, SIZE_PX, getIcon, iconSvg } from './icons';
import { SKILL_ICON_PATH } from './skillMentionNode';

describe('ICONS registry', () => {
  it('is non-empty', () => {
    expect(Object.keys(ICONS).length).toBeGreaterThan(0);
  });

  it('every entry has a component and a non-empty raw svg', () => {
    for (const [name, entry] of Object.entries(ICONS)) {
      // unplugin-icons component can be a function or a defineComponent object
      const ct = typeof entry.component;
      expect(['function', 'object'], `${name} component type`).toContain(ct);
      expect(typeof entry.svg, `${name} svg type`).toBe('string');
      expect(entry.svg.trim(), `${name} svg`).not.toBe('');
      expect(entry.svg.toLowerCase(), `${name} svg contains <svg`).toContain('<svg');
    }
  });

  it('every entry svg is on a 24x24 grid with a viewBox', () => {
    for (const [name, entry] of Object.entries(ICONS)) {
      expect(entry.svg, `${name} viewBox`).toContain('viewBox="0 0 24 24"');
    }
  });
});

describe('getIcon', () => {
  it('returns the entry for a known name', () => {
    expect(getIcon('plus')).toBe(ICONS.plus);
  });

  it('returns undefined for an unknown name (runtime fallback)', () => {
    // @ts-expect-error - intentional runtime misuse path
    expect(getIcon('definitely-not-an-icon')).toBeUndefined();
  });
});

describe('iconSvg', () => {
  it('renders a Remix icon with kw-icon class and default md size', () => {
    const svg = iconSvg('plus');
    expect(svg.startsWith('<svg ')).toBe(true);
    expect(svg).toContain('class="kw-icon"');
    expect(svg).toContain('width="16" height="16"');
  });

  it('maps size tokens to pixel width/height', () => {
    expect(iconSvg('plus', 'sm')).toContain(`width="${SIZE_PX.sm}" height="${SIZE_PX.sm}"`);
    expect(iconSvg('plus', 'md')).toContain(`width="${SIZE_PX.md}" height="${SIZE_PX.md}"`);
    expect(iconSvg('plus', 'lg')).toContain(`width="${SIZE_PX.lg}" height="${SIZE_PX.lg}"`);
  });

  it('does not duplicate width/height attributes from the raw icon', () => {
    const svg = iconSvg('plus');
    const widthCount = (svg.match(/\bwidth="/g) ?? []).length;
    const heightCount = (svg.match(/\bheight="/g) ?? []).length;
    expect(widthCount).toBe(1);
    expect(heightCount).toBe(1);
  });

  it('returns empty string for an unknown name', () => {
    // @ts-expect-error - intentional runtime misuse path
    expect(iconSvg('definitely-not-an-icon')).toBe('');
  });
});

describe('skill glyph (ADR-005)', () => {
  it('should render the dedicated skill glyph when iconSvg is called with skill', () => {
    const svg = iconSvg('skill');
    // Wand: long diagonal stroke from bottom-left; four-point star: the
    // largest closed path; plus three small sparkle cross paths.
    expect(svg).toContain('M3 21 10.5 13.5'); // wand shaft
    expect(svg).toContain('M13 2.5 14.3 5.7'); // four-point star
    expect(svg).toContain('stroke="currentColor"'); // follows text color
  });

  it('should keep bolt registered as a generic glyph when skill is added', () => {
    // ADR-005: bolt stays in the registry; only skill-surface refs change.
    expect(Object.keys(ICONS)).toContain('bolt');
    expect(Object.keys(ICONS)).toContain('skill');
  });

  it('should keep the inline mention-node path in sync with the registered skill asset', () => {
    // Normalize whitespace so formatting differences (single-line compound
    // path vs per-subpath `d` attributes) don't count as divergence.
    const normalize = (d: string) => d.replaceAll(/\s+/g, ' ').trim();

    // Asset: every `d="…"` value, in document order (one subpath per path).
    const assetSubpaths = Array.from(
      ICONS.skill.svg.matchAll(/d="([^"]+)"/g),
      (m) => normalize(m[1] ?? ''),
    ).filter((d) => d.length > 0);
    expect(assetSubpaths.length).toBeGreaterThan(0);

    // Inline compound path: split on each subpath's start (its `M`).
    const inlineSubpaths = SKILL_ICON_PATH.split(/(?=M)/)
      .map(normalize)
      .filter((d) => d.length > 0);

    // Same length, same order — the mention node must render exactly the
    // registered geometry so the two glyphs can never drift apart.
    expect(inlineSubpaths).toEqual(assetSubpaths);
  });
});
