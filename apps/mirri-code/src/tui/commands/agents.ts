import { ErrorCodes, isMirriError } from '@mirri-ai/mirri-code-sdk';

import type { SlashCommandHost } from './dispatch';
import { ChoicePickerComponent, type ChoiceOption } from '../components/dialogs/choice-picker';

/** D18-a: the v1 engine has no agent-profile data plane — point the user at
 * the web settings page or the config file instead of a dead list. */
const LEGACY_ENGINE_MESSAGE =
  'The legacy engine does not support /agents management. Use the settings page (mirri web) or edit the config file.';

/**
 * `/agents` slash command — lists the agent profiles from the harness data
 * plane (`harness.listAgentProfiles()`, the same source the web settings
 * page reads) with their enabled/disabled status. Selecting a profile
 * toggles its enabled state through `harness.setAgentProfileEnabled()`;
 * essential-profile refusals and other engine errors are surfaced verbatim.
 */
export async function handleAgentsCommand(host: SlashCommandHost): Promise<void> {
  await showAgentsSelector(host);
}

async function showAgentsSelector(host: SlashCommandHost): Promise<void> {
  let list;
  try {
    // No workspace root is passed (the SDK method takes none): the read
    // resolves any open workspace and falls back to the global sources —
    // the same resolution the REST endpoint applies. `/agents` is a global
    // view, so the TUI deliberately does not materialize a workspace from
    // the current workDir (D17 / Task 32 review follow-up item 1).
    list = await host.harness.listAgentProfiles();
  } catch (error) {
    host.showError(isNotImplementedError(error) ? LEGACY_ENGINE_MESSAGE : errorMessage(error));
    return;
  }

  const entries = new Map(list.items.map((profile) => [profile.name, profile]));
  const options: ChoiceOption[] = list.items.map((profile) => {
    const statusLabel = profile.enabled ? '✓ enabled' : '✗ disabled';
    const essentialLabel = profile.essential ? ' (essential)' : '';
    return {
      value: profile.name,
      label: `${profile.name} — ${statusLabel}${essentialLabel}`,
      description: profile.description,
    };
  });

  host.mountEditorReplacement(
    new ChoicePickerComponent({
      title: 'Agent Profiles',
      options,
      searchable: true,
      notice:
        list.skipped.length > 0 ? `${list.skipped.length} profile file(s) skipped` : undefined,
      noticeTone: 'warning',
      onSelect: (value) => {
        const entry = entries.get(value);
        if (entry === undefined) return;
        void handleAgentToggle(host, entry.name, entry.enabled);
      },
      onCancel: () => {
        host.restoreEditor();
      },
    }),
  );
}

async function handleAgentToggle(
  host: SlashCommandHost,
  name: string,
  enabled: boolean,
): Promise<void> {
  try {
    await host.harness.setAgentProfileEnabled(name, !enabled);
    host.showStatus(`Agent "${name}" ${enabled ? 'disabled' : 'enabled'}`);
  } catch (error) {
    if (isNotImplementedError(error)) {
      host.showError(LEGACY_ENGINE_MESSAGE);
    } else {
      // The engine's refusal text verbatim — e.g. the v2/rust essential
      // profile rejection — never a local essential hardcode.
      host.showError(errorMessage(error));
    }
    host.restoreEditor();
    return;
  }
  await showAgentsSelector(host);
}

function isNotImplementedError(error: unknown): boolean {
  return isMirriError(error) && error.code === ErrorCodes.NOT_IMPLEMENTED;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
