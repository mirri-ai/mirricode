import type { ModelAlias } from '@mirri-ai/mirri-code-sdk';
import { truncateToWidth, type Component } from '@mirri-ai/pi-tui';

import { modelDisplayName } from '#/tui/components/dialogs/model-selector';
import { STATUS_BULLET } from '#/tui/constant/symbols';
import { currentTheme } from '#/tui/theme';

/**
 * The mid-turn model-switch divider (spec 11 §2.5 A2): a single-line
 * bullet + label marker in the swarm-marker idiom — "Model switched:
 * A → B" — rendered live from the translated `model_switched` event and
 * on replay from the persisted marker message (origin kind
 * `model_switched`). The wire event carries model aliases; the display
 * name mapping is a presentation-end concern (M4 is not an identifier),
 * so the component resolves the aliases against the catalog and falls
 * back to the alias text when the catalog does not know the model.
 */
export class ModelSwitchMarkerComponent implements Component {
  constructor(
    private readonly from: string,
    private readonly to: string,
    private readonly models: Record<string, ModelAlias> = {},
  ) {}

  invalidate(): void {}

  render(width: number): string[] {
    const safeWidth = Math.max(0, width);
    if (safeWidth <= 0) return [''];

    const token = 'textDim';
    const marker = currentTheme.boldFg(token, STATUS_BULLET);
    const label = currentTheme.boldFg(
      token,
      `Model switched: ${modelDisplayName(this.from, this.models[this.from])} → ${modelDisplayName(this.to, this.models[this.to])}`,
    );
    return ['', truncateToWidth(marker + label, safeWidth, '…')];
  }
}

/** A validated model-switch pair as carried by the translated event's patch. */
export interface ModelSwitchPatch {
  readonly from: string;
  readonly to: string;
}

/**
 * Validate the `modelSwitched` member of a translated
 * `session.meta.updated` patch (the node-sdk translation layer's spelling
 * of the rust `event.session.model_switched` payload). Returns `undefined`
 * for anything malformed — a corrupted patch must never render a divider.
 */
export function parseModelSwitchPatch(raw: unknown): ModelSwitchPatch | undefined {
  if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) return undefined;
  const record = raw as Record<string, unknown>;
  const from = record['from'];
  const to = record['to'];
  if (typeof from !== 'string' || from.length === 0) return undefined;
  if (typeof to !== 'string' || to.length === 0) return undefined;
  return { from, to };
}
