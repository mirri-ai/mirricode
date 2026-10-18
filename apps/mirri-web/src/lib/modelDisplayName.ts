import type { AppModel } from '../api/types';

/**
 * Resolve a model alias to its human-facing display name for transcript
 * surfaces (the model-switch divider, T9/T11 review item C): exact id
 * match first (model names can collide across providers — the same
 * precedence the client's status mapping uses), then a raw-name match,
 * then the model's bare name, and finally the alias verbatim — a divider
 * must never blank out just because the alias is no longer registered
 * (e.g. rendering a resumed session after the user removed the model).
 */
export function resolveModelDisplayName(
  models: AppModel[] | undefined,
  alias: string | null | undefined,
): string {
  if (alias === null || alias === undefined || alias.length === 0) return '—';
  const matched =
    models?.find((m) => m.id === alias) ?? models?.find((m) => m.model === alias);
  return matched?.displayName ?? matched?.model ?? alias;
}
