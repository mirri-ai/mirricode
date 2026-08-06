/**
 * `externalHooks` domain — `hooks` config-section schema and TOML
 * transforms.
 *
 * Owns the `[[hooks]]` configuration section (external hook definitions),
 * including the snake_case ↔ camelCase TOML transforms for each hook entry.
 * Registered at module load via `registerConfigSection`.
 */

import { z } from 'zod';

import { registerConfigSection } from '#/app/config/configSectionContributions';
import { isPlainObject, plainObjectToToml, transformPlainObject } from '#/app/config/toml';

import { HOOK_EVENT_TYPES } from './types';

export const HOOKS_SECTION = 'hooks';

/**
 * Per-entry hook schema using the known v2 event set.
 */
export const HookDefSchema = z
  .object({
    event: z.enum(HOOK_EVENT_TYPES),
    matcher: z.string().optional(),
    command: z.string().min(1),
    timeout: z.number().int().min(1).max(600).optional(),
  })
  .strict();

export type HookDefConfig = z.infer<typeof HookDefSchema>;

/**
 * Hooks array schema: validates each entry individually and silently drops
 * entries with unrecognised event names instead of rejecting the entire array.
 * A rejected entry is replaced by `undefined` (via `.catch()`) then filtered out
 * by `.transform()`. The ConfigService then sees only the valid entries.
 */
export const HooksConfigSchema = z
  .array(
    z
      .object({
        event: z.enum(HOOK_EVENT_TYPES),
        matcher: z.string().optional(),
        command: z.string().min(1),
        timeout: z.number().int().min(1).max(600).optional(),
      })
      .strict()
      .catch(() => undefined as never),
  )
  .transform((entries) => entries.filter((e): e is HookDefConfig => e !== undefined));

export const hooksFromToml = (rawSnake: unknown): unknown => {
  if (!Array.isArray(rawSnake)) return rawSnake;
  return rawSnake.map((hook) => (isPlainObject(hook) ? transformPlainObject(hook) : hook));
};

export const hooksToToml = (value: unknown, _rawSnake: unknown): unknown => {
  if (!Array.isArray(value)) return value;
  return value.map((hook) => (isPlainObject(hook) ? plainObjectToToml(hook, undefined) : hook));
};

registerConfigSection(HOOKS_SECTION, HooksConfigSchema, {
  fromToml: hooksFromToml,
  toToml: hooksToToml,
});