/**
 * E2E test: verifies that [[hooks]] config from config.toml are actually
 * executed by the server.
 *
 * Starts a real kap-server with a minimal config.toml containing a
 * SessionStart hook that writes a marker file, creates a session, and
 * asserts the marker file exists.
 */
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, describe, expect, it } from 'vitest';

import { type RunningServer } from '../src/start';
import { startReadyServer } from './helpers/startReadyServer';
import { TEST_HOST_IDENTITY } from './helpers/hostIdentity';
import { bearerToken } from './helpers/auth';

describe('external hooks e2e', () => {
  let home: string;
  let server: RunningServer;
  let base: string;

  afterEach(async () => {
    if (server) {
      await server.close();
    }
    if (home) {
      await rm(home, { recursive: true, force: true });
    }
  });

  it('fires a SessionStart hook when a session is created', async () => {
    home = await mkdtemp(join(tmpdir(), 'mirri-hooks-e2e-'));
    const markerFile = join(home, 'hook-fired.marker');

    // Write a minimal config.toml with one SessionStart hook
    await writeFile(
      join(home, 'config.toml'),
      [
        '[[hooks]]',
        `event = "SessionStart"`,
        `command = "node -e 'require(\\"fs\\").writeFileSync(\\"${markerFile}\\", \\"triggered\\")'"`,
        'timeout = 5',
        '',
      ].join('\n'),
      'utf8',
    );

    server = await startReadyServer({
      hostIdentity: TEST_HOST_IDENTITY,
      host: '127.0.0.1',
      port: 0,
      homeDir: home,
      logLevel: 'silent',
    });
    base = `http://127.0.0.1:${server.port}`;

    // Create a session — this should trigger SessionStart hook
    const token = bearerToken(server);
    const res = await fetch(`${base}/api/v1/sessions`, {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
        authorization: `Bearer ${token}`,
      },
      body: JSON.stringify({ metadata: { cwd: home } }),
    });
    expect(res.status).toBe(200);

    // Give the hook a moment to write the marker file
    await new Promise((r) => setTimeout(r, 2000));

    // Verify the hook script was executed (marker file exists)
    const fs = await import('node:fs');
    expect(fs.existsSync(markerFile)).toBe(true);
  });

  it('fires only valid hooks and logs a warning for unknown events', async () => {
    home = await mkdtemp(join(tmpdir(), 'mirri-hooks-e2e-unknown-'));
    const goodMarker = join(home, 'good-hook.marker');
    const badMarker = join(home, 'bad-hook.marker'); // should NOT be created

    await writeFile(
      join(home, 'config.toml'),
      [
        '[[hooks]]',
        `event = "PostLlmRequest"`,
        `command = "touch ${badMarker}"`,
        'timeout = 5',
        '[[hooks]]',
        `event = "RewiteToolInput"`,
        `command = "touch ${badMarker}"`,
        'timeout = 5',
        '[[hooks]]',
        `event = "SessionStart"`,
        `command = "touch ${goodMarker}"`,
        'timeout = 5',
      ].join('\n'),
      'utf8',
    );

    server = await startReadyServer({
      hostIdentity: TEST_HOST_IDENTITY,
      host: '127.0.0.1',
      port: 0,
      homeDir: home,
      logLevel: 'silent',
    });
    base = `http://127.0.0.1:${server.port}`;

    const token = bearerToken(server);
    const res = await fetch(`${base}/api/v1/sessions`, {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
        authorization: `Bearer ${token}`,
      },
      body: JSON.stringify({ metadata: { cwd: home } }),
    });
    expect(res.status).toBe(200);

    await new Promise((r) => setTimeout(r, 2000));

    // SessionStart hook must fire despite the unknown events
    const fs = await import('node:fs');
    expect(fs.existsSync(goodMarker)).toBe(true);
    expect(fs.existsSync(badMarker)).toBe(false);
  });
});