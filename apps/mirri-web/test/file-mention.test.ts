// Pure-logic tests for the Composer's TipTap file-mention layer (E9): the
// `fileMention` chip node's renderHTML contract (chip span + file/folder icon
// in the SVG namespace), doc JSON → wire-text serialization (`@path`, folder
// trailing slash), and doc-order chip collection with dedupe by path.
import { describe, expect, it } from 'vitest';
import { FileMention } from '../src/lib/fileMentionNode';
import { collectFileMentionAttrs, serializeDocJsonWithFileMentions } from '../src/lib/fileMention';

describe('fileMention node', () => {
  it('should render a chip with file kind and path attrs', () => {
    const props = mockRenderProps({
      display: 'Composer.vue',
      path: 'src/components/Composer.vue',
      kind: 'file',
      root: 'webapp',
    });
    const render = FileMention.config.renderHTML!.call(FileMention as never, props as never) as unknown[];

    // Outer chip span with the fileMention marker and both chip classes.
    expect(render[0]).toBe('span');
    const attrs = render[1] as Record<string, string>;
    expect(attrs['data-file-mention']).toBe('');
    expect(attrs['data-kind']).toBe('file');
    expect(attrs['data-path']).toBe('src/components/Composer.vue');
    expect(attrs['class']).toContain('ref-chip');
    expect(attrs['class']).toContain('file-ref-chip');

    // The icon svg MUST carry the SVG namespace prefix (a bare "svg" tag is
    // created via createElement, lands in the HTML namespace and renders as a
    // 0×0 unknown element — the chip icon vanishes); the path child stays bare.
    const svgSpec = render.find((c) => Array.isArray(c) && c[0].includes('svg')) as unknown[];
    expect((svgSpec[0] as string).startsWith('http://www.w3.org/2000/svg')).toBe(true);
    expect((svgSpec as unknown[]).some((c) => Array.isArray(c) && c[0] === 'path')).toBe(true);

    // Display text child.
    const nameSpec = render.find((c) => Array.isArray(c) && (c[1] as { class?: string })?.class === 'ref-chip-name');
    expect(nameSpec).toBeDefined();
    expect((nameSpec as unknown[])[2]).toBe('Composer.vue');
  });

  it('should serialize file mention as @path text with trailing slash for folders', () => {
    const doc = { type: 'doc', content: [{ type: 'paragraph', content: [
      { type: 'text', text: '看看 ' },
      { type: 'fileMention', attrs: { display: 'utils', path: 'src/utils', kind: 'folder' } },
    ] }] };
    expect(serializeDocJsonWithFileMentions(doc)).toBe('看看 @src/utils/');
  });

  it('should collect file mention attrs in document order with dedupe by path', () => {
    const doc = { type: 'doc', content: [{ type: 'paragraph', content: [
      { type: 'fileMention', attrs: { display: 'a', path: 'a.ts', kind: 'file' } },
      { type: 'fileMention', attrs: { display: 'a', path: 'a.ts', kind: 'file' } },
    ] }] };
    expect(collectFileMentionAttrs(doc)).toHaveLength(1);
  });
});

/** Minimal RenderHTMLProps stand-in: renderHTML only reads node.attrs. */
function mockRenderProps(attrs: Record<string, string>) {
  return { node: { attrs }, HTMLAttributes: {} } as never;
}
