import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { SyncDescriptor } from '#/_base/di/descriptors';
import { DisposableStore } from '#/_base/di/lifecycle';
import { TestInstantiationService } from '#/_base/di/test';
import { IBootstrapService } from '#/app/bootstrap/bootstrap';
import { IConfigRegistry, IConfigService } from '#/app/config/config';
import { ConfigRegistry, ConfigService } from '#/app/config/configService';
import {
  EXPERIMENTAL_SECTION,
  IFlagService,
} from '#/app/flag/flag';
import { IFlagRegistry, type FlagDefinitionInput, getContributedFlags } from '#/app/flag/flagRegistry';
import { FlagRegistryService } from '#/app/flag/flagRegistryService';
import { FlagService, MASTER_ENV } from '#/app/flag/flagService';
import { ILogService } from '#/_base/log/log';
import { IAtomicTomlDocumentStore } from '#/persistence/interface/atomicDocumentStore';
import { TomlAtomicDocumentStore } from '#/persistence/backends/node-fs/atomicDocumentStore';
import { InMemoryStorageService } from '#/persistence/backends/memory/inMemoryStorageService';
import { IFileSystemStorageService } from '#/persistence/interface/storage';

import { TOOL_SELECT_FLAG_ID, TOOL_SELECT_FLAG_ENV } from '#/agent/toolSelect/flag';
import { HOOK_COMMAND_REWRITE_FLAG_ID, HOOK_COMMAND_REWRITE_FLAG_ENV } from '#/agent/hookCommandRewrite/flag';

import { stubBootstrap } from '../bootstrap/stubs';
import { stubLog } from '../../_base/log/stubs';

const exampleFlag: FlagDefinitionInput = {
  id: 'example_flag',
  title: 'Example flag',
  description: 'Example experimental flag used to exercise the flag registry.',
  env: 'MIRRICODE_EXPERIMENTAL_EXAMPLE_FLAG',
  default: true,
  surface: 'core',
};

describe('FlagRegistryService', () => {
  it('registers and resolves by id', () => {
    const reg = new FlagRegistryService();
    reg.register(exampleFlag);
    // The constructor already drains getContributedFlags(), so example_flag
    // is appended after the import-time contributions.
    expect(reg.get('example_flag')?.env).toBe('MIRRICODE_EXPERIMENTAL_EXAMPLE_FLAG');
  });

  it('returns undefined for an unknown id', () => {
    const reg = new FlagRegistryService();
    expect(reg.get('does_not_exist')).toBeUndefined();
  });

  it('throws on a duplicate id', () => {
    const reg = new FlagRegistryService();
    reg.register(exampleFlag);
    expect(() => reg.register(exampleFlag)).toThrow();
  });

  it('unregisters when the returned disposable is disposed', () => {
    const reg = new FlagRegistryService();
    const handle = reg.register(exampleFlag);
    handle.dispose();
    expect(reg.get('example_flag')).toBeUndefined();
  });
});

describe('FlagService', () => {
  let disposables: DisposableStore;
  let homeDir: string;

  beforeEach(() => {
    disposables = new DisposableStore();
    homeDir = `/tmp/mirri-code-flag-${process.pid}-${Date.now()}-${Math.random().toString(36).slice(2)}`;
  });
  afterEach(() => disposables.dispose());

  function makeFlags(env: Readonly<Record<string, string | undefined>> = {}) {
    const ix = disposables.add(new TestInstantiationService());
    ix.stub(IBootstrapService, stubBootstrap(homeDir, env));
    ix.stub(ILogService, stubLog());
    ix.stub(IFileSystemStorageService, new InMemoryStorageService());
    ix.set(IAtomicTomlDocumentStore, new SyncDescriptor(TomlAtomicDocumentStore));
    ix.set(IConfigRegistry, new SyncDescriptor(ConfigRegistry));
    ix.set(IConfigService, new SyncDescriptor(ConfigService));
    ix.set(IFlagRegistry, new SyncDescriptor(FlagRegistryService));
    ix.set(IFlagService, new SyncDescriptor(FlagService));
    ix.get(IFlagRegistry).register(exampleFlag);
    return {
      registry: ix.get(IConfigRegistry),
      config: ix.get(IConfigService),
      flags: ix.get(IFlagService),
    };
  }

  it('registers the experimental config section downward', () => {
    const { registry } = makeFlags();
    expect(registry.getSection(EXPERIMENTAL_SECTION)).toMatchObject({
      domain: EXPERIMENTAL_SECTION,
    });
    expect(registry.getSection(EXPERIMENTAL_SECTION)?.schema).toBeDefined();
  });

  it('resolves the registry default when nothing overrides it', () => {
    const { flags } = makeFlags();
    const state = flags.explain('example_flag');
    expect(state?.enabled).toBe(true);
    expect(state?.source).toBe('default');
    expect(flags.enabled('example_flag')).toBe(true);
  });

  it('returns undefined for an unregistered flag', () => {
    const { flags } = makeFlags();
    expect(flags.explain('does_not_exist')).toBeUndefined();
    expect(flags.enabled('does_not_exist')).toBe(false);
  });

  it('applies config overrides above the default', async () => {
    const { config, flags } = makeFlags();
    await config.set(EXPERIMENTAL_SECTION, { example_flag: false });
    const state = flags.explain('example_flag');
    expect(state?.enabled).toBe(false);
    expect(state?.source).toBe('config');
    expect(state?.configValue).toBe(false);
  });

  it('lets per-feature env override config', async () => {
    const { config, flags } = makeFlags({
      MIRRICODE_EXPERIMENTAL_EXAMPLE_FLAG: 'true',
    });
    await config.set(EXPERIMENTAL_SECTION, { example_flag: false });
    const state = flags.explain('example_flag');
    expect(state?.enabled).toBe(true);
    expect(state?.source).toBe('env');
    expect(state?.configValue).toBe(false);
  });

  it('lets the master env switch force every flag on', async () => {
    const { config, flags } = makeFlags({ [MASTER_ENV]: '1' });
    await config.set(EXPERIMENTAL_SECTION, { example_flag: false });
    const state = flags.explain('example_flag');
    expect(state?.enabled).toBe(true);
    expect(state?.source).toBe('master-env');
  });

  it('refreshes overrides when the experimental config section changes', async () => {
    const { config, flags } = makeFlags();
    expect(flags.enabled('example_flag')).toBe(true);
    await config.set(EXPERIMENTAL_SECTION, { example_flag: false });
    expect(flags.enabled('example_flag')).toBe(false);
    await config.set(EXPERIMENTAL_SECTION, { example_flag: true });
    expect(flags.enabled('example_flag')).toBe(true);
  });

  it('ignores unrelated config section changes', async () => {
    const { config, flags } = makeFlags();
    await config.set('agent', { modelAlias: 'k2' });
    expect(flags.explain('example_flag')?.source).toBe('default');
  });

  it('supports imperative setConfigOverrides', () => {
    const { flags } = makeFlags();
    flags.setConfigOverrides({ example_flag: false });
    expect(flags.enabled('example_flag')).toBe(false);
    flags.setConfigOverrides(undefined);
    expect(flags.enabled('example_flag')).toBe(true);
  });

  it('exposes snapshot / enabledIds / explainAll', () => {
    const { flags } = makeFlags();
    // example_flag defaults to true; all product flags default to false.
    const snap = flags.snapshot();
    expect(snap['example_flag']).toBe(true);
    expect(flags.enabledIds()).toContain('example_flag');
    expect(flags.explainAll().map((s) => s.id)).toContain('example_flag');
  });

  it('treats truthy env values case-insensitively', () => {
    const { flags } = makeFlags({ MIRRICODE_EXPERIMENTAL_EXAMPLE_FLAG: 'YES' });
    expect(flags.enabled('example_flag')).toBe(true);
  });

  it('treats falsy env values case-insensitively', () => {
    const { flags } = makeFlags({ MIRRICODE_EXPERIMENTAL_EXAMPLE_FLAG: 'off' });
    expect(flags.enabled('example_flag')).toBe(false);
  });

  it('reads only the env name declared in the registry', () => {
    const { flags } = makeFlags({ MIRRICODE_EXPERIMENTAL_UNKNOWN: 'false' });
    expect(flags.enabled('example_flag')).toBe(true);
  });

  it('ignores garbage env values', () => {
    const { flags } = makeFlags({ MIRRICODE_EXPERIMENTAL_EXAMPLE_FLAG: 'maybe' });
    expect(flags.enabled('example_flag')).toBe(true);
  });

  it('ignores obsolete config ids outside the registry', async () => {
    const { config, flags } = makeFlags();
    await config.set(EXPERIMENTAL_SECTION, {
      obsolete_flag: false,
      example_flag: false,
    });

    expect(flags.enabled('example_flag')).toBe(false);
    expect(flags.explain('obsolete_flag')).toBeUndefined();
  });
});

/**
 * Mirri product flags — verify that the two flags ported from v1
 * (`tool-select`, `hook-command-rewrite`) are contributed via the
 * decentralized `registerFlagDefinition()` mechanism and resolve correctly
 * through env, config, and defaults.
 */
describe('Mirri product flags registration', () => {
  it('should contribute hook-command-rewrite into the import-time buffer', () => {
    const ids = getContributedFlags().map((d) => d.id);
    expect(ids).toContain(HOOK_COMMAND_REWRITE_FLAG_ID);
  });

  it('should contribute tool-select into the import-time buffer', () => {
    const ids = getContributedFlags().map((d) => d.id);
    expect(ids).toContain(TOOL_SELECT_FLAG_ID);
  });

  it('should match v1 contract for hook-command-rewrite definition', () => {
    const def = getContributedFlags().find((d) => d.id === HOOK_COMMAND_REWRITE_FLAG_ID);
    expect(def).toMatchObject({
      id: 'hook-command-rewrite',
      env: 'MIRRICODE_EXPERIMENTAL_HOOK_COMMAND_REWRITE',
      default: false,
      surface: 'core',
    });
    expect(def!.title).toBeTruthy();
    expect(def!.description).toBeTruthy();
  });

  it('should match v1 contract for tool-select definition', () => {
    const def = getContributedFlags().find((d) => d.id === TOOL_SELECT_FLAG_ID);
    expect(def).toMatchObject({
      id: 'tool-select',
      env: 'MIRRICODE_EXPERIMENTAL_TOOL_SELECT',
      default: false,
      surface: 'core',
    });
    expect(def!.title).toBeTruthy();
    expect(def!.description).toBeTruthy();
  });
});

describe('tool-select flag resolution (env + config + default)', () => {
  let disposables: DisposableStore;
  let homeDir: string;

  beforeEach(() => {
    disposables = new DisposableStore();
    homeDir = `/tmp/mirri-code-flag-toolselect-${process.pid}-${Date.now()}-${Math.random().toString(36).slice(2)}`;
  });
  afterEach(() => disposables.dispose());

  function makeFlags(env: Readonly<Record<string, string | undefined>> = {}) {
    const ix = disposables.add(new TestInstantiationService());
    ix.stub(IBootstrapService, stubBootstrap(homeDir, env));
    ix.stub(ILogService, stubLog());
    ix.stub(IFileSystemStorageService, new InMemoryStorageService());
    ix.set(IAtomicTomlDocumentStore, new SyncDescriptor(TomlAtomicDocumentStore));
    ix.set(IConfigRegistry, new SyncDescriptor(ConfigRegistry));
    ix.set(IConfigService, new SyncDescriptor(ConfigService));
    ix.set(IFlagRegistry, new SyncDescriptor(FlagRegistryService));
    ix.set(IFlagService, new SyncDescriptor(FlagService));
    // tool-select is already contributed at import time by
    // agent/toolSelect/flag.ts — FlagRegistryService drains it in its
    // constructor via getContributedFlags().
    return {
      config: ix.get(IConfigService),
      flags: ix.get(IFlagService),
    };
  }

  it('should default to false when no override is present', () => {
    const { flags } = makeFlags();
    expect(flags.enabled(TOOL_SELECT_FLAG_ID)).toBe(false);
    expect(flags.explain(TOOL_SELECT_FLAG_ID)?.source).toBe('default');
  });

  it('should enable via per-flag env variable', () => {
    const { flags } = makeFlags({ [TOOL_SELECT_FLAG_ENV]: 'true' });
    expect(flags.enabled(TOOL_SELECT_FLAG_ID)).toBe(true);
    expect(flags.explain(TOOL_SELECT_FLAG_ID)?.source).toBe('env');
  });

  it('should enable via config [experimental] section', async () => {
    const { config, flags } = makeFlags();
    await config.set(EXPERIMENTAL_SECTION, { [TOOL_SELECT_FLAG_ID]: true });
    expect(flags.enabled(TOOL_SELECT_FLAG_ID)).toBe(true);
    expect(flags.explain(TOOL_SELECT_FLAG_ID)?.source).toBe('config');
  });

  it('should let env override config', async () => {
    const { config, flags } = makeFlags({ [TOOL_SELECT_FLAG_ENV]: '1' });
    await config.set(EXPERIMENTAL_SECTION, { [TOOL_SELECT_FLAG_ID]: false });
    expect(flags.enabled(TOOL_SELECT_FLAG_ID)).toBe(true);
    expect(flags.explain(TOOL_SELECT_FLAG_ID)?.source).toBe('env');
  });

  it('should let master env switch force tool-select on', async () => {
    const { config, flags } = makeFlags({ [MASTER_ENV]: 'true' });
    await config.set(EXPERIMENTAL_SECTION, { [TOOL_SELECT_FLAG_ID]: false });
    expect(flags.enabled(TOOL_SELECT_FLAG_ID)).toBe(true);
    expect(flags.explain(TOOL_SELECT_FLAG_ID)?.source).toBe('master-env');
  });

  it('should disable via per-flag env when config says true', async () => {
    const { config, flags } = makeFlags({ [TOOL_SELECT_FLAG_ENV]: 'false' });
    await config.set(EXPERIMENTAL_SECTION, { [TOOL_SELECT_FLAG_ID]: true });
    expect(flags.enabled(TOOL_SELECT_FLAG_ID)).toBe(false);
    expect(flags.explain(TOOL_SELECT_FLAG_ID)?.source).toBe('env');
  });
});
