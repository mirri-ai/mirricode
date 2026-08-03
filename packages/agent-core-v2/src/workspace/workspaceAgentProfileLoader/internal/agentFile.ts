/**
 * `workspaceAgentProfileLoader` domain — agent-file parsing primitives.
 *
 * Parses a single agent Markdown file (frontmatter + body) into an
 * `AgentFileDefinition`. Pure functions with no IO: callers read bytes however
 * they like and pass the decoded text in. Unknown frontmatter fields are
 * ignored so later format extensions stay forward-compatible. Compatibility conventions match other agent CLIs: a
 * missing `name` falls back to the file name (OpenCode), a lone `*` in
 * `tools` / `subagents` means unrestricted like an omitted field, and list
 * fields accept either a bare comma-separated string or the YAML list form
 * (Claude Code).
 */

import { load as loadYaml, dump as dumpYaml } from 'js-yaml';
import { FrontmatterError, parseFrontmatter, serializeFrontmatter } from '#/_base/text/frontmatter';

import type { AgentFileDefinition, AgentFileSource } from './types';

export class AgentFileParseError extends Error {
  readonly reason?: unknown;

  constructor(message: string, cause?: unknown) {
    super(message);
    this.name = 'AgentFileParseError';
    if (cause !== undefined) this.reason = cause;
  }
}

export interface ParseAgentFileOptions {
  readonly path: string;
  readonly source: AgentFileSource;
  readonly text: string;
}

const AGENT_NAME_PATTERN = /^[a-z0-9]+(?:-[a-z0-9]+)*$/;

const YAML_EXTENSIONS = ['.yml', '.yaml'] as const;

function isYamlFile(path: string): boolean {
  return YAML_EXTENSIONS.some((ext) => path.endsWith(ext));
}

/**
 * Parse a pure-YAML agent file (`.yml`/`.yaml`). The entire file content is
 * treated as the YAML data map — there is no separate Markdown body. The
 * system prompt defaults to a minimal instruction so the profile is usable
 * even without an explicit `prompt`/body field; users who want a custom
 * system prompt should use the `.md` format (frontmatter + body).
 */
function parseYamlAgentFile(options: ParseAgentFileOptions): { data: unknown; body: string } {
  try {
    const data = loadYaml(options.text) ?? {};
    return { data, body: '' };
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    throw new AgentFileParseError(`Invalid YAML in ${options.path}: ${message}`, error);
  }
}

export function parseAgentFileText(options: ParseAgentFileOptions): AgentFileDefinition {
  let parsed: { data: unknown; body: string };
  if (isYamlFile(options.path)) {
    parsed = parseYamlAgentFile(options);
  } else {
    try {
      parsed = parseFrontmatter(options.text);
    } catch (error) {
      if (error instanceof FrontmatterError) {
        throw new AgentFileParseError(
          `Invalid frontmatter in ${options.path}: ${error.message}`,
          error,
        );
      }
      throw error;
    }
  }

  const frontmatter = parsed.data;
  if (frontmatter === null) {
    throw new AgentFileParseError(`Missing frontmatter in ${options.path}`);
  }
  if (!isRecord(frontmatter)) {
    throw new AgentFileParseError(
      `Frontmatter in ${options.path} must be a mapping at the top level`,
    );
  }

  const nameField = frontmatter['name'];
  if (nameField !== undefined && nameField !== null && typeof nameField !== 'string') {
    throw new AgentFileParseError(
      `Frontmatter field "name" in ${options.path} must be a non-empty string`,
    );
  }
  const name = nonEmptyString(nameField) ?? deriveNameFromPath(options.path);
  if (name === undefined) {
    throw new AgentFileParseError(`Missing required frontmatter field "name" in ${options.path}`);
  }
  if (!AGENT_NAME_PATTERN.test(name)) {
    throw new AgentFileParseError(
      `Invalid agent name "${name}" in ${options.path}: expected kebab-case (e.g. "code-reviewer")`,
    );
  }

  const description = requiredNonEmptyString(
    frontmatter['description'],
    'description',
    options.path,
  );

  const override = parseBoolean(frontmatter['override'], 'override', options.path);
  const rawTools = parseStringList(frontmatter['tools'], 'tools', options.path);
  const tools = rawTools?.length === 1 && rawTools[0] === '*' ? undefined : rawTools;
  const disallowedTools = parseStringList(
    frontmatter['disallowedTools'],
    'disallowedTools',
    options.path,
  );
  const rawSubagents = parseStringList(frontmatter['subagents'], 'subagents', options.path);
  const subagents =
    rawSubagents?.length === 1 && rawSubagents[0] === '*' ? undefined : rawSubagents;
  const modelPreference = parseModelPreference(frontmatter['model_preference'], options.path);
  const defaultModel = nonEmptyString(frontmatter['defaultModel'] ?? frontmatter['default_model']);
  const capabilitiesRequired = parseStringList(
    frontmatter['capabilitiesRequired'] ?? frontmatter['capabilities_required'],
    'capabilitiesRequired',
    options.path,
  );
  const extendsProfile = nonEmptyString(frontmatter['extends']);
  const promptVars = parsePromptVars(frontmatter['promptVars'] ?? frontmatter['prompt_vars'], options.path);

  const prompt = parsed.body.trim();
  if (prompt.length === 0 && !isYamlFile(options.path)) {
    throw new AgentFileParseError(`Missing prompt body in ${options.path}`);
  }
  // For pure-YAML files with no body, use a default prompt so the profile
  // is still usable. Users wanting a custom system prompt should use .md.
  const effectivePrompt = prompt.length > 0 ? prompt : 'You are a helpful assistant.';

  return {
    name,
    description,
    whenToUse: nonEmptyString(frontmatter['whenToUse']),
    override,
    tools,
    disallowedTools,
    subagents,
    modelPreference,
    defaultModel,
    capabilitiesRequired,
    extends: extendsProfile,
    promptVars,
    prompt: effectivePrompt,
    path: options.path,
    source: options.source,
  };
}

function parseModelPreference(
  value: unknown,
  filePath: string,
): AgentFileDefinition['modelPreference'] {
  if (value === undefined || value === null) return undefined;
  if (value === 'primary' || value === 'secondary') return value;
  throw new AgentFileParseError(
    `Frontmatter field "model_preference" in ${filePath} must be "primary" or "secondary"`,
  );
}

function parseBoolean(value: unknown, field: string, filePath: string): boolean {
  if (value === undefined || value === null) return false;
  if (typeof value === 'boolean') return value;
  throw new AgentFileParseError(
    `Frontmatter field "${field}" in ${filePath} must be a boolean`,
  );
}

/**
 * Parse the `promptVars` frontmatter field into a `Record<string, string>`.
 * Accepts a YAML map of string→string; non-string values are rejected so a
 * typo (e.g. `roleAdditional: 42`) surfaces as a parse error rather than
 * silently coercing. Returns `undefined` when the field is absent or empty.
 */
function parsePromptVars(
  value: unknown,
  filePath: string,
): Record<string, string> | undefined {
  if (value === undefined || value === null) return undefined;
  if (typeof value !== 'object' || Array.isArray(value)) {
    throw new AgentFileParseError(
      `Frontmatter field "promptVars" in ${filePath} must be a map of string to string`,
    );
  }
  const out: Record<string, string> = {};
  for (const [key, raw] of Object.entries(value as Record<string, unknown>)) {
    if (raw === undefined || raw === null) continue;
    if (typeof raw !== 'string') {
      throw new AgentFileParseError(
        `Frontmatter field "promptVars.${key}" in ${filePath} must be a string`,
      );
    }
    out[key] = raw;
  }
  return Object.keys(out).length === 0 ? undefined : out;
}

function parseStringList(
  value: unknown,
  field: string,
  filePath: string,
): readonly string[] | undefined {
  if (value === undefined || value === null) return undefined;
  if (typeof value === 'string') {
    return value
      .split(',')
      .map((item) => item.trim())
      .filter((item) => item !== '');
  }
  if (!Array.isArray(value)) {
    throw new AgentFileParseError(
      `Frontmatter field "${field}" in ${filePath} must be a comma-separated string or a list of strings`,
    );
  }
  const out: string[] = [];
  for (const item of value) {
    if (typeof item !== 'string' || item.trim() === '') {
      throw new AgentFileParseError(
        `Frontmatter field "${field}" in ${filePath} must be a list of non-empty strings`,
      );
    }
    out.push(item.trim());
  }
  return out;
}

function requiredNonEmptyString(value: unknown, field: string, filePath: string): string {
  if (value !== undefined && value !== null && typeof value !== 'string') {
    throw new AgentFileParseError(
      `Frontmatter field "${field}" in ${filePath} must be a non-empty string`,
    );
  }
  const parsed = nonEmptyString(value);
  if (parsed === undefined) {
    throw new AgentFileParseError(`Missing required frontmatter field "${field}" in ${filePath}`);
  }
  return parsed;
}

function deriveNameFromPath(filePath: string): string | undefined {
  const base = filePath.split(/[\\/]/).pop() ?? '';
  const name = base.replace(/\.[^.]*$/, '');
  return name !== '' ? name : undefined;
}

function nonEmptyString(value: unknown): string | undefined {
  return typeof value === 'string' && value.trim() !== '' ? value.trim() : undefined;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

const YAML_AGENT_EXTENSIONS = ['.yml', '.yaml'] as const;

function isYamlAgentFile(path: string): boolean {
  return YAML_AGENT_EXTENSIONS.some((ext) => path.endsWith(ext));
}

/**
 * Serialize an {@link AgentFileDefinition} back into file text. The format is
 * determined by the `path` extension: `.md` produces frontmatter + body
 * (Markdown); `.yml`/`.yaml` produce pure YAML. This is the inverse of
 * {@link parseAgentFileText} — a round-trip (parse → serialize → parse)
 * preserves the agent's name, description, tools, prompt, and other fields.
 *
 * For `.md` output, the `prompt` field is placed in the body (after the
 * closing `---` fence) and all other fields go into the YAML frontmatter.
 * For `.yaml` output, all fields including `prompt` are placed in the YAML
 * map (the body is empty, matching the parse behavior for pure-YAML files).
 */
export function serializeAgentFile(def: AgentFileDefinition): string {
  const data: Record<string, unknown> = { name: def.name };
  if (def.description !== undefined) data['description'] = def.description;
  if (def.whenToUse !== undefined) data['whenToUse'] = def.whenToUse;
  if (def.override) data['override'] = true;
  if (def.tools !== undefined) data['tools'] = [...def.tools];
  if (def.disallowedTools !== undefined) data['disallowedTools'] = [...def.disallowedTools];
  if (def.subagents !== undefined) data['subagents'] = [...def.subagents];
  if (def.modelPreference !== undefined) data['model_preference'] = def.modelPreference;
  if (def.defaultModel !== undefined) data['defaultModel'] = def.defaultModel;
  if (def.capabilitiesRequired !== undefined) data['capabilitiesRequired'] = [...def.capabilitiesRequired];
  if (def.extends !== undefined) data['extends'] = def.extends;
  if (def.promptVars !== undefined) data['promptVars'] = def.promptVars;

  if (isYamlAgentFile(def.path)) {
    if (def.prompt.length > 0) {
      data['prompt'] = def.prompt;
    }
    return dumpYaml(data, { lineWidth: -1 }).trimEnd() + '\n';
  }

  // .md format: frontmatter + body
  return serializeFrontmatter(data, def.prompt);
}
