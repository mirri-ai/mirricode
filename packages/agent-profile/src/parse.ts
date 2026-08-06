/**
 * Agent profile file parsing — pure function, no IO.
 *
 * Parses a `.md` file (YAML frontmatter + Markdown body) or a legacy `.yaml`/
 * `.yml` file into an `AgentFileDef`. Legacy field names are mapped to the
 * unified schema; unknown legacy fields are silently ignored so old files
 * do not break discovery.
 */

import { load as loadYaml } from 'js-yaml';

import { FrontmatterError, parseFrontmatter } from './frontmatter';
import { AgentFileParseError } from './errors';

export { AgentFileParseError };
import {
  AGENT_NAME_PATTERN,
  type AgentFileDef,
  type AgentFileSource,
} from './schema';

const YAML_EXTENSIONS = ['.yml', '.yaml'] as const;

function isYamlFile(path: string): boolean {
  return YAML_EXTENSIONS.some((ext) => path.endsWith(ext));
}

const DEFAULT_PROMPT = 'You are a helpful assistant.';

export interface ParseAgentFileOptions {
  readonly path: string;
  readonly text: string;
  readonly source?: AgentFileSource;
}

export function parseAgentFileText(options: ParseAgentFileOptions): AgentFileDef {
  let data: unknown;
  let body: string;

  if (isYamlFile(options.path)) {
    try {
      data = loadYaml(options.text) ?? {};
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      throw new AgentFileParseError(`Invalid YAML in ${options.path}: ${message}`, error);
    }
    body = '';
  } else {
    try {
      const parsed = parseFrontmatter(options.text);
      data = parsed.data;
      body = parsed.body.trim();
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

  if (data === null) {
    throw new AgentFileParseError(`Missing frontmatter in ${options.path}`);
  }
  if (typeof data !== 'object' || data === null || Array.isArray(data)) {
    throw new AgentFileParseError(
      `Frontmatter in ${options.path} must be a mapping at the top level`,
    );
  }

  const record = data as Record<string, unknown>;

  // Name: explicit field, or derive from filename.
  const nameField = record['name'];
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

  // Description: required non-empty string.
  const description = requiredNonEmptyString(record['description'], 'description', options.path);

  // Prompt: from body (.md) or from prompt/systemPromptTemplate field (.yaml).
  let prompt: string;
  if (!isYamlFile(options.path)) {
    if (body.length === 0) {
      throw new AgentFileParseError(`Missing prompt body in ${options.path}`);
    }
    prompt = body;
  } else {
    const promptField = nonEmptyString(record['prompt'] ?? record['systemPromptTemplate']);
    // Legacy YAML files used promptVars.roleAdditional as the system prompt body
    // before the unified schema moved it inline. If no explicit prompt field is
    // present but roleAdditional is, use that as the prompt.
    const roleAdditional =
      record['promptVars'] !== undefined &&
      typeof record['promptVars'] === 'object' &&
      !Array.isArray(record['promptVars'])
        ? nonEmptyString((record['promptVars'] as Record<string, unknown>)['roleAdditional'])
        : undefined;
    prompt = promptField ?? roleAdditional ?? DEFAULT_PROMPT;
  }

  // Optional fields.
  const whenToUse = nonEmptyString(record['whenToUse']);
  const defaultModel = nonEmptyString(record['defaultModel'] ?? record['default_model']);
  const tools = parseStringList(record['tools']);
  const subagents = parseSubagents(record['subagents']);
  const capabilitiesRequired = parseStringList(
    record['capabilitiesRequired'] ?? record['capabilities_required'],
  );

  return {
    name,
    description,
    whenToUse,
    defaultModel,
    tools,
    subagents,
    capabilitiesRequired,
    prompt,
    filePath: options.path,
    source: options.source,
  };
}

function deriveNameFromPath(filePath: string): string | undefined {
  const base = filePath.split(/[\\/]/).pop() ?? '';
  const name = base.replace(/\.[^.]*$/, '');
  return name !== '' ? name : undefined;
}

function nonEmptyString(value: unknown): string | undefined {
  return typeof value === 'string' && value.trim() !== '' ? value.trim() : undefined;
}

function requiredNonEmptyString(
  value: unknown,
  field: string,
  filePath: string,
): string {
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

function parseStringList(value: unknown): readonly string[] | undefined {
  if (value === undefined || value === null) return undefined;
  if (typeof value === 'string') {
    return value
      .split(',')
      .map((item) => item.trim())
      .filter((item) => item !== '');
  }
  if (!Array.isArray(value)) return undefined;
  return value.filter((item): item is string => typeof item === 'string' && item.trim() !== '');
}

/**
 * Parse the legacy `subagents` field. In v1 it was `Record<string, {description}>`;
 * in v2 it is `string[]`. Normalize both to `string[]`.
 */
function parseSubagents(value: unknown): readonly string[] | undefined {
  if (value === undefined || value === null) return undefined;
  if (Array.isArray(value)) {
    return value.filter((item): item is string => typeof item === 'string' && item.trim() !== '');
  }
  if (typeof value === 'object') {
    return Object.keys(value as Record<string, unknown>);
  }
  return undefined;
}
