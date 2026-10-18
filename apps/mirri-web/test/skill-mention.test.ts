// Pure-logic tests for the Composer's TipTap skill-mention layer: doc JSON
// construction from raw draft strings (full links + legacy compact tokens),
// doc JSON → wire-text serialization (the full `[$name](target)` link form —
// the compact token is retired from the payload path), node-position math for
// the history/grown wiring, and the raw markdown-link node parse rule.
import { describe, expect, it } from 'vitest';
import { formatSkillReferenceLink } from '@mirri-ai/protocol';
import {
  buildSkillMentionDocJson,
  docHasContent,
  isCaretAtDocStart,
  serializeDocJsonToText,
  splitDraftIntoSegments,
  textOffsetToPos,
} from '../src/lib/skillMention';
import { SkillMention } from '../src/lib/skillMentionNode';

const SKILL_A = '/Users/example/skills/alpha/SKILL.md';
const SKILL_B = '/Users/example/skills/beta/SKILL.md';

// ---------------------------------------------------------------------------
// parse — raw string → doc JSON
// ---------------------------------------------------------------------------

describe('buildSkillMentionDocJson — draft parsing (ADR-004, compact-token compat)', () => {
  it('should produce a single empty paragraph for the empty string', () => {
    expect(buildSkillMentionDocJson('')).toEqual({
      type: 'doc',
      content: [{ type: 'paragraph' }],
    });
  });

  it('should keep plain text as text nodes without mention children', () => {
    const doc = buildSkillMentionDocJson('hello world');
    expect(doc.content).toEqual([{ type: 'paragraph', content: [{ type: 'text', text: 'hello world' }] }]);
  });

  it('should turn a full reference link into a skillMention node with the target', () => {
    const link = formatSkillReferenceLink('alpha', SKILL_A);
    const doc = buildSkillMentionDocJson(`use ${link} now`);
    expect(doc.content?.[0]?.content).toEqual([
      { type: 'text', text: 'use ' },
      { type: 'skillMention', attrs: { display: 'alpha', target: SKILL_A, kind: 'skill' } },
      { type: 'text', text: ' now' },
    ]);
  });

  it('should turn a command link into a skillMention node of kind command', () => {
    const doc = buildSkillMentionDocJson('run [$model](command:/model) ok');
    expect(doc.content?.[0]?.content).toEqual([
      { type: 'text', text: 'run ' },
      { type: 'skillMention', attrs: { display: 'model', target: 'command:/model', kind: 'command' } },
      { type: 'text', text: ' ok' },
    ]);
  });

  it('should turn a legacy compact token into a targetless skillMention node', () => {
    // Old drafts (pre-migration) stored `[$name]`; they load as nodes and the
    // pre-submit gate flags them as missing before send.
    const doc = buildSkillMentionDocJson('go [$alpha] now');
    expect(doc.content?.[0]?.content).toEqual([
      { type: 'text', text: 'go ' },
      { type: 'skillMention', attrs: { display: 'alpha', target: '', kind: 'skill' } },
      { type: 'text', text: ' now' },
    ]);
  });

  it('should split into multiple paragraphs on newlines', () => {
    const doc = buildSkillMentionDocJson('line one\nline two');
    expect(doc.content).toEqual([
      { type: 'paragraph', content: [{ type: 'text', text: 'line one' }] },
      { type: 'paragraph', content: [{ type: 'text', text: 'line two' }] },
    ]);
  });

  it('should keep blank lines as empty paragraphs', () => {
    const doc = buildSkillMentionDocJson('a\n\nb');
    expect(doc.content?.[1]).toEqual({ type: 'paragraph' });
  });

  it('should accept a leading mention (empty composer insert form)', () => {
    const doc = buildSkillMentionDocJson(`[$alpha](${SKILL_A}) then`);
    expect(doc.content?.[0]?.content?.[0]).toEqual({
      type: 'skillMention',
      attrs: { display: 'alpha', target: SKILL_A, kind: 'skill' },
    });
  });
});

describe('splitDraftIntoSegments — segment split', () => {
  it('should return one plain segment for reference-free text', () => {
    expect(splitDraftIntoSegments('plain')).toEqual([{ kind: 'plain', text: 'plain' }]);
  });

  it('should not treat a plain markdown link as a mention', () => {
    expect(splitDraftIntoSegments('see [alpha](/skills/alpha/SKILL.md) here')).toEqual([
      { kind: 'plain', text: 'see [alpha](/skills/alpha/SKILL.md) here' },
    ]);
  });

  it('should prefer the full link when a compact token sits inside one', () => {
    const link = formatSkillReferenceLink('alpha', SKILL_A);
    const segs = splitDraftIntoSegments(`go ${link} ok`);
    expect(segs).toHaveLength(3);
    expect(segs[1]).toMatchObject({ kind: 'skill', display: 'alpha', target: SKILL_A });
  });
});

// ---------------------------------------------------------------------------
// serialize — doc JSON → wire text
// ---------------------------------------------------------------------------

describe('serializeDocJsonToText — wire serialization (full-link payload)', () => {
  it('should serialize the empty doc to the empty string', () => {
    expect(serializeDocJsonToText(buildSkillMentionDocJson(''))).toBe('');
  });

  it('should serialize plain text verbatim', () => {
    expect(serializeDocJsonToText(buildSkillMentionDocJson('hello world'))).toBe('hello world');
  });

  it('should serialize a skill mention back to the full reference link', () => {
    const link = formatSkillReferenceLink('alpha', SKILL_A);
    expect(serializeDocJsonToText(buildSkillMentionDocJson(`use ${link} now`))).toBe(`use ${link} now`);
  });

  it('should serialize a command mention to the command link form', () => {
    expect(serializeDocJsonToText(buildSkillMentionDocJson('run [$model](command:/model) ok'))).toBe(
      'run [$model](command:/model) ok',
    );
  });

  it('should join paragraphs with newlines (round-trip a multi-line draft)', () => {
    const text = 'line one\nline two';
    expect(serializeDocJsonToText(buildSkillMentionDocJson(text))).toBe(text);
  });

  it('should serialize a mention-only doc to just the link', () => {
    const link = formatSkillReferenceLink('alpha', SKILL_A);
    expect(serializeDocJsonToText(buildSkillMentionDocJson(link))).toBe(link);
  });

  it('should serialize a targetless legacy node to its bare display name', () => {
    // Legacy compact token loaded into the editor without a resolved target —
    // the pre-submit gate flags it; the serializer degrades to visible text
    // instead of emitting a broken link.
    expect(serializeDocJsonToText(buildSkillMentionDocJson('go [$alpha] now'))).toBe('go alpha now');
  });

  it('should serialize adjacent mentions with their surrounding text intact', () => {
    const a = formatSkillReferenceLink('alpha', SKILL_A);
    const b = formatSkillReferenceLink('beta', SKILL_B);
    const text = `${a} and ${b}`;
    expect(serializeDocJsonToText(buildSkillMentionDocJson(text))).toBe(text);
  });

  it('should serialize a hardBreak node as a newline', () => {
    const doc = {
      type: 'doc',
      content: [
        { type: 'paragraph', content: [{ type: 'text', text: 'a' }, { type: 'hardBreak' }, { type: 'text', text: 'b' }] },
      ],
    };
    expect(serializeDocJsonToText(doc)).toBe('a\nb');
  });
});

// ---------------------------------------------------------------------------
// node-position math
// ---------------------------------------------------------------------------

describe('docHasContent / isCaretAtDocStart — composer wiring math', () => {  it('should report no content for the pristine doc', () => {
    expect(docHasContent(buildSkillMentionDocJson(''))).toBe(false);
  });

  it('should report content for plain text', () => {
    expect(docHasContent(buildSkillMentionDocJson('hi'))).toBe(true);
  });

  it('should report content for a bare mention', () => {
    expect(docHasContent(buildSkillMentionDocJson(`[$alpha](${SKILL_A})`))).toBe(true);
  });

  it('should report content for a multi-paragraph doc even when empty', () => {
    // A bare newline counts as content — matches the textarea scrollHeight
    // growth heuristic (the box grew on an empty line).
    expect(docHasContent(buildSkillMentionDocJson('\n'))).toBe(true);
  });

  it('should treat selection position 0 as the doc start', () => {
    expect(isCaretAtDocStart(0)).toBe(true);
    expect(isCaretAtDocStart(1)).toBe(false);
  });
});

describe('textOffsetToPos — textBetween projection offset → doc position', () => {
  it('should map 1:1 inside a single paragraph (text and atomic nodes)', () => {
    // "use 〇 now" (9 chars) — the 〇 placeholder is one char for the atomic
    // chip node; the first paragraph's content starts at position 1.
    expect(textOffsetToPos('use 〇 now', 0)).toBe(1);
    expect(textOffsetToPos('use 〇 now', 5)).toBe(6);
    expect(textOffsetToPos('use 〇 now', 9)).toBe(10);
  });

  it('should add +2 per paragraph boundary (one \\n char in the projection)', () => {
    // "line1\n@file": '@file' starts at string offset 6 but at doc position 8
    // (position 1 + line1's 5 chars + close+open tokens = 2).
    const text = 'line1\n@file';
    expect(textOffsetToPos(text, 5)).toBe(6); // inside line 1 — 1:1 from pos 1
    expect(textOffsetToPos(text, 6)).toBe(8); // first char of paragraph 2
    expect(textOffsetToPos(text, 11)).toBe(13); // end of paragraph 2
  });

  it('should map offsets across multiple boundaries', () => {
    // doc: p1"a" p2"b" p3"c" — content positions 1, 4, 7 (each boundary is
    // close+open = +2 after the preceding char).
    const text = 'a\nb\nc';
    expect(textOffsetToPos(text, 1)).toBe(2); // end of 'a'
    expect(textOffsetToPos(text, 2)).toBe(4); // 'b' in paragraph 2
    expect(textOffsetToPos(text, 4)).toBe(7); // 'c' in paragraph 3
    expect(textOffsetToPos(text, 5)).toBe(8); // doc content end
  });

  it('should walk an offset at the very end to one past the last char', () => {
    // "abc" — the doc's content ends at position 4 (1 + 3 chars).
    expect(textOffsetToPos('abc', 3)).toBe(4);
  });
});

// ---------------------------------------------------------------------------
// the TipTap node definition — schema-level contract, no DOM
// ---------------------------------------------------------------------------

describe('SkillMention node — schema contract', () => {
  it('should be an inline atom in the inline group', () => {
    expect(SkillMention.config.inline).toBe(true);
    expect(SkillMention.config.atom).toBe(true);
    expect(SkillMention.config.group).toBe('inline');
  });

  it('should register the insertSkillMention command and parse/render rules', () => {
    expect(typeof SkillMention.config.addCommands?.call(SkillMention).insertSkillMention).toBe('function');
    const parseRules = SkillMention.config.parseHTML?.call(SkillMention) ?? [];
    expect(parseRules.some((r) => r.tag === 'span[data-skill-mention]')).toBe(true);
  });

  it('should render the chip svg in the SVG namespace so browsers display it', () => {
    // renderHTML builds a ProseMirror DOMOutputSpec; prosemirror-model's
    // renderSpec only calls createElementNS when the tag carries an explicit
    // namespace prefix ("http://www.w3.org/2000/svg svg"). Without it the
    // svg lands in the HTML namespace, the browser treats it as an unknown
    // element (0×0, no presentation attributes) and the chip icon vanishes.
    const render = SkillMention.config.renderHTML?.call(
      SkillMention,
      mockRenderProps({ display: 'alpha', target: SKILL_A, kind: 'skill' }),
    ) as unknown[];
    const svgSpec = render.find((c) => Array.isArray(c) && c[0].includes('svg')) as unknown[];
    expect((svgSpec[0] as string).startsWith('http://www.w3.org/2000/svg')).toBe(true);
    // The path child must NOT carry the prefix — renderSpec passes the
    // parent's namespace down to children, so a prefixed child tag would
    // double-qualify and break.
    const pathSpec = (svgSpec as unknown[]).find((c) => Array.isArray(c) && c[0] === 'path') as unknown[];
    expect(pathSpec).toBeDefined();
  });

  it('should render the command chip svg in the SVG namespace too', () => {
    const render = SkillMention.config.renderHTML?.call(
      SkillMention,
      mockRenderProps({ display: 'model', target: 'command:/model', kind: 'command' }),
    ) as unknown[];
    const svgSpec = render.find((c) => Array.isArray(c) && c[0].includes('svg')) as unknown[];
    expect((svgSpec[0] as string).startsWith('http://www.w3.org/2000/svg')).toBe(true);
  });
});

/** Minimal RenderHTMLProps stand-in: renderHTML only reads node.attrs. */
function mockRenderProps(attrs: Record<string, string>) {
  return { node: { attrs }, HTMLAttributes: {} } as never;
}

