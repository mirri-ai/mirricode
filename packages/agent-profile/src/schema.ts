/**
 * Agent profile file schema — the unified type definition shared by
 * `agent-core` (v1) and `agent-core-v2` (v2).
 *
 * A profile file is a `.md` document (YAML frontmatter + Markdown body) or
 * a legacy `.yaml`/`.yml` file. The frontmatter holds structured metadata
 * (name, tools, model, …); the Markdown body is the system-prompt template.
 * This interface is pure data — runtime prompt rendering
 * (`systemPrompt(context)`) is each engine's own adaptation layer.
 */

/** Where an agent file was discovered from. */
export type AgentFileSource = 'plugin' | 'project' | 'user' | 'extra' | 'explicit';

/**
 * Accepted agent-file extensions, ordered by priority for same-name conflict
 * resolution. `.md` wins; `.yaml` is next; `.yml` is lowest.
 */
export const AGENT_FILE_EXTENSIONS = ['.md', '.yaml', '.yml'] as const;

/** Kebab-case validation pattern for agent profile names. */
export const AGENT_NAME_PATTERN = /^[a-z0-9]+(?:-[a-z0-9]+)*$/;

/** A root directory to scan for agent files. */
export interface AgentFileRoot {
  readonly path: string;
  readonly source: AgentFileSource;
}

/** A file that was skipped during discovery due to a parse error. */
export interface SkippedAgentFile {
  readonly path: string;
  readonly reason: string;
}

/** The parsed definition of a single agent profile file. */
export interface AgentFileDef {
  /** Required, kebab-case identifier, e.g. "code-reviewer". */
  readonly name: string;
  /** Required, one-line description for UI display and LLM selection. */
  readonly description: string;
  /** Optional, LLM selection hint — when to use this profile. */
  readonly whenToUse?: string;
  /** Optional, default model alias, e.g. "claude-sonnet". */
  readonly defaultModel?: string;
  /** Optional, tool allowlist (omitted = all tools available). */
  readonly tools?: readonly string[];
  /** Optional, subagent profile names this agent may dispatch (omitted = all). */
  readonly subagents?: readonly string[];
  /** Optional, capability tags, e.g. ["code.explore", "code.read"]. */
  readonly capabilitiesRequired?: readonly string[];
  /** System prompt template body — the Markdown body of a `.md` file. */
  readonly prompt: string;
  /** File path the definition was parsed from (only set during discovery). */
  readonly filePath?: string;
  /** Discovery source of the file. */
  readonly source?: AgentFileSource;
}

/** Result of a discovery pass over one or more roots. */
export interface AgentFileDiscoveryResult {
  readonly agents: readonly AgentFileDef[];
  readonly skipped: readonly SkippedAgentFile[];
  readonly scannedRoots: readonly string[];
}
