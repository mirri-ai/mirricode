/**
 * Security hardening verification for kap-server (B0-L21).
 *
 * Three assertions required by the audit:
 *   1. Non-loopback bind without TLS is rejected (both LAN and public classes).
 *   2. Auth-failure rate limit triggers after the configured threshold on a
 *      non-loopback bind.
 *   3. Bearer auth is enforced on every /api/* route on a non-loopback bind.
 *
 * This file does NOT use the F4 harness — all tests stand on their own.
 */

import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { type RunningServer, startServer } from '../src/start';
import { startReadyServer } from './helpers/startReadyServer';
import { TEST_HOST_IDENTITY } from './helpers/hostIdentity';

describe('kap-server security hardening', () => {
  let server: RunningServer | undefined;
  let home: string | undefined;

  beforeEach(async () => {
    home = await mkdtemp(join(tmpdir(), 'mirri-security-hardening-'));
  });

  afterEach(async () => {
    if (server !== undefined) {
      await server.close();
      server = undefined;
    }
    if (home !== undefined) {
      await rm(home, { recursive: true, force: true });
      home = undefined;
    }
  });

  // ── 1. Non-loopback bind without TLS rejected ────────────────────────

  describe('non-loopback bind gate', () => {
    it('should reject a public bind (0.0.0.0) without TLS opt-out', async () => {
      await expect(
        startServer({
          hostIdentity: TEST_HOST_IDENTITY,
          host: '0.0.0.0',
          port: 0,
          homeDir: home,
          logLevel: 'silent',
        }),
      ).rejects.toThrow(/Refusing to bind 0\.0\.0\.0 \(public\) without TLS/);
    });

    it('should reject a LAN bind (192.168.1.1) without TLS opt-out', async () => {
      await expect(
        startServer({
          hostIdentity: TEST_HOST_IDENTITY,
          host: '192.168.1.1',
          port: 0,
          homeDir: home,
          logLevel: 'silent',
        }),
      ).rejects.toThrow(/Refusing to bind 192\.168\.1\.1 \(lan\) without TLS/);
    });

    it('should reject a LAN bind (10.0.0.1) without TLS opt-out', async () => {
      await expect(
        startServer({
          hostIdentity: TEST_HOST_IDENTITY,
          host: '10.0.0.1',
          port: 0,
          homeDir: home,
          logLevel: 'silent',
        }),
      ).rejects.toThrow(/Refusing to bind 10\.0\.0\.1 \(lan\) without TLS/);
    });

    it('should boot a non-loopback bind when --insecure-no-tls is set', async () => {
      server = await startReadyServer({
        hostIdentity: TEST_HOST_IDENTITY,
        host: '0.0.0.0',
        port: 0,
        homeDir: home,
        logLevel: 'silent',
        insecureNoTls: true,
      });
      expect(server).toBeDefined();
      const res = await server.app.inject({
        method: 'GET',
        url: '/api/v1/healthz',
      });
      expect(res.statusCode).toBe(200);
    });
  });

  // ── 2. Rate limit triggers after threshold on non-loopback bind ───────

  describe('auth-failure rate limit on non-loopback', () => {
    it('should return 429 after the default threshold of bad tokens', async () => {
      // Boot a public server with a fast in-memory auth service (no bcrypt).
      server = await startReadyServer({
        hostIdentity: TEST_HOST_IDENTITY,
        host: '0.0.0.0',
        port: 0,
        homeDir: home,
        logLevel: 'silent',
        insecureNoTls: true,
        authTokenService: {
          _serviceBrand: undefined,
          getToken: () => 'test-persistent-token',
          isValid: async (candidate) => candidate === 'test-persistent-token',
        },
      });

      // The default maxFailures is 10. Send 10 bad tokens, all should get 401.
      for (let i = 0; i < 10; i += 1) {
        const res = await server.app.inject({
          method: 'GET',
          url: '/api/v1/sessions',
          headers: { authorization: 'Bearer wrong-token' },
        });
        expect(res.statusCode).toBe(401);
      }

      // The 11th request should be rate-limited with 429 / 42901.
      const limited = await server.app.inject({
        method: 'GET',
        url: '/api/v1/sessions',
        headers: { authorization: 'Bearer wrong-token' },
      });
      expect(limited.statusCode).toBe(429);
      const body = limited.json() as Record<string, unknown>;
      expect(body['code']).toBe(42901);
      expect(body['msg']).toBe('Too many failed auth attempts');
    });

    it('should return 429 even with a valid token once banned', async () => {
      server = await startReadyServer({
        hostIdentity: TEST_HOST_IDENTITY,
        host: '0.0.0.0',
        port: 0,
        homeDir: home,
        logLevel: 'silent',
        insecureNoTls: true,
        authTokenService: {
          _serviceBrand: undefined,
          getToken: () => 'test-persistent-token',
          isValid: async (candidate) => candidate === 'test-persistent-token',
        },
      });

      // Exhaust the failure budget.
      for (let i = 0; i < 10; i += 1) {
        await server.app.inject({
          method: 'GET',
          url: '/api/v1/sessions',
          headers: { authorization: 'Bearer wrong' },
        });
      }

      // Even a valid token should be rejected while the source is banned.
      const validButBanned = await server.app.inject({
        method: 'GET',
        url: '/api/v1/sessions',
        headers: { authorization: 'Bearer test-persistent-token' },
      });
      expect(validButBanned.statusCode).toBe(429);
    });

    it('should NOT rate-limit on a loopback bind (no limiter wired)', async () => {
      server = await startReadyServer({
        hostIdentity: TEST_HOST_IDENTITY,
        host: '127.0.0.1',
        port: 0,
        homeDir: home,
        logLevel: 'silent',
        authTokenService: {
          _serviceBrand: undefined,
          getToken: () => 'test-persistent-token',
          isValid: async (candidate) => candidate === 'test-persistent-token',
        },
      });

      // On loopback, the limiter is not wired, so repeated failures stay at 401.
      for (let i = 0; i < 12; i += 1) {
        const res = await server.app.inject({
          method: 'GET',
          url: '/api/v1/sessions',
          headers: { authorization: 'Bearer wrong' },
        });
        expect(res.statusCode).toBe(401);
      }
    });
  });

  // ── 3. Bearer auth enforced on non-loopback bind ─────────────────────

  describe('bearer auth enforced on non-loopback', () => {
    it('should reject /api/* without a token with 40101', async () => {
      server = await startReadyServer({
        hostIdentity: TEST_HOST_IDENTITY,
        host: '0.0.0.0',
        port: 0,
        homeDir: home,
        logLevel: 'silent',
        insecureNoTls: true,
      });
      const res = await server.app.inject({
        method: 'GET',
        url: '/api/v1/sessions',
      });
      expect(res.statusCode).toBe(401);
      const body = res.json() as Record<string, unknown>;
      expect(body['code']).toBe(40101);
    });

    it('should reject /api/* with a wrong token with 40101', async () => {
      server = await startReadyServer({
        hostIdentity: TEST_HOST_IDENTITY,
        host: '0.0.0.0',
        port: 0,
        homeDir: home,
        logLevel: 'silent',
        insecureNoTls: true,
      });
      const res = await server.app.inject({
        method: 'GET',
        url: '/api/v1/sessions',
        headers: { authorization: 'Bearer wrong-token' },
      });
      expect(res.statusCode).toBe(401);
      const body = res.json() as Record<string, unknown>;
      expect(body['code']).toBe(40101);
    });

    it('should accept /api/* with the persistent token', async () => {
      server = await startReadyServer({
        hostIdentity: TEST_HOST_IDENTITY,
        host: '0.0.0.0',
        port: 0,
        homeDir: home,
        logLevel: 'silent',
        insecureNoTls: true,
      });
      const token = server.authTokenService.getToken();
      const res = await server.app.inject({
        method: 'GET',
        url: '/api/v1/sessions',
        headers: { authorization: `Bearer ${token}` },
      });
      expect(res.statusCode).toBe(200);
    });

    it('should still bypass healthz without a token', async () => {
      server = await startReadyServer({
        hostIdentity: TEST_HOST_IDENTITY,
        host: '0.0.0.0',
        port: 0,
        homeDir: home,
        logLevel: 'silent',
        insecureNoTls: true,
      });
      const res = await server.app.inject({
        method: 'GET',
        url: '/api/v1/healthz',
      });
      expect(res.statusCode).toBe(200);
    });

    it('should require auth for /openapi.json', async () => {
      server = await startReadyServer({
        hostIdentity: TEST_HOST_IDENTITY,
        host: '0.0.0.0',
        port: 0,
        homeDir: home,
        logLevel: 'silent',
        insecureNoTls: true,
      });
      const res = await server.app.inject({
        method: 'GET',
        url: '/openapi.json',
      });
      expect(res.statusCode).toBe(401);
    });
  });
});
