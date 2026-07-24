import type { Tool } from '@mirri-ai/kosong';
import { describe, expect, it } from 'vitest';

import type { Agent } from '../../src/agent';
import { ToolManager } from '../../src/agent/tool';
import type { MCPClient } from '../../src/mcp/types';
import { executeTool } from '../tools/fixtures/execute-tool';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Minimal mock Agent with an `emitEvent` that can be observed in tests. */
function fakeAgent(emitCalls?: unknown[]): Agent {
  return {
    records: {
      logRecord() {},
    },
    config: {
      data: () => ({ provider: undefined }),
    },
    goal: {
      getGoal: () => ({ goal: null }),
    },
    emitEvent(event: unknown) {
      emitCalls?.push(event);
    },
  } as unknown as Agent;
}

/** Creates a mock MCPClient with 'echo' and 'noop' tools. */
function fakeClient(): MCPClient {
  return {
    async listTools() {
      return [
        {
          name: 'echo',
          description: 'Echoes back',
          inputSchema: {
            type: 'object',
            properties: { text: { type: 'string' } },
            required: ['text'],
          },
        },
        {
          name: 'noop',
          description: 'Does nothing',
          inputSchema: { type: 'object', properties: {} },
        },
      ];
    },
    async callTool(name, args) {
      if (name === 'echo') {
        return { content: [{ type: 'text', text: String(args['text']) }], isError: false };
      }
      return { content: [{ type: 'text', text: 'ok' }], isError: false };
    },
  };
}

/** Creates a second mock MCPClient with a single 'calc' tool. */
function fakeCalcClient(): MCPClient {
  return {
    async listTools() {
      return [
        {
          name: 'calc',
          description: 'Calculates',
          inputSchema: { type: 'object', properties: {} },
        },
      ];
    },
    async callTool() {
      return { content: [{ type: 'text', text: '42' }], isError: false };
    },
  };
}

/** Converts an MCPClient's listTools output into the kosong Tool[] shape. */
async function discoverTools(client: MCPClient): Promise<Tool[]> {
  const defs = await client.listTools();
  return defs.map((d) => ({
    name: d.name,
    description: d.description,
    parameters: d.inputSchema as Record<string, unknown>,
  }));
}

// ---------------------------------------------------------------------------
// Test suite
// ---------------------------------------------------------------------------

describe('MCP tool runtime enable/disable', () => {
  // ── Server-level disable/enable ─────────────────────────────────────────

  it('should hide all tools from a disabled MCP server in loopTools', async () => {
    const tm = new ToolManager(fakeAgent());
    tm.setActiveTools(['mcp__*']);
    const client = fakeClient();
    tm.registerMcpServer('s', client, await discoverTools(client));

    // Before disabling, both tools are visible.
    expect(tm.loopTools.map((t) => t.name).toSorted()).toEqual([
      'mcp__s__echo',
      'mcp__s__noop',
    ]);

    tm.disableMcpServer('s');

    // After disabling, no tools from server 's' appear in loopTools.
    expect(tm.loopTools.map((t) => t.name)).toEqual([]);
  });

  it('should restore tools to loopTools when a disabled MCP server is re-enabled', async () => {
    const tm = new ToolManager(fakeAgent());
    tm.setActiveTools(['mcp__*']);
    const client = fakeClient();
    tm.registerMcpServer('s', client, await discoverTools(client));

    tm.disableMcpServer('s');
    expect(tm.loopTools.map((t) => t.name)).toEqual([]);

    tm.enableMcpServer('s');

    expect(tm.loopTools.map((t) => t.name).toSorted()).toEqual([
      'mcp__s__echo',
      'mcp__s__noop',
    ]);
  });

  it('should mark tools as inactive in toolInfos when server is disabled', async () => {
    const tm = new ToolManager(fakeAgent());
    tm.setActiveTools(['mcp__*']);
    const client = fakeClient();
    tm.registerMcpServer('s', client, await discoverTools(client));

    tm.disableMcpServer('s');

    const mcpInfos = [...tm.toolInfos()].filter((i) => i.source === 'mcp');
    expect(mcpInfos).toEqual([
      expect.objectContaining({ name: 'mcp__s__echo', active: false }),
      expect.objectContaining({ name: 'mcp__s__noop', active: false }),
    ]);
  });

  it('should exclude disabled server tools from loadableDynamicToolNames', async () => {
    const tm = new ToolManager(fakeAgent());
    tm.setActiveTools(['mcp__*']);
    const client = fakeClient();
    tm.registerMcpServer('s', client, await discoverTools(client));

    expect(tm.loadableDynamicToolNames()).toContain('mcp__s__echo');
    expect(tm.loadableDynamicToolNames()).toContain('mcp__s__noop');

    tm.disableMcpServer('s');

    expect(tm.loadableDynamicToolNames()).toEqual([]);
  });

  // ── Tool-level disable/enable ───────────────────────────────────────────

  it('should hide a specific disabled MCP tool from loopTools', async () => {
    const tm = new ToolManager(fakeAgent());
    tm.setActiveTools(['mcp__*']);
    const client = fakeClient();
    tm.registerMcpServer('s', client, await discoverTools(client));

    tm.disableMcpTool('mcp__s__echo');

    // Only noop should remain visible.
    expect(tm.loopTools.map((t) => t.name)).toEqual(['mcp__s__noop']);
  });

  it('should restore a specific MCP tool to loopTools when re-enabled', async () => {
    const tm = new ToolManager(fakeAgent());
    tm.setActiveTools(['mcp__*']);
    const client = fakeClient();
    tm.registerMcpServer('s', client, await discoverTools(client));

    tm.disableMcpTool('mcp__s__echo');
    expect(tm.loopTools.map((t) => t.name)).toEqual(['mcp__s__noop']);

    tm.enableMcpTool('mcp__s__echo');

    expect(tm.loopTools.map((t) => t.name).toSorted()).toEqual([
      'mcp__s__echo',
      'mcp__s__noop',
    ]);
  });

  it('should keep other tools visible when only one tool is disabled', async () => {
    const tm = new ToolManager(fakeAgent());
    tm.setActiveTools(['mcp__*']);
    const client = fakeClient();
    tm.registerMcpServer('s', client, await discoverTools(client));

    tm.disableMcpTool('mcp__s__echo');

    // noop is unaffected.
    const noopInfo = [...tm.toolInfos()].find((i) => i.name === 'mcp__s__noop');
    expect(noopInfo?.active).toBe(true);
    expect(tm.loopTools.map((t) => t.name)).toEqual(['mcp__s__noop']);
  });

  it('should mark a disabled tool as inactive in toolInfos', async () => {
    const tm = new ToolManager(fakeAgent());
    tm.setActiveTools(['mcp__*']);
    const client = fakeClient();
    tm.registerMcpServer('s', client, await discoverTools(client));

    tm.disableMcpTool('mcp__s__echo');

    const mcpInfos = [...tm.toolInfos()].filter((i) => i.source === 'mcp');
    expect(mcpInfos).toEqual([
      expect.objectContaining({ name: 'mcp__s__echo', active: false }),
      expect.objectContaining({ name: 'mcp__s__noop', active: true }),
    ]);
  });

  // ── Combination scenarios ────────────────────────────────────────────────

  it('should keep tool hidden when server is disabled even if tool is explicitly enabled', async () => {
    const tm = new ToolManager(fakeAgent());
    tm.setActiveTools(['mcp__*']);
    const client = fakeClient();
    tm.registerMcpServer('s', client, await discoverTools(client));

    // Disable the server, then explicitly enable one of its tools.
    tm.disableMcpServer('s');
    tm.enableMcpTool('mcp__s__echo');

    // Server-level disable wins — the tool is still hidden.
    expect(tm.loopTools.map((t) => t.name)).toEqual([]);
  });

  it('should persist tool-level disable after server is disabled and re-enabled', async () => {
    const tm = new ToolManager(fakeAgent());
    tm.setActiveTools(['mcp__*']);
    const client = fakeClient();
    tm.registerMcpServer('s', client, await discoverTools(client));

    // Disable one tool, then disable the whole server, then re-enable the server.
    tm.disableMcpTool('mcp__s__echo');
    tm.disableMcpServer('s');
    tm.enableMcpServer('s');

    // The tool-level disable persists: only noop is visible.
    expect(tm.loopTools.map((t) => t.name)).toEqual(['mcp__s__noop']);
  });

  // ── Runtime guard (second insurance) ────────────────────────────────────

  it('should return error result when calling resolveExecution on a disabled tool', async () => {
    const tm = new ToolManager(fakeAgent());
    tm.setActiveTools(['mcp__*']);
    const client = fakeClient();
    tm.registerMcpServer('s', client, await discoverTools(client));

    // Grab the tool BEFORE disabling (it disappears from loopTools after).
    const echo = tm.loopTools.find((t) => t.name === 'mcp__s__echo');
    expect(echo).toBeDefined();

    tm.disableMcpTool('mcp__s__echo');

    // resolveExecution should return an error result instead of executing.
    const resolved = echo!.resolveExecution({ text: 'hello' });
    // resolveExecution may return sync or promise
    const execution = 'then' in resolved ? await resolved : resolved;
    expect(execution.isError).toBe(true);
    if (execution.isError) {
      expect(execution.output).toContain('disabled');
    }
  });

  it('should return error result when calling resolveExecution on a tool from a disabled server', async () => {
    const tm = new ToolManager(fakeAgent());
    tm.setActiveTools(['mcp__*']);
    const client = fakeClient();
    tm.registerMcpServer('s', client, await discoverTools(client));

    // Grab the tool BEFORE disabling the server.
    const echo = tm.loopTools.find((t) => t.name === 'mcp__s__echo');
    expect(echo).toBeDefined();

    tm.disableMcpServer('s');

    const resolved = echo!.resolveExecution({ text: 'hello' });
    const execution = 'then' in resolved ? await resolved : resolved;
    expect(execution.isError).toBe(true);
    if (execution.isError) {
      expect(execution.output).toContain('disabled');
    }
  });

  it('should execute normally when a previously disabled tool is re-enabled', async () => {
    const tm = new ToolManager(fakeAgent());
    tm.setActiveTools(['mcp__*']);
    const client = fakeClient();
    tm.registerMcpServer('s', client, await discoverTools(client));

    // Grab the tool, disable it, then re-enable it.
    const echo = tm.loopTools.find((t) => t.name === 'mcp__s__echo');
    expect(echo).toBeDefined();

    tm.disableMcpTool('mcp__s__echo');
    tm.enableMcpTool('mcp__s__echo');

    // After re-enabling, execution should work normally.
    const result = await executeTool(echo!, {
      turnId: '1',
      toolCallId: 'tc-1',
      args: { text: 'hello world' },
      signal: new AbortController().signal,
    });
    expect(result.isError).toBe(false);
    expect(result.output).toBe('hello world');
  });

  // ── State persistence ───────────────────────────────────────────────────

  it('should persist disabled state across server re-registration', async () => {
    const tm = new ToolManager(fakeAgent());
    tm.setActiveTools(['mcp__*']);
    const firstClient = fakeClient();
    tm.registerMcpServer('s', firstClient, await discoverTools(firstClient));

    // Disable one tool.
    tm.disableMcpTool('mcp__s__echo');

    // Simulate a server reconnect: re-register with a new client.
    const secondClient = fakeClient();
    tm.registerMcpServer('s', secondClient, await discoverTools(secondClient));

    // The tool-level disable persists across re-registration.
    expect(tm.loopTools.map((t) => t.name)).toEqual(['mcp__s__noop']);
    expect(tm.isMcpToolDisabled('mcp__s__echo')).toBe(true);
  });

  // ── Query methods ───────────────────────────────────────────────────────

  it('should return correct values from getDisabledMcpServers', async () => {
    const tm = new ToolManager(fakeAgent());
    tm.setActiveTools(['mcp__*']);
    const client = fakeClient();
    const calcClient = fakeCalcClient();
    tm.registerMcpServer('a', client, await discoverTools(client));
    tm.registerMcpServer('b', calcClient, await discoverTools(calcClient));

    expect(tm.getDisabledMcpServers()).toEqual([]);

    tm.disableMcpServer('a');
    expect(tm.getDisabledMcpServers()).toEqual(['a']);

    tm.disableMcpServer('b');
    expect(tm.getDisabledMcpServers().toSorted()).toEqual(['a', 'b']);

    tm.enableMcpServer('a');
    expect(tm.getDisabledMcpServers()).toEqual(['b']);
  });

  it('should return correct values from getDisabledMcpTools', async () => {
    const tm = new ToolManager(fakeAgent());
    tm.setActiveTools(['mcp__*']);
    const client = fakeClient();
    tm.registerMcpServer('s', client, await discoverTools(client));

    expect(tm.getDisabledMcpTools()).toEqual([]);

    tm.disableMcpTool('mcp__s__echo');
    expect(tm.getDisabledMcpTools()).toEqual(['mcp__s__echo']);

    tm.disableMcpTool('mcp__s__noop');
    expect(tm.getDisabledMcpTools().toSorted()).toEqual(['mcp__s__echo', 'mcp__s__noop']);

    tm.enableMcpTool('mcp__s__echo');
    expect(tm.getDisabledMcpTools()).toEqual(['mcp__s__noop']);
  });

  it('should return correct boolean from isMcpServerDisabled', async () => {
    const tm = new ToolManager(fakeAgent());
    tm.setActiveTools(['mcp__*']);
    const client = fakeClient();
    tm.registerMcpServer('s', client, await discoverTools(client));

    expect(tm.isMcpServerDisabled('s')).toBe(false);

    tm.disableMcpServer('s');
    expect(tm.isMcpServerDisabled('s')).toBe(true);

    tm.enableMcpServer('s');
    expect(tm.isMcpServerDisabled('s')).toBe(false);
  });

  it('should return correct boolean from isMcpToolDisabled', async () => {
    const tm = new ToolManager(fakeAgent());
    tm.setActiveTools(['mcp__*']);
    const client = fakeClient();
    tm.registerMcpServer('s', client, await discoverTools(client));

    expect(tm.isMcpToolDisabled('mcp__s__echo')).toBe(false);
    expect(tm.isMcpToolDisabled('mcp__s__noop')).toBe(false);

    tm.disableMcpTool('mcp__s__echo');
    expect(tm.isMcpToolDisabled('mcp__s__echo')).toBe(true);
    expect(tm.isMcpToolDisabled('mcp__s__noop')).toBe(false);

    tm.enableMcpTool('mcp__s__echo');
    expect(tm.isMcpToolDisabled('mcp__s__echo')).toBe(false);
  });

  // ── Event emission ──────────────────────────────────────────────────────

  it('should emit tool.list.updated event when disabling a server', async () => {
    const events: unknown[] = [];
    const tm = new ToolManager(fakeAgent(events));
    tm.setActiveTools(['mcp__*']);
    const client = fakeClient();
    tm.registerMcpServer('s', client, await discoverTools(client));

    events.length = 0; // clear events from registration
    tm.disableMcpServer('s');

    expect(events).toContainEqual(
      expect.objectContaining({ type: 'tool.list.updated' }),
    );
  });

  it('should emit tool.list.updated event when enabling a server', async () => {
    const events: unknown[] = [];
    const tm = new ToolManager(fakeAgent(events));
    tm.setActiveTools(['mcp__*']);
    const client = fakeClient();
    tm.registerMcpServer('s', client, await discoverTools(client));

    tm.disableMcpServer('s');
    events.length = 0;
    tm.enableMcpServer('s');

    expect(events).toContainEqual(
      expect.objectContaining({ type: 'tool.list.updated' }),
    );
  });

  it('should emit tool.list.updated event when disabling a tool', async () => {
    const events: unknown[] = [];
    const tm = new ToolManager(fakeAgent(events));
    tm.setActiveTools(['mcp__*']);
    const client = fakeClient();
    tm.registerMcpServer('s', client, await discoverTools(client));

    events.length = 0;
    tm.disableMcpTool('mcp__s__echo');

    expect(events).toContainEqual(
      expect.objectContaining({ type: 'tool.list.updated' }),
    );
  });

  it('should emit tool.list.updated event when enabling a tool', async () => {
    const events: unknown[] = [];
    const tm = new ToolManager(fakeAgent(events));
    tm.setActiveTools(['mcp__*']);
    const client = fakeClient();
    tm.registerMcpServer('s', client, await discoverTools(client));

    tm.disableMcpTool('mcp__s__echo');
    events.length = 0;
    tm.enableMcpTool('mcp__s__echo');

    expect(events).toContainEqual(
      expect.objectContaining({ type: 'tool.list.updated' }),
    );
  });
});
