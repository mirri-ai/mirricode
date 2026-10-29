// apps/mirri-web/test/file-ref-echo.test.ts
// E9 — user-bubble file-reference echo. A message sent with structured file
// references serializes each fileMention chip to plain `@path` wire text
// (folders carry a trailing `/`, see src/lib/fileMention.ts). The user bubble
// then has to RECOGNIZE that serialized form and re-render it as a chip
// (US9.10): the split plan gains a `fileRef` segment kind. This suite pins
// the pure split logic — recognition, folder-vs-file precedence, and the
// anti-false-positive rules (emails, skill links).

import { describe, expect, it } from 'vitest';
import { splitMessageInlineReferences } from '../src/lib/inlineReferences';
import type { InlineSegment } from '../src/lib/inlineReferences';
import { formatSkillReferenceLink } from '@mirri-ai/protocol';

describe('splitMessageInlineReferences — file-reference echo (E9)', () => {
  it('should split a serialized file reference into a fileRef segment when the text mentions a path', () => {
    expect(splitMessageInlineReferences('look at @src/a.ts please')).toEqual<InlineSegment[]>([
      { kind: 'plain', text: 'look at ' },
      { kind: 'fileRef', refKind: 'file', display: 'src/a.ts', target: 'src/a.ts', start: 8, end: 17 },
      { kind: 'plain', text: ' please' },
    ]);
  });

  it('should treat a trailing-slash mention followed by whitespace as a folder segment when the bubble splits it', () => {
    expect(splitMessageInlineReferences('check @src/utils/ now')).toEqual<InlineSegment[]>([
      { kind: 'plain', text: 'check ' },
      { kind: 'fileRef', refKind: 'folder', display: 'src/utils/', target: 'src/utils/', start: 6, end: 17 },
      { kind: 'plain', text: ' now' },
    ]);
  });

  it('should treat a trailing-slash mention at the end of the text as a folder segment when nothing follows it', () => {
    expect(splitMessageInlineReferences('check @src/utils/')).toEqual<InlineSegment[]>([
      { kind: 'plain', text: 'check ' },
      { kind: 'fileRef', refKind: 'folder', display: 'src/utils/', target: 'src/utils/', start: 6, end: 17 },
    ]);
  });

  it('should not treat an email address as a file reference when the @ is glued to a word', () => {
    expect(splitMessageInlineReferences('mail me at user@host.com ok')).toEqual<InlineSegment[]>([
      { kind: 'plain', text: 'mail me at user@host.com ok' },
    ]);
  });

  it('should not treat an inline skill link as a file reference when both forms appear', () => {
    const link = formatSkillReferenceLink('alpha', '/skills/alpha/SKILL.md');
    const segments = splitMessageInlineReferences(`use ${link} and @src/a.ts`);
    expect(segments).toEqual<InlineSegment[]>([
      { kind: 'plain', text: 'use ' },
      { kind: 'skill', display: 'alpha', target: '/skills/alpha/SKILL.md', start: 4, end: 4 + link.length },
      { kind: 'plain', text: ' and ' },
      { kind: 'fileRef', refKind: 'file', display: 'src/a.ts', target: 'src/a.ts', start: 9 + link.length, end: 18 + link.length },
    ]);
  });

  it('should degrade a quoted file reference to plain text when the mention carries spaces', () => {
    // Quoted form `@"a b"` never round-trips as a structured chip (the wire
    // text of a chip is always `@path` with a whitespace-free path), and the
    // bubble deliberately does not parse quotes — no false chips from
    // hand-typed history.
    expect(splitMessageInlineReferences('open @"a b" now')).toEqual<InlineSegment[]>([
      { kind: 'plain', text: 'open @"a b" now' },
    ]);
  });
});

describe('splitMessageInlineReferences — metadata-driven chips (M5)', () => {
  it('should render a chip with the truncated mark when the metadata marks the reference truncated', () => {
    // M4/US9.4: the send-time line probe found more lines than the engine's
    // budget — the chip must say so.
    expect(
      splitMessageInlineReferences('read @big.log now', undefined, [
        { path: 'big.log', kind: 'file', truncated: true },
      ]),
    ).toEqual<InlineSegment[]>([
      { kind: 'plain', text: 'read ' },
      { kind: 'fileRef', refKind: 'file', display: 'big.log', target: 'big.log', start: 5, end: 13, truncated: true },
      { kind: 'plain', text: ' now' },
    ]);
  });

  it('should keep a hand-typed @path as plain text when the metadata does not list it', () => {
    // US9.10: only what the user actually chipped renders as a chip — a
    // hand-typed `@path` that never went through the picker stays text.
    expect(
      splitMessageInlineReferences('look at @a.ts and @b.ts', undefined, [
        { path: 'a.ts', kind: 'file' },
      ]),
    ).toEqual<InlineSegment[]>([
      { kind: 'plain', text: 'look at ' },
      { kind: 'fileRef', refKind: 'file', display: 'a.ts', target: 'a.ts', start: 8, end: 13 },
      { kind: 'plain', text: ' and @b.ts' },
    ]);
  });

  it('should match a metadata folder by its trailing-slash wire spelling', () => {
    // The chip serializes folders as `@src/utils/` — the metadata carries the
    // bare path, so the slashed token must resolve against it (and NOT render
    // the bare-path file form).
    expect(
      splitMessageInlineReferences('check @src/utils/ now', undefined, [
        { path: 'src/utils', kind: 'folder' },
      ]),
    ).toEqual<InlineSegment[]>([
      { kind: 'plain', text: 'check ' },
      { kind: 'fileRef', refKind: 'folder', display: 'src/utils/', target: 'src/utils/', start: 6, end: 17 },
      { kind: 'plain', text: ' now' },
    ]);
  });

  it('should render every well-formed @path as a chip when no metadata exists', () => {
    // Reloaded history: no send-time metadata — the regex fallback keeps the
    // pre-M5 behavior (all well-formed tokens chip).
    expect(splitMessageInlineReferences('look at @a.ts and @b.ts', undefined, undefined)).toEqual<
      InlineSegment[]
    >([
      { kind: 'plain', text: 'look at ' },
      { kind: 'fileRef', refKind: 'file', display: 'a.ts', target: 'a.ts', start: 8, end: 13 },
      { kind: 'plain', text: ' and ' },
      { kind: 'fileRef', refKind: 'file', display: 'b.ts', target: 'b.ts', start: 18, end: 23 },
    ]);
  });
});
