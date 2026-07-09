import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { afterEach, describe, expect, it, vi } from 'vitest';

import {
  createMirriDefaultHeaders,
  createMirriDeviceHeaders,
  createMirriDeviceId,
  createMirriUserAgent,
  MIRRICODE_PLATFORM,
  readMirriDeviceId,
} from '../src/identity';

const tmpRoots: string[] = [];

function tempHome(): string {
  const dir = mkdtempSync(join(tmpdir(), 'kimi-oauth-identity-'));
  tmpRoots.push(dir);
  return dir;
}

afterEach(() => {
  for (const dir of tmpRoots.splice(0)) {
    rmSync(dir, { recursive: true, force: true });
  }
});

describe('Kimi identity factories', () => {
  it('creates and reuses a device id in the explicit homeDir', () => {
    const homeDir = tempHome();
    const first = createMirriDeviceId(homeDir);
    const second = createMirriDeviceId(homeDir);

    expect(first).toMatch(/^[0-9a-f-]+$/);
    expect(second).toBe(first);
  });

  it('creates different device ids for different homeDir values', () => {
    const first = createMirriDeviceId(tempHome());
    const second = createMirriDeviceId(tempHome());

    expect(second).not.toBe(first);
  });

  it('reads an existing device id without creating one when missing', () => {
    const homeDir = tempHome();

    expect(readMirriDeviceId(homeDir)).toBeNull();
    expect(readMirriDeviceId(homeDir)).toBeNull();

    const first = createMirriDeviceId(homeDir);
    expect(readMirriDeviceId(homeDir)).toBe(first);
  });

  it('treats an empty device id file as missing', () => {
    const homeDir = tempHome();
    writeFileSync(join(homeDir, 'device_id'), '  \n', 'utf-8');

    expect(readMirriDeviceId(homeDir)).toBeNull();
  });

  it('creates complete X-Msh device headers from host version', () => {
    const headers = createMirriDeviceHeaders({
      homeDir: tempHome(),
      version: '1.2.3-test',
    });

    expect(headers['X-Msh-Platform']).toBe(MIRRICODE_PLATFORM);
    expect(headers['X-Msh-Version']).toBe('1.2.3-test');
    expect(headers['X-Msh-Device-Name']).toBeTruthy();
    expect(headers['X-Msh-Device-Model']).toBeTruthy();
    expect(headers['X-Msh-Os-Version']).toBeTruthy();
    expect(headers['X-Msh-Device-Id']).toMatch(/^[0-9a-f-]+$/);
  });

  it('creates mirri-code-cli User-Agent and appends suffix only to UA', () => {
    expect(
      createMirriUserAgent({
        userAgentProduct: 'mirri-code-cli',
        version: '1.2.3',
      }),
    ).toBe('mirri-code-cli/1.2.3');
    expect(
      createMirriUserAgent({
        userAgentProduct: 'mirri-code-cli',
        version: '1.2.3',
        userAgentSuffix: 'wire 4.5.6',
      }),
    ).toBe('mirri-code-cli/1.2.3 (wire 4.5.6)');
  });

  it('merges User-Agent and device headers into default headers', () => {
    const headers = createMirriDefaultHeaders({
      homeDir: tempHome(),
      userAgentProduct: 'mirri-code-cli',
      version: '1.2.3',
    });

    expect(headers['User-Agent']).toBe('mirri-code-cli/1.2.3');
    expect(headers['X-Msh-Version']).toBe('1.2.3');
    expect(headers['X-Msh-Device-Id']).toMatch(/^[0-9a-f-]+$/);
  });
});

// HTTP header values must be plain ASCII without leading/trailing whitespace.
// The public factories surface the sanitizer used for User-Agent and X-Msh-*.
describe('ascii header value sanitization', () => {
  it('strips a trailing newline from a header value', () => {
    const ua = createMirriUserAgent({ userAgentProduct: 'mirri-code-cli', version: '6.8.0-101\n' });
    expect(ua).toBe('mirri-code-cli/6.8.0-101');
  });

  it('drops non-ASCII codepoints while keeping the ASCII remainder', () => {
    const ua = createMirriUserAgent({ userAgentProduct: 'mirri-code-cli', version: 'héllo' });
    expect(ua).toBe('mirri-code-cli/hllo');
  });

  it('uses the unknown fallback when every hostname codepoint is non-ASCII', async () => {
    vi.resetModules();
    vi.doMock('node:os', async () => {
      const actual = await vi.importActual<typeof import('node:os')>('node:os');
      return {
        ...actual,
        hostname: () => '你好',
        release: () => '1.0.0',
        type: () => 'Linux',
        arch: () => 'x64',
      };
    });

    try {
      const { createMirriDeviceHeaders: createHeaders } = await import('../src/identity');
      const headers = createHeaders({ homeDir: tempHome(), version: '1.0.0' });
      expect(headers['X-Msh-Device-Name']).toBe('unknown');
    } finally {
      vi.doUnmock('node:os');
      vi.resetModules();
    }
  });

  it('keeps every device-header value free of leading or trailing whitespace', async () => {
    vi.resetModules();
    vi.doMock('node:os', async () => {
      const actual = await vi.importActual<typeof import('node:os')>('node:os');
      return {
        ...actual,
        hostname: () => '  myhost  ',
        release: () => '#101-Ubuntu SMP\n',
        type: () => 'Linux',
        arch: () => 'x64',
      };
    });

    try {
      const { createMirriDeviceHeaders: createHeaders } = await import('../src/identity');
      const headers = createHeaders({ homeDir: tempHome(), version: '1.0.0' });
      for (const [key, value] of Object.entries(headers)) {
        expect(value, `header ${key} has untrimmed whitespace: ${JSON.stringify(value)}`).toBe(
          value.trim(),
        );
      }
    } finally {
      vi.doUnmock('node:os');
      vi.resetModules();
    }
  });

  it('falls back to Darwin kernel version when sw_vers is unavailable', async () => {
    vi.resetModules();
    vi.doMock('node:os', async () => ({
      ...(await vi.importActual<typeof import('node:os')>('node:os')),
      hostname: () => 'my-mac',
      release: () => '25.5.0',
      type: () => 'Darwin',
      arch: () => 'arm64',
    }));
    // Force the sw_vers lookup to fail so the test is deterministic on macOS too,
    // where the real binary would otherwise return the host's product version.
    vi.doMock('node:child_process', async () => ({
      ...(await vi.importActual<typeof import('node:child_process')>('node:child_process')),
      execFileSync: () => {
        throw new Error('ENOENT');
      },
    }));

    try {
      const { createMirriDeviceHeaders } = await import('../src/identity');
      const headers = createMirriDeviceHeaders({ homeDir: tempHome(), version: '1.0.0' });
      expect(headers['X-Msh-Device-Model']).toBe('macOS 25.5.0 arm64');
    } finally {
      vi.doUnmock('node:os');
      vi.doUnmock('node:child_process');
      vi.resetModules();
    }
  });
});
