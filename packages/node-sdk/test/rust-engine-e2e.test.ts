import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import { describe, expect, it } from 'vitest';

import type {
  AgentConfigData,
  AgentContextData,
  AgentType,
  PermissionData,
  PlanData,
  SessionMeta,
  UsageStatus,
} from '@mirri-ai/agent-core';
import type {
  AgentReplayRecord,
  BackgroundTaskInfo,
  ResumedAgentState,
  ResumedSessionSummary,
  ToolInfo,
} from '#/types';
import type {
  MirriEngineNative,
  MirriNodeBinding,
  MockLlmResponseSpec,
} from '#/sdk-rpc-client-rust';
import { writeRustEngineModelConfig } from './rust-engine-model-config';

// Load the native binding directly — this test bypasses the harness to
// exercise the raw NAPI → EngineBridge → EmbeddedServer → MockLlm path.
// The .node binary must be built with `--features mock-llm`:
//   cd rust && cargo build --features mock-llm -p mirri-node
//   cp target/debug/libmirri_node.dylib ../packages/mirri-node/mirri-node.darwin-arm64.node
//   cp target/debug/libmirri_node.dylib ../packages/mirri-node/mirri-node.node
const binding = require('@mirri-ai/mirri-node') as MirriNodeBinding;

const hasMockLlm = typeof binding.createEngineWithMockLlm === 'function';
const hasScriptedMock =
  typeof binding.createEngineWithScriptedMockLlm === 'function';

type EngineEvent = { method: string; params: unknown };

/** Read one field off an engine event's flat params object. */
function eventField(event: EngineEvent, key: string): unknown {
  return (event.params as Record<string, unknown> | undefined)?.[key];
}

/** Accumulate text from all event.assistant.delta notifications seen so far. */
function accumulateChunks(events: EngineEvent[]): string {
  return events
    .filter(e => e.method === 'event.assistant.delta')
    .map(e => (eventField(e, 'delta') as string) ?? '')
    .join('');
}

/**
 * Subscribe to engine events and resolve once the accumulated assistant text
 * equals `expectedText`, or after a debounce timeout with whatever was received.
 *
 * napi-rs ThreadsafeFunction uses CalleeHandled error strategy: the JS
 * callback receives `(err, value)` — err is null on success, value is the
 * event object (already a JS object, not a JSON string).
 */
function subscribeForReply(
  engine: MirriEngineNative,
  expectedText: string,
  timeoutMs = 15_000,
): Promise<EngineEvent[]> {
  const collected: EngineEvent[] = [];
  let resolveDone: (events: EngineEvent[]) => void;
  let rejectDone: (error: Error) => void;
  const donePromise = new Promise<EngineEvent[]>((resolve, reject) => {
    resolveDone = resolve;
    rejectDone = reject;
  });
  let debounceTimer: NodeJS.Timeout | undefined;
  const timeout = setTimeout(() => {
    if (debounceTimer) clearTimeout(debounceTimer);
    rejectDone(
      new Error(
        `timed out; received ${collected.length} events, accumulated text: "${accumulateChunks(collected)}"`,
      ),
    );
  }, timeoutMs);

  engine.onEvent((_err, event) => {
    collected.push(event);
    if (debounceTimer) clearTimeout(debounceTimer);
    // If accumulated text matches, resolve immediately
    if (accumulateChunks(collected) === expectedText) {
      clearTimeout(timeout);
      resolveDone(collected);
      return;
    }
    // Otherwise set a debounce in case more chunks come
    debounceTimer = setTimeout(() => {
      clearTimeout(timeout);
      resolveDone(collected);
    }, 2000);
  });

  return donePromise;
}

describe.runIf(hasMockLlm)('Rust engine E2E with mock LLM', () => {
  it('should stream assistant reply when message is sent in fire mode', async () => {
    const replyText = 'Hello from the mock LLM!';
    const engine = binding.createEngineWithMockLlm!([replyText]);

    try {
      // Subscribe BEFORE sending to avoid missing early events
      const donePromise = subscribeForReply(engine, replyText);

      // Create a session
      const created = (await engine.callMethod('session.create', {
        title: 'E2E test',
        modelId: 'contract-e2e-model',
      })) as { id: string };
      expect(created.id).toMatch(/^session-/);

      // Send a fire-and-forget message
      const sent = (await engine.callMethod('message.send', {
        session_id: created.id,
        text: 'Hi there',
        mode: 'fire',
      })) as { ok: boolean; session_id: string; turn_id: string };

      expect(sent.ok).toBe(true);
      expect(sent.session_id).toBe(created.id);
      expect(sent.turn_id).toMatch(/^turn-/);

      // Wait for all chunks to arrive
      const events = await donePromise;

      // Verify event.message.received arrived before the first
      // event.assistant.delta
      const firstChunkIndex = events.findIndex(
        e => e.method === 'event.assistant.delta',
      );
      const messageReceivedIndex = events.findIndex(
        e => e.method === 'event.message.received',
      );
      expect(messageReceivedIndex).toBeGreaterThanOrEqual(0);
      expect(messageReceivedIndex).toBeLessThan(firstChunkIndex);

      // Verify the accumulated assistant text matches the mock reply
      expect(accumulateChunks(events)).toBe(replyText);
    } finally {
      await engine.dispose();
    }
  });

  it('should return turn result when message is sent in wait mode', async () => {
    const engine = binding.createEngineWithMockLlm!(['Wait mode reply']);

    try {
      const created = (await engine.callMethod('session.create', {
        modelId: 'contract-e2e-model',
      })) as {
        id: string;
      };

      const result = (await engine.callMethod('message.send', {
        session_id: created.id,
        text: 'Blocking ask',
        mode: 'wait',
      })) as { turn: { stopReason: string }; agent_id: string };

      expect(result.turn.stopReason).toBe('end_turn');
      expect(result.agent_id).toBe('main');
    } finally {
      await engine.dispose();
    }
  });

  it('should reject when calling unknown method', async () => {
    const engine = binding.createEngineWithMockLlm!(['dummy']);

    try {
      await expect(
        engine.callMethod('no.such.method', {}),
      ).rejects.toThrow();
    } finally {
      await engine.dispose();
    }
  });

  it('should deliver multiple chunk events when mock LLM produces multi-character response', async () => {
    const replyText = 'ABCDEFGH';
    const engine = binding.createEngineWithMockLlm!([replyText]);

    try {
      const donePromise = subscribeForReply(engine, replyText);

      const created = (await engine.callMethod('session.create', {
        modelId: 'contract-e2e-model',
      })) as {
        id: string;
      };
      await engine.callMethod('message.send', {
        session_id: created.id,
        text: 'count chunks',
        mode: 'fire',
      });

      const events = await donePromise;

      const chunkCount = events.filter(
        e => e.method === 'event.assistant.delta',
      ).length;

      // The mock LLM streams one character at a time, so we expect exactly
      // replyText.length chunk events
      expect(chunkCount).toBe(replyText.length);

      // And the accumulated text should match
      expect(accumulateChunks(events)).toBe(replyText);
    } finally {
      await engine.dispose();
    }
  });
});

describe.runIf(hasScriptedMock)('Rust engine E2E with scripted mock LLM — subagent', () => {
  it('should spawn a subagent and return its result when Agent tool is invoked', async () => {
    // Scripted flow (shared FIFO queue):
    // 1. Main agent calls Agent tool → tool_use response
    // 2. Subagent gets "Subagent task complete" → end_turn
    // 3. Main agent gets "Main final reply after subagent" → end_turn
    const responses: MockLlmResponseSpec[] = [
      {
        text: '',
        toolCalls: [
          {
            name: 'Agent',
            args: {
              prompt: 'Do the subtask',
              description: 'test subtask',
              subagent_type: 'coder',
            },
          },
        ],
      },
      {
        text: 'Subagent task complete',
        toolCalls: [],
      },
      {
        text: 'Main final reply after subagent',
        toolCalls: [],
      },
    ];

    const engine = binding.createEngineWithScriptedMockLlm!(responses);

    try {
      // Collect events until the main agent's turn settles (the engine's
      // post-tool main replies land in the session record without
      // assistant.delta streaming — the settle, not the deltas, is the
      // completion signal).
      const collected: EngineEvent[] = [];
      const donePromise = new Promise<EngineEvent[]>(resolve => {
        const deadline = setTimeout(() => resolve(collected), 30_000);
        engine.onEvent((_err, event) => {
          collected.push(event);
          if (
            event.method === 'event.turn.ended' &&
            (event as { params?: { agentId?: string } }).params?.agentId === 'main'
          ) {
            clearTimeout(deadline);
            resolve(collected);
          }
        });
      });

      const created = (await engine.callMethod('session.create', {
        permissionMode: 'yolo',
        modelId: 'contract-e2e-model',
      })) as { id: string };

      // Send message in fire mode to trigger the agent loop
      await engine.callMethod('message.send', {
        session_id: created.id,
        text: 'Please use the Agent tool to do a subtask',
        mode: 'fire',
      });

      const events = await donePromise;

      // Verify the subagent's reply streamed over the deltas
      const fullReply = accumulateChunks(events);
      expect(fullReply).toContain('Subagent task complete');

      // Verify the main agent's final reply landed in the session record
      const snapshot = (await engine.callMethod('session.get', {
        session_id: created.id,
      })) as unknown;
      expect(JSON.stringify(snapshot)).toContain('Main final reply after subagent');

      // Verify the subagent tool round-trip emitted the projected tool
      // lifecycle notifications (the Agent tool call the main agent made)
      const hasToolLifecycle = events.some(
        e =>
          e.method === 'event.tool.call.started' ||
          e.method === 'event.tool.result',
      );
      expect(hasToolLifecycle, 'the Agent tool call must emit event.tool.* notifications').toBe(true);
    } finally {
      await engine.dispose();
    }
  });

  it('should list Agent and AgentSwarm tools when profile manager is configured', async () => {
    const engine = binding.createEngineWithMockLlm!(['dummy']);

    try {
      const created = (await engine.callMethod('session.create', {
        modelId: 'contract-e2e-model',
      })) as {
        id: string;
      };

      // Tools are registered but not active until setActiveTools is called.
      // The embedded engine starts with an empty active list; real profiles
      // set enabled_tools, but the mock-llm engine has no profile for the
      // main agent, so we activate tools explicitly.
      await engine.callMethod('agent.setActiveTools', {
        session_id: created.id,
        agent_id: 'main',
        names: ['Agent', 'AgentSwarm'],
      });

      const tools = (await engine.callMethod('tool.list', {
        session_id: created.id,
        agent_id: 'main',
      })) as Array<{ name: string; description: string }>;

      const toolNames = tools.map(t => t.name);
      expect(toolNames).toContain('Agent');
      expect(toolNames).toContain('AgentSwarm');

      // C5: descriptions should include available profiles since the
      // profile manager has the "coder" profile registered.
      const agentTool = tools.find(t => t.name === 'Agent');
      expect(agentTool).toBeDefined();
      expect(agentTool!.description).toContain('coder');
      expect(agentTool!.description).toContain('Available profiles');

      const swarmTool = tools.find(t => t.name === 'AgentSwarm');
      expect(swarmTool).toBeDefined();
      expect(swarmTool!.description).toContain('coder');
      expect(swarmTool!.description).toContain('Available profiles');
    } finally {
      await engine.dispose();
    }
  });

  it('should dispatch multiple subagents concurrently when AgentSwarm tool is invoked', async () => {
    // Scripted flow (shared FIFO queue):
    // 1. Main agent calls AgentSwarm with items ["alpha", "beta"]
    // 2. Subagent 1 gets "Result alpha" → end_turn
    // 3. Subagent 2 gets "Result beta" → end_turn
    // 4. Main agent gets "Swarm complete" → end_turn
    const responses: MockLlmResponseSpec[] = [
      {
        text: '',
        toolCalls: [
          {
            name: 'AgentSwarm',
            args: {
              description: 'swarm test',
              prompt_template: 'Process: {{item}}',
              items: ['alpha', 'beta'],
            },
          },
        ],
      },
      {
        text: 'Result alpha',
        toolCalls: [],
      },
      {
        text: 'Result beta',
        toolCalls: [],
      },
      {
        text: 'Swarm complete',
        toolCalls: [],
      },
    ];

    const engine = binding.createEngineWithScriptedMockLlm!(responses);

    try {
      // Collect all events until the main agent's turn settles (the
      // engine's post-tool main replies land in the session record
      // without assistant.delta streaming — the settle, not the deltas, is
      // the completion signal).
      const collected: EngineEvent[] = [];
      const donePromise = new Promise<EngineEvent[]>(resolve => {
        const deadline = setTimeout(() => resolve(collected), 30_000);
        engine.onEvent((_err, event) => {
          collected.push(event);
          if (
            event.method === 'event.turn.ended' &&
            (event as { params?: { agentId?: string } }).params?.agentId === 'main'
          ) {
            clearTimeout(deadline);
            resolve(collected);
          }
        });
      });

      const created = (await engine.callMethod('session.create', {
        permissionMode: 'yolo',
        modelId: 'contract-e2e-model',
      })) as { id: string };

      await engine.callMethod('message.send', {
        session_id: created.id,
        text: 'Use AgentSwarm to process alpha and beta',
        mode: 'fire',
      });

      const events = await donePromise;

      // Verify the main agent produced its final reply after the swarm
      // completed: the settle lands it in the session record (the
      // subagent texts "Result alpha" / "Result beta" are returned inside
      // the AgentSwarm tool's XML result, not as assistant deltas).
      const snapshot = (await engine.callMethod('session.get', {
        session_id: created.id,
      })) as unknown;
      expect(JSON.stringify(snapshot)).toContain('Swarm complete');
      void events;
    } finally {
      await engine.dispose();
    }
  });
});

describe('Rust engine E2E with explicit skill dirs (SDK skillDirs parity)', () => {
  it('discovers the custom skills and activates one when the engine is created with skillDirs', async () => {
    // Given an isolated home and a custom skill directory
    const home = await mkdtemp(join(tmpdir(), 'mirri-skilldirs-home-'));
    await writeRustEngineModelConfig(home, 'contract-e2e-model');
    const skillsRoot = await mkdtemp(join(tmpdir(), 'mirri-skilldirs-root-'));
    const skillDir = join(skillsRoot, 'code-review');
    await mkdir(skillDir, { recursive: true });
    await writeFile(
      join(skillDir, 'SKILL.md'),
      '---\ndescription: Reviews code carefully.\n---\n\n# Code Review\n\nLook for bugs.',
    );

    // When the engine is created with the explicit skill dirs (the SDK's
    // `skillDirs` path: non-empty dirs replace the default user/project
    // discovery and serve as the user skill source)
    const engine = binding.createEngine(home, [skillsRoot]);
    try {
      const listed = (await engine.callMethod('skill.list', {})) as Array<{
        name: string;
        description: string;
        source: string;
      }>;
      const ours = listed.find((skill) => skill.name === 'code-review');
      expect(ours).toBeDefined();
      expect(ours?.source).toBe('user');
      expect(ours?.description).toBe('Reviews code carefully.');

      // And the skill activates on a live session (content injection needs
      // no LLM — the production engine path, not a mock factory)
      const created = (await engine.callMethod('session.create', {
        title: 'skill dirs e2e',
        modelId: 'contract-e2e-model',
      })) as { id: string };
      const activated = (await engine.callMethod('agent.activateSkill', {
        session_id: created.id,
        name: 'code-review',
      })) as { ok: boolean; skill: string };
      expect(activated.ok).toBe(true);
      expect(activated.skill).toBe('code-review');
    } finally {
      await engine.dispose();
      await rm(home, { recursive: true, force: true });
      await rm(skillsRoot, { recursive: true, force: true });
    }
  });
});

describe.runIf(hasScriptedMock)('Rust engine E2E resume shape (v1 ResumedSessionSummary)', () => {
  it('serves the v1 resumed summary with the replayed conversation when a closed session is resumed', async () => {
    // Given an isolated home and a file the scripted tool call will read
    const home = await mkdtemp(join(tmpdir(), 'mirri-resume-home-'));
    const readPath = join(home, 'notes.txt');
    await writeFile(readPath, 'the answer is 42');

    // And an engine whose first response makes a real tool call and whose
    // second ends the turn
    const responses: MockLlmResponseSpec[] = [
      {
        text: '',
        toolCalls: [{ name: 'Read', args: { path: readPath } }],
      },
      { text: 'All done after the tool', toolCalls: [] },
    ];
    const engine = binding.createEngineWithScriptedMockLlm!(responses, home);
    try {
      // When a session runs the tool round-trip and is closed and resumed
      const created = (await engine.callMethod('session.create', {
        title: 'resumable e2e',
        permissionMode: 'yolo',
        modelId: 'contract-e2e-model',
      })) as { id: string };
      const sent = (await engine.callMethod('message.send', {
        session_id: created.id,
        text: 'read the notes file',
        mode: 'wait',
      })) as { turn: { stopReason: string } };
      expect(sent.turn.stopReason).toBe('end_turn');
      await engine.callMethod('session.close', { session_id: created.id });
      const resumedRaw = (await engine.callMethod('session.resume', {
        session_id: created.id,
      })) as unknown;

      // Then the body is the v1 ResumedSessionSummary (compile-time
      // alignment: the mapping below only typechecks against the v1
      // SDK types the TUI's session-replay consumes)
      const summary = resumedSummaryFromWire(resumedRaw);
      expect(summary.id).toBe(created.id);
      expect(summary.title).toBe('resumable e2e');
      expect(summary.archived).toBe(false);
      expect(typeof summary.workDir).toBe('string');
      expect(typeof summary.sessionDir).toBe('string');
      expect(typeof summary.createdAt).toBe('number');
      expect(typeof summary.updatedAt).toBe('number');
      expect(summary.sessionMetadata.title).toBe('resumable e2e');
      expect(summary.sessionMetadata.agents['main']?.type).toBe('main');

      const main = summary.agents['main'];
      expect(main).toBeDefined();
      expect(main?.type).toBe('main');
      expect(main?.config.cwd).toBe(summary.workDir);
      expect(typeof main?.config.modelAlias).toBe('string');
      expect(typeof main?.config.thinkingEffort).toBe('string');
      // The replay rebuilt the whole conversation: the user message, the
      // assistant's tool call, the tool result, and the final assistant
      // reply — one `message` record per context entry, in order (the
      // `tools_added` first-activation reminder no longer occupies a user
      // context slot in the current engine — the activation injects the
      // tool listing into the turn context without a replayed record)
      const replay = main?.replay ?? [];
      const messageRecords = replay.filter(r => r.type === 'message');
      expect(messageRecords).toHaveLength(4);
      const roles = messageRecords.map(r => r.message.role);
      expect(roles).toEqual(['user', 'assistant', 'tool', 'assistant']);
      expect(messageRecords[1]?.message.toolCalls?.[0]?.name).toBe('Read');
      expect(messageRecords[2]?.message.toolCallId).toBe(
        messageRecords[1]?.message.toolCalls?.[0]?.id,
      );
      const finalText = (messageRecords[3]?.message.content ?? [])
        .map(part => (part.type === 'text' ? part.text : ''))
        .join('');
      expect(finalText).toBe('All done after the tool');
      // The context snapshot mirrors the same history
      expect(main?.context.history).toHaveLength(4);
      expect(main?.context.tokenCount).toBeGreaterThan(0);
      // The snapshot state the TUI hydrates from
      expect(main?.permission.mode).toBe('yolo');
      expect(main?.plan).toBeNull();
      expect(main?.swarmMode).toBe(false);
      expect(main?.usage.total).toBeDefined();
      expect(Array.isArray(main?.tools)).toBe(true);
      expect(Array.isArray(main?.background)).toBe(true);
    } finally {
      await engine.dispose();
      await rm(home, { recursive: true, force: true });
    }
  });
});

/**
 * Map the rust engine's `session.resume` wire body onto the v1 SDK
 * `ResumedSessionSummary` type. The explicit field walk is the
 * compile-time alignment: any drift between the engine's wire and the v1
 * contract the TUI consumes fails this mapping's typecheck, not a
 * silent runtime shape hole.
 */
function resumedSummaryFromWire(wire: unknown): ResumedSessionSummary {
  if (typeof wire !== 'object' || wire === null) {
    throw new Error('session.resume must return an object');
  }
  const w = wire as Record<string, unknown>;
  const agentsWire = (w['agents'] ?? {}) as Record<string, unknown>;
  const mainWire = agentsWire['main'] as Record<string, unknown> | undefined;
  const main: ResumedAgentState | undefined = mainWire === undefined
    ? undefined
    : {
        type: mainWire['type'] as AgentType,
        config: mainWire['config'] as AgentConfigData,
        context: mainWire['context'] as AgentContextData,
        replay: (mainWire['replay'] ?? []) as readonly AgentReplayRecord[],
        permission: mainWire['permission'] as PermissionData,
        plan: mainWire['plan'] as PlanData,
        swarmMode: mainWire['swarmMode'] as boolean | undefined,
        usage: mainWire['usage'] as UsageStatus,
        tools: (mainWire['tools'] ?? []) as readonly ToolInfo[],
        background: (mainWire['background'] ?? []) as readonly BackgroundTaskInfo[],
      };
  return {
    id: w['id'] as string,
    title: w['title'] as string | undefined,
    workDir: w['workDir'] as string,
    sessionDir: w['sessionDir'] as string,
    createdAt: w['createdAt'] as number,
    updatedAt: w['updatedAt'] as number,
    archived: w['archived'] as boolean | undefined,
    sessionMetadata: w['sessionMetadata'] as SessionMeta,
    agents: { main } as ResumedSessionSummary['agents'],
    warning: w['warning'] as string | undefined,
  };
}
