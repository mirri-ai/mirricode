import type { SlashCommandHost } from './dispatch';
import { ChoicePickerComponent, type ChoiceOption } from '../components/dialogs/choice-picker';

const BUILTIN_AGENT_PROFILES = [
  { name: 'agent', description: 'Default main agent (essential)', builtin: true },
  { name: 'coder', description: 'General-purpose software engineering sub-agent', builtin: true },
  { name: 'explore', description: 'Read-only codebase exploration sub-agent', builtin: true },
  { name: 'plan', description: 'Implementation planning sub-agent', builtin: true },
];

/**
 * `/agents` slash command — lists built-in agent profiles with their
 * enabled/disabled status. Selecting a non-essential profile toggles
 * its enabled state by updating `disabled_agents` in config.toml.
 */
export async function handleAgentsCommand(host: SlashCommandHost): Promise<void> {
  await showAgentsSelector(host);
}

async function showAgentsSelector(host: SlashCommandHost): Promise<void> {
  const config = await host.harness.getConfig();
  const disabledAgents = new Set(config.disabledAgents ?? []);

  const options: ChoiceOption[] = BUILTIN_AGENT_PROFILES.map((p) => {
    const isEnabled = !disabledAgents.has(p.name) || p.name === 'agent';
    const statusLabel = isEnabled ? '✓ enabled' : '✗ disabled';
    const essentialLabel = p.name === 'agent' ? ' (essential)' : '';
    return {
      value: p.name,
      label: `${p.name} — ${statusLabel}${essentialLabel}`,
      description: p.description,
    };
  });

  host.mountEditorReplacement(
    new ChoicePickerComponent({
      title: 'Agent Profiles',
      options,
      searchable: true,
      onSelect: (value) => {
        void handleAgentToggle(host, value);
      },
      onCancel: () => {
        host.restoreEditor();
      },
    }),
  );
}

async function handleAgentToggle(host: SlashCommandHost, name: string): Promise<void> {
  if (name === 'agent') {
    host.showStatus('The "agent" profile is essential and cannot be disabled');
    host.restoreEditor();
    return;
  }

  const config = await host.harness.getConfig();
  const currentDisabled = new Set(config.disabledAgents ?? []);

  if (currentDisabled.has(name)) {
    currentDisabled.delete(name);
  } else {
    currentDisabled.add(name);
  }

  try {
    await host.harness.setConfig({
      ...config,
      disabledAgents: [...currentDisabled],
    });
    host.showStatus(`Agent "${name}" ${currentDisabled.has(name) ? 'disabled' : 'enabled'}`);
    await showAgentsSelector(host);
  } catch (error) {
    host.showError(`Failed to update agent profile: ${error instanceof Error ? error.message : String(error)}`);
    host.restoreEditor();
  }
}
