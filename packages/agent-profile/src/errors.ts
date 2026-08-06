/**
 * Agent profile file parse error.
 *
 * Thrown by `parseAgentFileText` when the file content is invalid —
 * malformed frontmatter, missing required fields, invalid name format, etc.
 * Callers (e.g. `discoverAgentFiles`) catch these and collect them into the
 * `skipped` list so one bad file does not zero the whole discovery pass.
 */
export class AgentFileParseError extends Error {
  readonly reason?: unknown;

  constructor(message: string, cause?: unknown) {
    super(message);
    this.name = 'AgentFileParseError';
    if (cause !== undefined) {
      Object.defineProperty(this, 'cause', { value: cause, configurable: true });
    }
  }
}
