import type { MirriErrorCode } from './codes';

export interface MirriErrorOptions {
  /** JSON-serializable structured details. */
  readonly details?: Record<string, unknown>;
  /** Original error or value. Local-only; never serialized to the wire. */
  readonly cause?: unknown;
}

/**
 * The single Kimi error class.
 *
 * Discrimination is always by `code`. Cross-process consumers receive
 * `MirriErrorPayload` and must branch on `code` rather than class identity.
 */
export class MirriError extends Error {
  readonly code: MirriErrorCode;
  readonly details?: Record<string, unknown>;
  override readonly cause?: unknown;

  constructor(code: MirriErrorCode, message: string, options: MirriErrorOptions = {}) {
    super(message);
    this.name = 'MirriError';
    this.code = code;
    this.details = options.details;
    this.cause = options.cause;
  }
}
