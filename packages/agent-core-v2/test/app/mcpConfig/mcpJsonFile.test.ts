/**
 * Scenario: the mcp.json file-format functions — raw reads, verbatim writes,
 * stored-form validation, and the presentation views.
 *
 * Exercises the real `HostFileSystem` against temporary files. Run with
 * `pnpm --filter @mirri-ai/agent-core-v2 exec vitest run
 * test/app/mcpConfig/mcpJsonFile.test.ts`.
 */

import { mkdtempSync } from 'node:fs';
import { rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'pathe';
import { afterEach, describe, expect, it } from 'vitest';

import {
  effectiveMcpConfig,
  presentMcpConfigEntry,
  readMcpJsonEntries,
  readMcpJsonRoot,
  removeMcpServerEntry,
  setMcpServerToolFilter,
  upsertMcpServerEntry,
  userMcpJsonPath,
} from '#/app/mcpConfig/mcpJsonFile';
import { HostFileSystem } from '#/os/backends/node-local/hostFsService';
import { Error2, ErrorCodes } from '#/errors';

const fs = new HostFileSystem();

const tempDirs: string[] = [];

afterEach(async () => {
  for (const dir of tempDirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

function makeTempDir(): string {
  const dir = mkdtempSync(join(tmpdir(), 'mirri-mcp-config-'));
  tempDirs.push(dir);
  return dir;
}

async function writeRaw(path: string, text: string): Promise<void> {
  await fs.mkdir(dirname(path), { recursive: true });
  await fs.writeText(path, text);
}

function expectError2(promise: Promise<unknown>, code: string): Promise<Error2> {
  return promise.then(
    () => {
      throw new Error('expected the promise to reject');
    },
    (error: unknown) => {
      expect(error).toBeInstanceOf(Error2);
      expect((error as Error2).code).toBe(code);
      return error as Error2;
    },
  );
}

describe('userMcpJsonPath', () => {
  it('appends mcp.json to the resolved mirri home it is given', () => {
    expect(userMcpJsonPath('/home/user/.mirri-code')).toBe(
      '/home/user/.mirri-code/mcp.json',
    );
    expect(userMcpJsonPath('/custom/home')).toBe('/custom/home/mcp.json');
  });
});

describe('readMcpJsonRoot', () => {
  it('should return an empty envelope when the file is missing', async () => {
    const dir = makeTempDir();

    await expect(readMcpJsonRoot(fs, join(dir, 'mcp.json'))).resolves.toEqual({
      mcpServers: {},
    });
  });

  it('should return an empty envelope when the file is blank', async () => {
    const path = join(makeTempDir(), 'mcp.json');
    await writeRaw(path, '   \n');

    await expect(readMcpJsonRoot(fs, path)).resolves.toEqual({ mcpServers: {} });
  });

  it('should return the parsed root with unknown top-level keys preserved', async () => {
    const path = join(makeTempDir(), 'mcp.json');
    await writeRaw(path, '{"mcpServers":{"a":{"command":"x"}},"meta":{"v":2}}');

    await expect(readMcpJsonRoot(fs, path)).resolves.toEqual({
      mcpServers: { a: { command: 'x' } },
      meta: { v: 2 },
    });
  });

  it('should throw CONFIG_INVALID when the file is not JSON', async () => {
    const path = join(makeTempDir(), 'mcp.json');
    await writeRaw(path, '{not json');

    await expectError2(readMcpJsonRoot(fs, path), ErrorCodes.CONFIG_INVALID);
  });

  it('should throw CONFIG_INVALID when the root is not a JSON object', async () => {
    const path = join(makeTempDir(), 'mcp.json');
    await writeRaw(path, '[1, 2]');

    await expectError2(readMcpJsonRoot(fs, path), ErrorCodes.CONFIG_INVALID);
  });
});

describe('readMcpJsonEntries', () => {
  it('should return the stored entries verbatim without validation', async () => {
    const path = join(makeTempDir(), 'mcp.json');
    await writeRaw(
      path,
      '{"mcpServers":{"flat":{"command":"npx"},"broken":{"enabled":true}},"meta":1}',
    );

    await expect(readMcpJsonEntries(fs, path)).resolves.toEqual({
      flat: { command: 'npx' },
      broken: { enabled: true },
    });
  });

  it('should return an empty record when the envelope is missing', async () => {
    const path = join(makeTempDir(), 'mcp.json');
    await writeRaw(path, '{"meta":1}');

    await expect(readMcpJsonEntries(fs, path)).resolves.toEqual({});
  });

  it('should throw CONFIG_INVALID when mcpServers is not an object', async () => {
    const path = join(makeTempDir(), 'mcp.json');
    await writeRaw(path, '{"mcpServers":"nope"}');

    await expectError2(readMcpJsonEntries(fs, path), ErrorCodes.CONFIG_INVALID);
  });
});

describe('upsertMcpServerEntry', () => {
  it('should write pretty JSON with a trailing newline', async () => {
    const path = join(makeTempDir(), 'mcp.json');

    await upsertMcpServerEntry(fs, path, 'fresh', { command: 'npx' });

    expect(await fs.readText(path)).toBe(
      '{\n  "mcpServers": {\n    "fresh": {\n      "command": "npx"\n    }\n  }\n}\n',
    );
  });

  it('should store the config verbatim and keep existing entries with their `${VAR}` references', async () => {
    const path = join(makeTempDir(), 'mcp.json');
    await writeRaw(
      path,
      '{"mcpServers":{"existing":{"transport":"stdio","command":"node","args":["${HOME}/bin/server"]}}}',
    );

    await upsertMcpServerEntry(fs, path, 'fresh', {
      transport: 'stdio',
      command: 'npx',
      args: ['-y', 'server'],
      env: { TOKEN: '${MY_TOKEN}' },
    });

    const root = await readMcpJsonRoot(fs, path);
    expect(root['mcpServers']).toEqual({
      existing: {
        transport: 'stdio',
        command: 'node',
        args: ['${HOME}/bin/server'],
      },
      fresh: {
        transport: 'stdio',
        command: 'npx',
        args: ['-y', 'server'],
        env: { TOKEN: '${MY_TOKEN}' },
      },
    });
  });

  it('should create the file with the envelope and parent directories when missing', async () => {
    const dir = makeTempDir();
    const path = join(dir, 'nested', 'mcp.json');

    await upsertMcpServerEntry(fs, path, 'fresh', {
      transport: 'http',
      url: 'https://example.test/mcp',
    });

    await expect(readMcpJsonEntries(fs, path)).resolves.toEqual({
      fresh: { transport: 'http', url: 'https://example.test/mcp' },
    });
  });

  it('should accept the flat Claude form without rewriting the stored shape', async () => {
    const path = join(makeTempDir(), 'mcp.json');

    await upsertMcpServerEntry(fs, path, 'flat', { command: 'npx', args: ['-y', 's'] });

    await expect(readMcpJsonEntries(fs, path)).resolves.toEqual({
      flat: { command: 'npx', args: ['-y', 's'] },
    });
  });

  it('should accept an http url carrying a `${VAR}` reference (stored-form validation)', async () => {
    const path = join(makeTempDir(), 'mcp.json');

    await upsertMcpServerEntry(fs, path, 'remote', {
      transport: 'http',
      url: '${API_BASE}/mcp',
    });

    await expect(readMcpJsonEntries(fs, path)).resolves.toEqual({
      remote: { transport: 'http', url: '${API_BASE}/mcp' },
    });
  });

  it('should reject a config with no transport, command, or url and write nothing', async () => {
    const path = join(makeTempDir(), 'mcp.json');

    const error = await expectError2(
      upsertMcpServerEntry(fs, path, 'broken', { enabled: true }),
      ErrorCodes.VALIDATION_FAILED,
    );
    expect(error.message).toContain('broken');

    await expect(readMcpJsonEntries(fs, path)).resolves.toEqual({});
  });

  it('should reject a non-object config', async () => {
    const path = join(makeTempDir(), 'mcp.json');

    await expectError2(
      upsertMcpServerEntry(fs, path, 'broken', 'nope'),
      ErrorCodes.VALIDATION_FAILED,
    );
  });

  it('should preserve unknown top-level keys of the file it writes', async () => {
    const path = join(makeTempDir(), 'mcp.json');
    await writeRaw(path, '{"mcpServers":{},"meta":{"v":2}}');

    await upsertMcpServerEntry(fs, path, 'fresh', { command: 'npx' });

    await expect(readMcpJsonRoot(fs, path)).resolves.toEqual({
      mcpServers: { fresh: { command: 'npx' } },
      meta: { v: 2 },
    });
  });
});

describe('removeMcpServerEntry', () => {
  it('should remove the named entry and keep the rest', async () => {
    const path = join(makeTempDir(), 'mcp.json');
    await writeRaw(path, '{"mcpServers":{"a":{"command":"x"},"b":{"command":"y"}},"meta":1}');

    await expect(removeMcpServerEntry(fs, path, 'a')).resolves.toBe(true);

    await expect(readMcpJsonRoot(fs, path)).resolves.toEqual({
      mcpServers: { b: { command: 'y' } },
      meta: 1,
    });
  });

  it('should be an idempotent success when the name is absent', async () => {
    const path = join(makeTempDir(), 'mcp.json');
    await writeRaw(path, '{"mcpServers":{"a":{"command":"x"}}}');

    await expect(removeMcpServerEntry(fs, path, 'missing')).resolves.toBe(false);
    await expect(readMcpJsonEntries(fs, path)).resolves.toEqual({ a: { command: 'x' } });
  });

  it('should create nothing when the file does not exist', async () => {
    const dir = makeTempDir();
    const path = join(dir, 'mcp.json');

    await expect(removeMcpServerEntry(fs, path, 'a')).resolves.toBe(false);
    await expect(readMcpJsonEntries(fs, path)).resolves.toEqual({});
  });
});

describe('setMcpServerToolFilter', () => {
  it('should replace present fields and clear absent ones', async () => {
    const path = join(makeTempDir(), 'mcp.json');
    await writeRaw(
      path,
      '{"mcpServers":{"a":{"command":"x","enabledTools":["t1","t2"],"disabledTools":["t3"]}}}',
    );

    await setMcpServerToolFilter(fs, path, 'a', {
      enabledTools: ['t1'],
    });

    await expect(readMcpJsonEntries(fs, path)).resolves.toEqual({
      a: { command: 'x', enabledTools: ['t1'] },
    });
  });

  it('should set both fields when both are present in the patch', async () => {
    const path = join(makeTempDir(), 'mcp.json');
    await writeRaw(path, '{"mcpServers":{"a":{"command":"x"}}}');

    await setMcpServerToolFilter(fs, path, 'a', {
      enabledTools: ['t1'],
      disabledTools: ['t2'],
    });

    await expect(readMcpJsonEntries(fs, path)).resolves.toEqual({
      a: { command: 'x', enabledTools: ['t1'], disabledTools: ['t2'] },
    });
  });

  it('should throw MCP_SERVER_NOT_FOUND when the entry is absent', async () => {
    const path = join(makeTempDir(), 'mcp.json');
    await writeRaw(path, '{"mcpServers":{"a":{"command":"x"}}}');

    await expectError2(
      setMcpServerToolFilter(fs, path, 'missing', { enabledTools: ['t1'] }),
      ErrorCodes.MCP_SERVER_NOT_FOUND,
    );
  });

  it('should not rewrite the file when the patch changes nothing', async () => {
    const path = join(makeTempDir(), 'mcp.json');
    await writeRaw(
      path,
      '{"mcpServers":{"a":{"command":"x","enabledTools":["t1"]}}}',
    );
    const before = await fs.readText(path);

    await setMcpServerToolFilter(fs, path, 'a', { enabledTools: ['t1'] });

    expect(await fs.readText(path)).toBe(before);
  });
});

describe('presentMcpConfigEntry', () => {
  it('should infer the stdio transport tag when command is present and transport is absent', () => {
    expect(presentMcpConfigEntry({ command: 'npx', args: ['-y'] })).toEqual({
      transport: 'stdio',
      command: 'npx',
      args: ['-y'],
    });
  });

  it('should prefer command over url when both are present', () => {
    expect(presentMcpConfigEntry({ command: 'echo', url: 'https://example.test' })).toMatchObject({
      transport: 'stdio',
      command: 'echo',
      url: 'https://example.test',
    });
  });

  it('should infer the http transport tag when url is present and transport is absent', () => {
    expect(presentMcpConfigEntry({ url: '${API_BASE}/mcp' })).toEqual({
      transport: 'http',
      url: '${API_BASE}/mcp',
    });
  });

  it('should leave an entry with an explicit transport untouched', () => {
    const raw = { transport: 'stdio', command: 'npx' };
    expect(presentMcpConfigEntry(raw)).toBe(raw);
  });

  it('should return an empty object for a non-object entry', () => {
    expect(presentMcpConfigEntry('broken')).toEqual({});
  });
});

describe('effectiveMcpConfig', () => {
  it('should derive the effective state from a full config', () => {
    expect(
      effectiveMcpConfig({
        transport: 'stdio',
        command: 'npx',
        enabled: false,
        enabledTools: ['t1'],
        disabledTools: ['t2'],
        startupTimeoutMs: 5000,
        toolTimeoutMs: 10000,
      }),
    ).toEqual({
      enabled: false,
      enabledTools: ['t1'],
      disabledTools: ['t2'],
      startupTimeoutMs: 5000,
      toolTimeoutMs: 10000,
    });
  });

  it('should default enabled to true when omitted', () => {
    expect(effectiveMcpConfig({ transport: 'stdio', command: 'npx' })).toEqual({
      enabled: true,
    });
  });

  it('should derive overrides from a stored form with a `${VAR}` url', () => {
    expect(
      effectiveMcpConfig({ transport: 'http', url: '${API_BASE}/mcp', enabled: false }),
    ).toEqual({ enabled: false });
  });

  it('should degrade to enabled-only for an unparseable entry', () => {
    expect(effectiveMcpConfig({ enabled: true })).toEqual({ enabled: true });
  });
});
