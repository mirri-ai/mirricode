import {
  APIConnectionError,
  APIEmptyResponseError,
  APIStatusError,
  APITimeoutError,
  ChatProviderError,
} from '@mirri-ai/kosong';

import { MirriError } from './classes';
import { ErrorCodes, MIRRI_ERROR_INFO, type MirriErrorCode } from './codes';

/**
 * Wire-safe payload of a Kimi error.
 *
 * The structure passed across process / language boundaries (RPC, events,
 * telemetry, SDK wrappers). Class identity does not survive the boundary;
 * downstream code must branch on `code` rather than `instanceof`.
 *
 * `details` is JSON-serialized. `cause` is intentionally absent -- it is
 * local-only diagnostic state and must not cross the boundary.
 */
export interface MirriErrorPayload {
  readonly code: MirriErrorCode;
  readonly message: string;
  readonly name?: string;
  readonly details?: Record<string, unknown>;
  readonly retryable: boolean;
}

/** Type guard for MirriError. */
export function isMirriError(error: unknown): error is MirriError {
  return error instanceof MirriError;
}

/**
 * Build a MirriErrorPayload directly from a code + message (no Error instance
 * needed). Use this for synthetic error events that are signaled, not thrown
 * -- e.g. "turn busy" or "compaction failed". `retryable` is filled from
 * MIRRI_ERROR_INFO so callers cannot drift out of sync with the registry.
 */
export function makeErrorPayload(
  code: MirriErrorCode,
  message: string,
  options?: { readonly details?: Record<string, unknown>; readonly name?: string },
): MirriErrorPayload {
  return {
    code,
    message,
    name: options?.name,
    details: options?.details,
    retryable: MIRRI_ERROR_INFO[code].retryable,
  };
}

/**
 * Normalize any value into a MirriErrorPayload.
 *
 * Recognized errors:
 * - `MirriError`: passthrough.
 * - `APIStatusError`: 429 -> rate_limit, 401 -> auth_error, otherwise -> api_error.
 * - `APIConnectionError` / `APITimeoutError`: connection_error.
 * - `ChatProviderError`: api_error.
 *
 * Anything else collapses to `internal`. We never echo `cause` or stack on
 * the wire.
 */
export function toMirriErrorPayload(error: unknown): MirriErrorPayload {
  if (isMirriError(error)) {
    return {
      code: error.code,
      message: error.message,
      name: error.name,
      details: error.details,
      retryable: MIRRI_ERROR_INFO[error.code].retryable,
    };
  }

  if (error instanceof APIStatusError) {
    const code: MirriErrorCode =
      error.statusCode === 429
        ? ErrorCodes.PROVIDER_RATE_LIMIT
        : error.statusCode === 401
          ? ErrorCodes.PROVIDER_AUTH_ERROR
          : ErrorCodes.PROVIDER_API_ERROR;
    return {
      code,
      message: sanitizeStatusErrorMessage(error.message),
      name: error.name,
      details: {
        statusCode: error.statusCode,
        requestId: error.requestId,
      },
      retryable: MIRRI_ERROR_INFO[code].retryable,
    };
  }

  if (error instanceof APIConnectionError || error instanceof APITimeoutError) {
    return {
      code: ErrorCodes.PROVIDER_CONNECTION_ERROR,
      message: error.message,
      name: error.name,
      retryable: MIRRI_ERROR_INFO[ErrorCodes.PROVIDER_CONNECTION_ERROR].retryable,
    };
  }

  if (error instanceof APIEmptyResponseError) {
    return {
      code: ErrorCodes.PROVIDER_API_ERROR,
      message: error.message,
      name: error.name,
      details: {
        finishReason: error.finishReason,
        rawFinishReason: error.rawFinishReason,
      },
      retryable: MIRRI_ERROR_INFO[ErrorCodes.PROVIDER_API_ERROR].retryable,
    };
  }

  if (error instanceof ChatProviderError) {
    return {
      code: ErrorCodes.PROVIDER_API_ERROR,
      message: error.message,
      name: error.name,
      retryable: MIRRI_ERROR_INFO[ErrorCodes.PROVIDER_API_ERROR].retryable,
    };
  }

  if (error instanceof Error) {
    return {
      code: ErrorCodes.INTERNAL,
      message: error.message,
      name: error.name,
      retryable: MIRRI_ERROR_INFO[ErrorCodes.INTERNAL].retryable,
    };
  }

  return {
    code: ErrorCodes.INTERNAL,
    message: String(error),
    retryable: MIRRI_ERROR_INFO[ErrorCodes.INTERNAL].retryable,
  };
}

/**
 * Provider status errors occasionally carry an HTML body instead of a
 * structured message (for example, nginx returning
 * "413 <html><head><title>413 Request Entity Too Large</title>...</html>").
 * Extract the `<title>` when present so the wire message is human readable,
 * and strip carriage returns so the text renders cleanly in terminals — a
 * trailing `\r` combined with line-end padding would otherwise overwrite
 * the whole line. The original HTML remains available in logs and `details`.
 */
function sanitizeStatusErrorMessage(message: string): string {
  const titleMatch = /<title[^>]*>([\s\S]*?)<\/title>/i.exec(message);
  const extracted = titleMatch?.[1]?.trim();
  const normalized = extracted !== undefined && extracted.length > 0 ? extracted : message;
  return normalized.replaceAll('\r', '');
}

/**
 * Rehydrate a MirriErrorPayload into a MirriError. Used by SDK boundary code
 * receiving errors over RPC to re-surface them with a real class so
 * in-process consumers can still use `instanceof`.
 */
export function fromMirriErrorPayload(payload: MirriErrorPayload): MirriError {
  return new MirriError(payload.code, payload.message, {
    details: payload.details,
  });
}
