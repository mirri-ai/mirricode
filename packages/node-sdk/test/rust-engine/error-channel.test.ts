import { describe, expect, it } from 'vitest';

import {
  decodeRustEngineError,
  parseRustEngineErrorSuffix,
  RustEngineRpcError,
} from '#/rust-engine/error-channel';

describe('rust engine NAPI error channel', () => {
  describe('parseRustEngineErrorSuffix', () => {
    it('should return the code and domain code when the message ends in the structured suffix', () => {
      // Given an engine error message carrying the M1.2-b wire shape
      const message =
        'Not implemented: task.detach is not supported ' +
        '[mirri-rpc-error {"code":-32603,"domainCode":"not_implemented"}]';

      // When the suffix is parsed
      const decoded = parseRustEngineErrorSuffix(message);

      // Then the human-readable message is the suffix-stripped prefix
      expect(decoded).toEqual({
        message: 'Not implemented: task.detach is not supported',
        code: -32603,
        domainCode: 'not_implemented',
      });
    });

    it('should return an undefined domain code when the suffix carries no domain code', () => {
      // Given an invalid-params error (no domain code in the engine data)
      const message =
        'name is required for mcp.reconnect [mirri-rpc-error {"code":-32602}]';

      // When the suffix is parsed
      const decoded = parseRustEngineErrorSuffix(message);

      // Then the code is decoded, the domain code is undefined, and the
      // retryability verdict is absent (never guessed)
      expect(decoded).toEqual({
        message: 'name is required for mcp.reconnect',
        code: -32602,
        domainCode: undefined,
        retryable: undefined,
      });
    });

    it('should return the retryable verdict when the suffix carries one', () => {
      // Given an engine error whose domain code the metadata table marks
      // retryable (context overflow — the client may compact and resend)
      const message =
        'context overflow [mirri-rpc-error {"code":-32603,"domainCode":"context.overflow","retryable":true}]';

      // When the suffix is parsed
      const decoded = parseRustEngineErrorSuffix(message);

      // Then the verdict rides alongside the code and domain code
      expect(decoded).toEqual({
        message: 'context overflow',
        code: -32603,
        domainCode: 'context.overflow',
        retryable: true,
      });
    });

    it('should return the retryAfterMs hint when the suffix carries one', () => {
      // Given a quota-exhausted engine error whose suffix carries the
      // provider's Retry-After hint (P11a F-P8-13 — the rust NAPI layer
      // re-surfaces the engine's error-data field)
      const message =
        'Provider quota exhausted: HTTP 429: top up ' +
        '[mirri-rpc-error {"code":-32603,"domainCode":"provider.quota_exhausted","retryable":false,"retryAfterMs":3000}]';

      // When the suffix is parsed
      const decoded = parseRustEngineErrorSuffix(message);

      // Then the hint rides alongside the code and domain code, so the UI
      // can surface "retry after 3s" without re-parsing the raw message
      expect(decoded).toEqual({
        message: 'Provider quota exhausted: HTTP 429: top up',
        code: -32603,
        domainCode: 'provider.quota_exhausted',
        retryable: false,
        retryAfterMs: 3000,
      });
    });

    it('should return undefined when the message carries no suffix', () => {
      // Given a plain message (a raw NAPI failure from a stale binary)
      // When the suffix is parsed
      // Then nothing decodes
      expect(parseRustEngineErrorSuffix('plain failure')).toBeUndefined();
      expect(parseRustEngineErrorSuffix('')).toBeUndefined();
    });

    it('should return undefined when the suffix payload is not valid JSON', () => {
      // Given a lookalike suffix that is not ours
      const message = 'weird [mirri-rpc-error {not json}]';

      // When the suffix is parsed
      // Then nothing decodes — the message stays opaque, never misparsed
      expect(parseRustEngineErrorSuffix(message)).toBeUndefined();
    });

    it('should return undefined when the payload parses but carries no numeric code', () => {
      // Given a suffix whose JSON parses but lacks the code field
      const message = 'weird [mirri-rpc-error {"domainCode":"not_implemented"}]';

      // When the suffix is parsed
      // Then nothing decodes
      expect(parseRustEngineErrorSuffix(message)).toBeUndefined();
    });
  });

  describe('decodeRustEngineError', () => {
    it('should rethrow a RustEngineRpcError with code and domainCode when the native error carries the suffix', () => {
      // Given a native rejection carrying the structured suffix
      const native = new Error(
        'Not implemented: mcp.enable is retired in the RPC contract; ' +
          'the embedded engine returns an explicit not_implemented error ' +
          '[mirri-rpc-error {"code":-32603,"domainCode":"not_implemented","retryable":false}]',
      );

      // When the error is decoded
      const decoded = decodeRustEngineError(native);

      // Then it becomes a RustEngineRpcError exposing the structured fields
      expect(decoded).toBeInstanceOf(RustEngineRpcError);
      const rpcError = decoded as RustEngineRpcError;
      expect(rpcError.code).toBe(-32603);
      expect(rpcError.domainCode).toBe('not_implemented');
      expect(rpcError.retryable).toBe(false);
      // And the message keeps the human-readable reason without the suffix
      expect(rpcError.message).toBe(
        'Not implemented: mcp.enable is retired in the RPC contract; ' +
          'the embedded engine returns an explicit not_implemented error',
      );
      // And the original native error stays reachable as the cause
      expect(rpcError.cause).toBe(native);
    });

    it('should leave the retryable field absent when an older binary carries no verdict', () => {
      // Given a native rejection from an older binary whose suffix predates
      // the retryable field
      const native = new Error(
        'provider rejected the request ' +
          '[mirri-rpc-error {"code":-32603,"domainCode":"provider.api_error"}]',
      );

      // When the error is decoded
      const decoded = decodeRustEngineError(native);

      // Then the structured fields decode and the verdict is absent — an
      // older binary degrades gracefully, never guessed
      const rpcError = decoded as RustEngineRpcError;
      expect(rpcError.domainCode).toBe('provider.api_error');
      expect(rpcError.retryable).toBeUndefined();
    });

    it('should pass a suffix-less error through unchanged when no suffix decodes', () => {
      // Given an ordinary error
      const error = new Error('some unrelated failure');

      // When the error is decoded
      // Then the same error object comes back untouched
      expect(decodeRustEngineError(error)).toBe(error);
    });

    it('should pass a non-error rejection through unchanged when the rejection is not an Error', () => {
      // Given a non-error rejection
      const rejection = 'just a string';

      // When it is decoded
      // Then the same value comes back untouched
      expect(decodeRustEngineError(rejection)).toBe(rejection);
      expect(decodeRustEngineError(undefined)).toBeUndefined();
    });
  });
});
