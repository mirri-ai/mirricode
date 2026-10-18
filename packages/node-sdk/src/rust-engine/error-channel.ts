/**
 * The NAPI error channel decoder — turns the native engine's structured
 * error suffix into a first-class `Error` carrying the JSON-RPC code and the
 * domain code.
 *
 * ## The channel (M-3)
 *
 * napi-rs 2.16's async promise machinery pins the JS `Error`'s `code` to the
 * fixed `Status` enum (`"GenericFailure"`) and offers no channel for extra
 * properties, so an engine error crossing the NAPI boundary would otherwise
 * degrade to a plain message string (`from_reason(e.to_string())`). The
 * Rust side therefore appends a compact JSON suffix to the error message
 * (see `engine_error_to_napi` in `rust/crates/mirri-node/src/napi_exports.rs`):
 *
 * ```text
 * Not implemented: … [mirri-rpc-error {"code":-32603,"domainCode":"not_implemented","retryable":false}]
 * ```
 *
 * The `retryable` field rides the same payload the wire surfaces carry
 * (the domain code's retryability verdict); it is absent when the engine
 * error attached no verdict — older binaries degrade gracefully.
 *
 * This module parses that suffix back off: [`decodeRustEngineError`] maps a
 * rejection from `engine.callMethod` onto a [`RustEngineRpcError`] whose
 * `code` is the numeric JSON-RPC code and whose `domainCode` is the engine's
 * domain reason (`not_implemented`, `session.not_found`, …) — mirroring what
 * the JSON-RPC transport's error objects expose over the wire, so SDK
 * consumers branch on the same fields regardless of the engine host.
 *
 * Messages that carry no suffix (a raw NAPI failure, a bridging error from a
 * stale binary) pass through unchanged, so a mismatched `.node` build never
 * crashes the client — the structured fields are simply absent.
 */

/**
 * The suffix pattern the Rust NAPI layer appends to engine error messages.
 * A single trailing `[mirri-rpc-error {…}]` segment; no hand-written engine
 * message ends with one, and `JSON.parse` validates the payload before it is
 * trusted.
 */
const RPC_ERROR_SUFFIX = / \[mirri-rpc-error (\{.*\})\]$/;

/** The structured payload the suffix carries. */
interface RustRpcErrorPayload {
  readonly code: number;
  readonly domainCode?: unknown;
  readonly retryable?: unknown;
  readonly retryAfterMs?: unknown;
}

/**
 * Parse the structured suffix off a native engine error message.
 *
 * Returns the decoded fields plus the human-readable message (the suffix
 * stripped), or `undefined` when the message carries no decodable suffix.
 */
export function parseRustEngineErrorSuffix(
  message: string,
): {
  message: string;
  code: number;
  domainCode: string | undefined;
  retryable: boolean | undefined;
  retryAfterMs: number | undefined;
} | undefined {
  const match = RPC_ERROR_SUFFIX.exec(message);
  if (match === null) return undefined;
  let payload: RustRpcErrorPayload;
  try {
    payload = JSON.parse(match[1]!) as RustRpcErrorPayload;
  } catch {
    // A lookalike suffix that is not valid JSON — not ours.
    return undefined;
  }
  if (typeof payload?.code !== 'number') return undefined;
  const domainCode =
    typeof payload.domainCode === 'string' ? payload.domainCode : undefined;
  const retryable =
    typeof payload.retryable === 'boolean' ? payload.retryable : undefined;
  // The quota-exhausted error's Retry-After hint (P11a F-P8-13): present
  // only when the engine attached one (a quota 429 without a provider hint
  // carries no guess).
  const retryAfterMs =
    typeof payload.retryAfterMs === 'number' ? payload.retryAfterMs : undefined;
  return {
    message: message.slice(0, match.index),
    code: payload.code,
    domainCode,
    retryable,
    retryAfterMs,
  };
}

/**
 * An engine error decoded off the NAPI error channel.
 *
 * `code` is the numeric JSON-RPC error code (`-32603` internal,
 * `-32602` invalid params, `-32001` not found, …); `domainCode` is the
 * engine's domain reason (`not_implemented`, `session.not_found`, …) when
 * the engine attached one; `retryable` is the domain code's retryability
 * verdict when the engine attached one (older binaries carry no verdict —
 * absent, never guessed). `cause` keeps the original native error.
 */
export class RustEngineRpcError extends Error {
  readonly code: number;
  readonly domainCode: string | undefined;
  readonly retryable: boolean | undefined;
  /** The provider's Retry-After hint in milliseconds, when the engine
   *  attached one (quota-exhausted 429s — the TS
   *  `APIProviderQuotaExhaustedError` parity). */
  readonly retryAfterMs: number | undefined;

  constructor(
    message: string,
    options: {
      code: number;
      domainCode?: string;
      retryable?: boolean;
      retryAfterMs?: number;
      cause?: unknown;
    },
  ) {
    super(message, { cause: options.cause });
    this.name = 'RustEngineRpcError';
    this.code = options.code;
    this.domainCode = options.domainCode;
    this.retryable = options.retryable;
    this.retryAfterMs = options.retryAfterMs;
  }
}

/**
 * Decode a rejection that crossed the NAPI boundary.
 *
 * A `Error` whose message ends in the structured suffix becomes a
 * [`RustEngineRpcError`] with the numeric `code` and `domainCode` exposed as
 * properties; anything else (non-errors, suffix-less messages) is returned
 * unchanged so older native binaries and unrelated failures degrade
 * gracefully.
 */
export function decodeRustEngineError(error: unknown): unknown {
  if (!(error instanceof Error)) return error;
  const decoded = parseRustEngineErrorSuffix(error.message);
  if (decoded === undefined) return error;
  return new RustEngineRpcError(decoded.message, {
    code: decoded.code,
    domainCode: decoded.domainCode,
    retryable: decoded.retryable,
    retryAfterMs: decoded.retryAfterMs,
    cause: error,
  });
}
