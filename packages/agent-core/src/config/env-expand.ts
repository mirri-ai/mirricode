/**
 * Environment-variable expansion for config string values.
 *
 * Supports two syntaxes inside any string value:
 *   - `${VAR_NAME}`       — POSIX-style reference
 *   - `${env:VAR_NAME}`   — explicit `env:` prefix (VS Code / common variant)
 *
 * Undefined variables resolve to the empty string; the downstream zod schema
 * is responsible for rejecting values that became invalid as a result
 * (e.g. an empty URL). Only string *values* are expanded — object keys,
 * numbers, booleans and `null` pass through untouched.
 *
 * The lookup function is injectable so tests can supply a deterministic
 * environment without touching `process.env`.
 */

/** Resolves a variable name to its value, or `undefined` when unset. */
export type EnvLookup = (name: string) => string | undefined;

const DEFAULT_ENV_LOOKUP: EnvLookup = (name) => process.env[name];

/**
 * Matches `${VAR}` and `${env:VAR}`, capturing the variable name.
 * Variable names follow the POSIX-ish rule: a letter or underscore followed
 * by letters, digits or underscores.
 */
const ENV_VAR_PATTERN = /\$\{(?:env:)?([A-Za-z_][A-Za-z0-9_]*)\}/g;

/**
 * Expand environment-variable references in a single string.
 * Runs a single left-to-right pass (no recursive expansion of interpolated
 * values that themselves contain `${...}`).
 */
export function expandEnvString(input: string, lookup: EnvLookup = DEFAULT_ENV_LOOKUP): string {
  return input.replace(ENV_VAR_PATTERN, (_match, varName: string) => lookup(varName) ?? '');
}

/**
 * Recursively expand environment-variable references in an arbitrary JSON value.
 * - Strings are expanded via {@link expandEnvString}.
 * - Arrays are mapped element-by-element.
 * - Object *values* are recursed; object *keys* are left literal.
 * - Primitives (number, boolean, null) are returned as-is.
 *
 * Returns a new structure; the input is never mutated.
 */
export function expandEnvVars(value: unknown, lookup: EnvLookup = DEFAULT_ENV_LOOKUP): unknown {
  if (typeof value === 'string') return expandEnvString(value, lookup);
  if (Array.isArray(value)) return value.map((item) => expandEnvVars(item, lookup));
  if (value !== null && typeof value === 'object') {
    const result: Record<string, unknown> = {};
    for (const [key, val] of Object.entries(value as Record<string, unknown>)) {
      result[key] = expandEnvVars(val, lookup);
    }
    return result;
  }
  return value;
}

/**
 * Whether a string is a *pure* environment-variable reference (e.g.
 * `${MY_VAR}` or `${env:MY_VAR}`), as opposed to a literal that merely
 * contains an embedded reference (e.g. `prefix-${MY_VAR}`). Used to decide
 * whether a config value is safe to expose to the UI without leaking a
 * secret — a pure reference is not itself a secret.
 */
export function isEnvRef(input: string): boolean {
  return /^\$\{(?:env:)?[A-Za-z_][A-Za-z0-9_]*\}$/.test(input);
}
