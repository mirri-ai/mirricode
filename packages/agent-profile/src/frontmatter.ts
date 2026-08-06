/**
 * Markdown frontmatter parsing and serialization — pure text processing.
 *
 * Splits a Markdown document into its YAML frontmatter block and body.
 * A document without a leading `---` fence parses as all body with
 * `data: null`; an unterminated fence is a `FrontmatterError`.
 *
 * `serializeFrontmatter` is the inverse: it renders a data object + body
 * back into the `---\n...\n---\nbody` text form so a round-trip
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
    throw new FrontmatterError(message, error);
  }
}

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
