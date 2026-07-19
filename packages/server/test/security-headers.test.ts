/**
 * Security response headers (ROADMAP M6.6).
 *
 * Verifies the `onSend` hook is registered only on a non-loopback bind and
 * that HSTS is omitted while TLS is terminated elsewhere (`tls: false`).
 */

import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { pino } from 'pino';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { startServer, type RunningServer } from '../src';
import { createSecurityHeadersHook } from '../src/services/auth/securityHeaders';
import { authHeaders, fixedTokenAuth } from './helpers/serverHarness';

const createdDirs: string[] = [];
const running: RunningServer[] = [];
let prevPassword: string | undefined;

function tmpPaths(): { lockPath: string; homeDir: string } {
  const dir = mkdtempSync(join(tmpdir(), 'mirri-sec-headers-'));
  const home = mkdtempSync(join(tmpdir(), 'mirri-sec-headers-home-'));
  createdDirs.push(dir, home);
  return { lockPath: join(dir, 'lock'), homeDir: home };
}

beforeEach(() => {
  prevPassword = process.env['MIRRICODE_PASSWORD'];
});

afterEach(async () => {
  for (const r of running.splice(0)) {
    try {
      await r.close();
    } catch {
      // ignore — best-effort teardown
    }
  }
  for (const dir of createdDirs.splice(0)) {
    rmSync(dir, { recursive: true, force: true });
  }
  if (prevPassword === undefined) {
    delete process.env['MIRRICODE_PASSWORD'];
  } else {
    process.env['MIRRICODE_PASSWORD'] = prevPassword;
  }
});

async function boot(host: string): Promise<RunningServer> {
  const { lockPath, homeDir } = tmpPaths();
  if (host !== '127.0.0.1') {
    // Non-loopback binds require a password + TLS opt-out (M6.3).
    process.env['MIRRICODE_PASSWORD'] = 'test-pw';
  }
  const server = await startServer({
    serviceOverrides: [fixedTokenAuth()],
    host,
    port: 0,
    lockPath,
    insecureNoTls: host !== '127.0.0.1',
    logger: pino({ level: 'silent' }),
    coreProcessOptions: { homeDir },
  });
  running.push(server);
  return server;
}

describe('security response headers (M6.6)', () => {
  it('sets nosniff / Referrer-Policy / CSP on a non-loopback bind, without HSTS', async () => {
    const server = await boot('0.0.0.0');
    const res = await fetch(`${server.address}/api/v1/sessions`, {
      headers: authHeaders(),
    });
    expect(res.status).toBe(200);
    expect(res.headers.get('x-content-type-options')).toBe('nosniff');
    expect(res.headers.get('referrer-policy')).toBe('no-referrer');
    expect(res.headers.get('content-security-policy')).toBe(
      "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; form-action 'self'; base-uri 'none'; frame-ancestors 'self'",
    );
    // TLS is terminated by the reverse proxy in this phase → no HSTS here.
    expect(res.headers.get('strict-transport-security')).toBeNull();
  });

  it('does NOT set the security headers on a loopback bind', async () => {
    const server = await boot('127.0.0.1');
    const res = await fetch(`${server.address}/api/v1/sessions`, {
      headers: authHeaders(),
    });
    expect(res.status).toBe(200);
    expect(res.headers.get('x-content-type-options')).toBeNull();
    expect(res.headers.get('referrer-policy')).toBeNull();
    expect(res.headers.get('content-security-policy')).toBeNull();
    expect(res.headers.get('strict-transport-security')).toBeNull();
  });
});

// Unit-level coverage of the onSend hook itself, independent of the full
// server bootstrap. Asserts the CSP relaxation is scoped to styles only —
// inline scripts must stay forbidden so KaTeX/Shiki/Mermaid markup can use
// inline `style="…"` without opening up script execution.
function captureHeaders() {
  const headers = new Map<string, string>();
  const reply = {
    header(name: string, value: string) {
      headers.set(name.toLowerCase(), value);
      return reply;
    },
  };
  return { headers, reply };
}

/** Split a CSP header value into directive → source-list tokens. */
function parseCsp(csp: string): Map<string, string[]> {
  const directives = new Map<string, string[]>();
  for (const part of csp.split(';')) {
    const tokens = part.trim().split(/\s+/).filter((t) => t.length > 0);
    if (tokens.length === 0) continue;
    directives.set(tokens[0] as string, tokens.slice(1));
  }
  return directives;
}

describe('createSecurityHeadersHook', () => {
  it('stamps the defensive headers and returns the payload unchanged', async () => {
    const { headers, reply } = captureHeaders();
    const hook = createSecurityHeadersHook({ tls: false });
    const payload = { ok: true };
    const result = await hook({} as never, reply as never, payload);
    expect(result).toBe(payload);
    expect(headers.get('x-content-type-options')).toBe('nosniff');
    expect(headers.get('referrer-policy')).toBe('no-referrer');
    expect(headers.get('content-security-policy')).toBeDefined();
  });

  // KaTeX math and Shiki highlighting are injected via innerHTML with
  // per-glyph `style="…"` attributes (KaTeX carries ALL vertical/font sizing
  // in them — stripping collapses formulas into overlapping glyphs), and
  // Mermaid embeds an inline <style> in its SVG. style-src must therefore
  // allow inline styles; script-src must stay strict.
  it('allows inline styles while keeping inline scripts forbidden', async () => {
    const { headers, reply } = captureHeaders();
    const hook = createSecurityHeadersHook({ tls: false });
    await hook({} as never, reply as never, 'payload');
    const csp = headers.get('content-security-policy');
    expect(csp).toBeDefined();
    const directives = parseCsp(csp ?? '');
    const styleSrc = directives.get('style-src');
    expect(styleSrc).toContain("'self'");
    expect(styleSrc).toContain("'unsafe-inline'");
    // Assert the EFFECTIVE script policy — script-src, falling back to
    // default-src when absent — rather than matching an exact substring, so
    // regressions like default-src gaining 'unsafe-inline' (which would
    // allow inline <script> through the fallback) also fail here.
    const effectiveScriptSrc = directives.get('script-src') ?? directives.get('default-src');
    expect(effectiveScriptSrc).toBeDefined();
    expect(effectiveScriptSrc).not.toContain("'unsafe-inline'");
    expect(effectiveScriptSrc).not.toContain("'unsafe-eval'");
    expect(effectiveScriptSrc).not.toContain('data:');
  });

  it('emits HSTS only when TLS is terminated at the server', async () => {
    const plain = captureHeaders();
    await createSecurityHeadersHook({ tls: false })({} as never, plain.reply as never, '');
    expect(plain.headers.has('strict-transport-security')).toBe(false);

    const tls = captureHeaders();
    await createSecurityHeadersHook({ tls: true })({} as never, tls.reply as never, '');
    expect(tls.headers.get('strict-transport-security')).toBe('max-age=31536000');
  });
});
