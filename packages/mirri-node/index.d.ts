/**
 * Native Rust engine binding for Mirri Code (NAPI cdylib).
 *
 * This module is loaded by `SDKRpcClientRust` (packages/node-sdk) when
 * `MIRRICODE_RUST_ENGINE=1` is set. It embeds the Rust engine directly
 * in the Node.js process — no network, no IPC.
 */

export = MirriNode;

declare namespace MirriNode {
  export interface MirriEngineNative {
    /**
     * Forward one RPC method call; resolves with the engine's JSON result.
     * Rejections carry the engine's structured error as a suffix on the
     * message — `… [mirri-rpc-error {"code":-32603,"domainCode":"…"}]` —
     * decodable with the SDK's `decodeRustEngineError`.
     */
    callMethod(method: string, params: unknown): Promise<unknown>;
    /**
     * Subscribe to engine events. The callback uses Node.js error-first
     * convention: `(err, event)` — `err` is always `null` (events never
     * error), `event` is a `{ method, params }` object. Supports one
     * subscription per engine.
     */
    onEvent(
      callback: (
        err: null,
        event: { method: string; params: unknown },
      ) => void,
    ): void;
    /**
     * Register the reverse-RPC handler. Same error-first convention:
     * `(err, request)` — `err` is always `null`, `request` is
     * `{ id, method, params }`. Answer with `respondReverseRpc(id, result)`
     * or `respondReverseRpcError(id, error)`.
     */
    onReverseRpc(
      callback: (
        err: null,
        request: { id: number; method: string; params: unknown },
      ) => void,
    ): void;
    /** Answer a pending reverse RPC request with a successful result. */
    respondReverseRpc(id: number, result: unknown): Promise<void>;
    /** Answer a pending reverse RPC request with an error message. */
    respondReverseRpcError(id: number, error: string): Promise<void>;
    /** Release the engine, cancelling all in-flight work. Idempotent. */
    dispose(): Promise<void>;
  }

  /** A tool call in a scripted mock LLM response. */
  export interface MockLlmToolCallSpec {
    /** Tool name, e.g. "Agent", "Read", "Bash". */
    name: string;
    /** Tool call ID. Auto-generated when omitted. */
    id?: string;
    /** Tool arguments as a JSON object. */
    args: Record<string, unknown>;
  }

  /** A single scripted mock LLM response. */
  export interface MockLlmResponseSpec {
    /** Plain text reply. When set (and `toolCalls` is empty), this is an
     *  end-turn response with stop_reason "completed". */
    text?: string;
    /** Tool calls to emit. When non-empty, stop_reason is "tool_calls" and
     *  the agent loop will execute the requested tools. */
    toolCalls: MockLlmToolCallSpec[];
  }

  export interface MirriNodeBinding {
    /**
     * Create an engine rooted at the current working directory. `homeDir`
     * optionally injects an explicit Mirri home — the directory holding
     * `config.toml`, `profiles/`, `plugins/`, MCP config, and the session
     * persistence root — instead of the env-based resolution
     * (`MIRRICODE_HOME`, then `$HOME/.mirri-code`); omitted keeps the ambient
     * behavior. `skillDirs` optionally carries explicit skill directories
     * (v1/v2 `skillDirs` parity): when non-empty they replace the engine's
     * default user/project skill discovery and serve as the user skill
     * source; omitted/empty keeps the ambient discovery.
     */
    createEngine(homeDir?: string, skillDirs?: string[]): MirriEngineNative;
    /**
     * Create an engine backed by a scripted mock LLM (only available when the
     * native binary is built with the `mock-llm` feature). Each string in
     * `responses` is queued as a plain-text end-turn reply, replayed in FIFO
     * order — one per `chat()` call. A "coder" profile is registered so the
     * Agent/AgentSwarm tools are available.
     */
    createEngineWithMockLlm?(responses: string[]): MirriEngineNative;
    /**
     * Create an engine with a scripted mock LLM that supports tool calls.
     * Each response can include tool calls, enabling E2E tests for subagent
     * spawning, tool execution, and multi-turn agent loops. Responses are
     * replayed in FIFO order across all agents (main + subagents share one
     * queue).
     */
    createEngineWithScriptedMockLlm?(
      responses: MockLlmResponseSpec[],
    ): MirriEngineNative;
  }
}

declare const MirriNode: MirriNode.MirriNodeBinding;
