import { describe, expect, it, vi } from 'vitest';

import { ErrorCodes, MirriError } from '@mirri-ai/mirri-code-sdk';
import { handleAgentsCommand } from '#/tui/commands/index';
import type { SlashCommandHost } from '#/tui/commands/dispatch';

const ENTER = '\r';
const DOWN = '\u001B[B';

const LEGACY_ENGINE_MESSAGE =
  'The legacy engine does not support /agents management. Use the settings page (mirri web) or edit the config file.';

function stripAnsi(text: string): string {
  return text.replaceAll(/\u001B\[[0-9;]*m/g, '');
}

interface TestPicker {
  handleInput(data: string): void;
  render(width: number): string[];
}

/** A wire-shaped `ProfileEntry` row with test-friendly defaults. */
function profileRow(overrides: Partial<Record<string, unknown>>): Record<string, unknown> {
  return {
    name: 'agent',
    description: 'Default main agent',
    source: 'builtin',
    builtin: true,
    essential: true,
    enabled: true,
    has_override: false,
    ...overrides,
  };
}

function makeHost(
  listResult: { items: readonly unknown[]; skipped?: readonly unknown[] },
  overrides: {
    setEnabledError?: Error;
    listError?: Error;
  } = {},
) {
  const { listError, setEnabledError } = overrides;
  const harness = {
    listAgentProfiles: listError
      ? vi.fn(async (): Promise<never> => {
          throw listError;
        })
      : vi.fn(async () => ({ items: listResult.items, skipped: listResult.skipped ?? [] })),
    setAgentProfileEnabled: setEnabledError
      ? vi.fn(async (): Promise<never> => {
          throw setEnabledError;
        })
      : vi.fn(async () => ({ ok: true })),
  };
  const host = {
    harness,
    showError: vi.fn(),
    showStatus: vi.fn(),
    mountEditorReplacement: vi.fn(),
    restoreEditor: vi.fn(),
  } as unknown as SlashCommandHost;
  return { host, harness };
}

function mountedPicker(host: SlashCommandHost, call = 0): TestPicker {
  const mock = host.mountEditorReplacement as ReturnType<typeof vi.fn>;
  return mock.mock.calls[call]?.[0] as TestPicker;
}

function renderedPicker(host: SlashCommandHost, call = 0): string {
  return stripAnsi(mountedPicker(host, call).render(100).join('\n'));
}

describe('handleAgentsCommand', () => {
  it('given a data-plane list with custom, disabled and essential rows, when /agents, then the picker shows the data-plane rows with status and essential markers', async () => {
    const rows = [
      profileRow({
        name: 'code-reviewer',
        description: 'Custom reviewer',
        source: 'user',
        builtin: false,
        essential: false,
        enabled: false,
      }),
      profileRow({ name: 'agent' }),
      profileRow({
        name: 'guardian',
        description: 'Custom essential role',
        source: 'user',
        builtin: false,
        essential: true,
        enabled: false,
      }),
    ];
    const { host } = makeHost({ items: rows });

    await handleAgentsCommand(host);

    expect(host.mountEditorReplacement).toHaveBeenCalledOnce();
    const text = renderedPicker(host);
    // Non-builtin custom names come from the data plane — no hardcoded list.
    expect(text).toContain('code-reviewer — ✗ disabled');
    expect(text).toContain('agent — ✓ enabled (essential)');
    // The essential marker comes from the row's `essential` field, not a
    // local name check — a custom essential name is marked too.
    expect(text).toContain('guardian — ✗ disabled (essential)');
    // Descriptions keep flowing through.
    expect(text).toContain('Custom reviewer');
    // No skipped files → no skip summary line.
    expect(text).not.toContain('skipped');
  });

  it('given a non-essential disabled profile is selected, when confirmed, then setAgentProfileEnabled(name, true) is called and the list refreshes', async () => {
    const rows = [
      profileRow({
        name: 'code-reviewer',
        description: 'Custom reviewer',
        source: 'user',
        builtin: false,
        essential: false,
        enabled: false,
      }),
      profileRow({ name: 'agent' }),
    ];
    const { host, harness } = makeHost({ items: rows });

    await handleAgentsCommand(host);
    mountedPicker(host).handleInput(ENTER);

    await vi.waitFor(() => {
      expect(harness.setAgentProfileEnabled).toHaveBeenCalledWith('code-reviewer', true);
    });
    expect(host.showStatus).toHaveBeenCalledWith('Agent "code-reviewer" enabled');
    // The list is re-read and the picker remounted with fresh rows.
    await vi.waitFor(() => {
      expect(harness.listAgentProfiles).toHaveBeenCalledTimes(2);
    });
    expect(host.mountEditorReplacement).toHaveBeenCalledTimes(2);
    expect(host.showError).not.toHaveBeenCalled();
    expect(host.restoreEditor).not.toHaveBeenCalled();
  });

  it('given an enabled non-essential profile is selected, when confirmed, then setAgentProfileEnabled(name, false) is called', async () => {
    const rows = [profileRow({ name: 'agent' }), profileRow({ name: 'explore', essential: false, enabled: true })];
    const { host, harness } = makeHost({ items: rows });

    await handleAgentsCommand(host);
    mountedPicker(host).handleInput(DOWN);
    mountedPicker(host).handleInput(ENTER);

    await vi.waitFor(() => {
      expect(harness.setAgentProfileEnabled).toHaveBeenCalledWith('explore', false);
    });
    expect(host.showStatus).toHaveBeenCalledWith('Agent "explore" disabled');
    expect(host.showError).not.toHaveBeenCalled();
  });

  it('given the essential row is selected and the engine refuses, when confirmed, then the engine error message is surfaced verbatim', async () => {
    const rows = [
      profileRow({ name: 'agent' }),
      profileRow({
        name: 'code-reviewer',
        description: 'Custom reviewer',
        source: 'user',
        builtin: false,
        essential: false,
        enabled: true,
      }),
    ];
    const { host, harness } = makeHost(
      { items: rows },
      {
        setEnabledError: new Error('Cannot disable the essential profile "agent"'),
      },
    );

    await handleAgentsCommand(host);
    mountedPicker(host).handleInput(ENTER);

    await vi.waitFor(() => {
      expect(harness.setAgentProfileEnabled).toHaveBeenCalledWith('agent', false);
    });
    await vi.waitFor(() => {
      expect(host.showError).toHaveBeenCalledWith('Cannot disable the essential profile "agent"');
    });
    // No local essential hardcode — only the engine's refusal text.
    expect(host.showError).not.toHaveBeenCalledWith(
      'The "agent" profile is essential and cannot be disabled',
    );
    expect(host.restoreEditor).toHaveBeenCalled();
    // The list is not refreshed after a refused toggle.
    expect(harness.listAgentProfiles).toHaveBeenCalledTimes(1);
  });

  it('given the v1 engine whose setAgentProfileEnabled rejects with NOT_IMPLEMENTED, when a profile is confirmed, then the legacy fallback message is shown and no refresh happens', async () => {
    const rows = [profileRow({ name: 'agent' }), profileRow({ name: 'explore', essential: false, enabled: true })];
    const { host, harness } = makeHost(
      { items: rows },
      {
        setEnabledError: new MirriError(
          ErrorCodes.NOT_IMPLEMENTED,
          'SDK setAgentProfileEnabled is not available on this engine',
        ),
      },
    );

    await handleAgentsCommand(host);
    mountedPicker(host).handleInput(DOWN);
    mountedPicker(host).handleInput(ENTER);

    await vi.waitFor(() => {
      expect(harness.setAgentProfileEnabled).toHaveBeenCalledWith('explore', false);
    });
    // The legacy fallback message, never the raw NOT_IMPLEMENTED text.
    expect(host.showError).toHaveBeenCalledWith(LEGACY_ENGINE_MESSAGE);
    expect(host.showError).not.toHaveBeenCalledWith(
      'SDK setAgentProfileEnabled is not available on this engine',
    );
    // The picker closes and the list is not re-read after a refused toggle.
    expect(host.restoreEditor).toHaveBeenCalled();
    expect(harness.listAgentProfiles).toHaveBeenCalledTimes(1);
  });

  it('given the v1 engine whose listAgentProfiles rejects with NOT_IMPLEMENTED, when /agents, then the legacy fallback message is shown and no picker mounts', async () => {
    const { host, harness } = makeHost(
      { items: [] },
      {
        listError: new MirriError(
          ErrorCodes.NOT_IMPLEMENTED,
          'SDK listAgentProfiles is not available on this engine',
        ),
      },
    );

    await handleAgentsCommand(host);

    expect(harness.listAgentProfiles).toHaveBeenCalledTimes(1);
    expect(host.showError).toHaveBeenCalledWith(LEGACY_ENGINE_MESSAGE);
    expect(host.showError).not.toHaveBeenCalledWith(
      'SDK listAgentProfiles is not available on this engine',
    );
    expect(host.mountEditorReplacement).not.toHaveBeenCalled();
  });

  it('given a non-NOT_IMPLEMENTED list failure, when /agents, then the engine error message is surfaced', async () => {
    const { host } = makeHost(
      { items: [] },
      { listError: new Error('catalog unavailable') },
    );

    await handleAgentsCommand(host);

    expect(host.showError).toHaveBeenCalledWith('catalog unavailable');
    expect(host.mountEditorReplacement).not.toHaveBeenCalled();
  });

  it('given an empty data-plane list, when /agents, then the picker mounts with no rows and no error', async () => {
    const { host } = makeHost({ items: [] });

    await handleAgentsCommand(host);

    expect(host.showError).not.toHaveBeenCalled();
    expect(host.mountEditorReplacement).toHaveBeenCalledOnce();
    expect(renderedPicker(host)).toContain('No matches');
  });

  it('given skipped agent files, when /agents, then a one-line skip summary is shown after the list', async () => {
    const { host } = makeHost({
      items: [profileRow({ name: 'agent' })],
      skipped: [
        { path: '/home/agents/broken.md', reason: 'invalid frontmatter' },
        { path: '/home/agents/worse.md', reason: 'parse error' },
        { path: '/home/agents/bad.md', reason: 'unknown field' },
      ],
    });

    await handleAgentsCommand(host);

    const text = renderedPicker(host);
    expect(text).toContain('3 profile file(s) skipped');
    // The summary does not expand into per-file details.
    expect(text).not.toContain('broken.md');
    expect(text).not.toContain('invalid frontmatter');
  });
});
