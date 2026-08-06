/**
 * `_base` text helpers — Markdown frontmatter parsing and serialization.
 *
 * Splits a Markdown document into its YAML frontmatter block and body. Pure
 * text processing with no IO and no domain knowledge. A document without a
 * leading `---` fence parses as all body with `data: null`; an unterminated
 * fence is a `FrontmatterError`.
 *
 * `serializeFrontmatter` is the inverse: it renders a data object + body back
 * into the `---\n...\n---\nbody` text form so a round-trip
 * (parse → serialize → parse) preserves both halves.
 */

import { dump as dumpYaml, load as loadYaml } from 'js-yaml';

export class FrontmatterError extends Error {
  constructor(message: string, cause?: unknown) {
    super(message);
    this.name = 'FrontmatterError';
    if (cause !== undefined) {
      Object.defineProperty(this, 'cause', { value: cause, configurable: true });
    }
  }
}

export interface ParsedFrontmatter {
  readonly data: unknown;
  readonly body: string;
}

const FENCE = '---';

export function parseFrontmatter(text: string): ParsedFrontmatter {
  const lines = text.split(/\r?\n/);
  if (lines[0]?.trim() !== FENCE) {
    return { data: null, body: text };
  }

  const close = lines.findIndex((line, index) => index > 0 && line.trim() === FENCE);
  if (close === -1) {
    throw new FrontmatterError('Missing closing frontmatter fence');
  }

  const yamlText = lines.slice(1, close).join('\n').trim();
  const body = lines.slice(close + 1).join('\n');
  if (yamlText === '') {
    return { data: {}, body };
  }

  try {
    return { data: loadYaml(yamlText) ?? {}, body };
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    throw new FrontmatterError(withActionableYamlHint(message), error);
  }
}

/**
 * Map common YAML parse failures to hints that tell the author what to fix.
 *
 * A plain scalar whose value starts with `*` is read by the YAML parser as an
 * anchor alias. SKILL.md authors routinely write Markdown emphasis (`**bold**`)
 * in frontmatter values, which trips js-yaml into an "unidentified alias" error
 * that does not explain the actual cause; append the fix so the error is
 * actionable instead of jargon.
 */
function withActionableYamlHint(message: string): string {
  if (message.includes('unidentified alias')) {
    return `${message}\nA value starting with "*" is parsed as a YAML anchor alias. If you wrote Markdown emphasis like **bold** in a frontmatter field, wrap the value in double quotes (e.g. description: "**bold**") or remove the emphasis.`;
  }
  return message;
}

/**
 * Serialize a data object and body into a Markdown document with YAML
 * frontmatter. Inverse of {@link parseFrontmatter}: the output, when parsed
 * again, yields the same `data` and `body`.
 *
 * The YAML block is rendered with `js-yaml`'s `dump`, which produces
 * human-readable key-value pairs. An empty body produces a document that
 * ends at the closing fence (no trailing content).
 */
export function serializeFrontmatter(
  data: Record<string, unknown>,
  body: string,
): string {
  const yamlText = dumpYaml(data, { lineWidth: -1 }).trimEnd();
  if (body.length === 0) {
    return `${FENCE}\n${yamlText}\n${FENCE}\n`;
  }
  return `${FENCE}\n${yamlText}\n${FENCE}\n${body}`;
}
