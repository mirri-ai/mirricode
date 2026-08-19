/**
 * F4 mirri TDD harness — boots `bootstrap()` in-process, wires
 * `@mirri-ai/klient/memory` on top, and exposes a typed facade for
 * feature-phase tests.
 *
 * ## Usage (feature-phase agents copy this)
 *
 * ```ts
 * import { harness } from '../_mirri-harness';
 *
 * const ctx = await harness();
 * ctx.stub.nextTextResponse('Hello!');
 * const session = await ctx.session();
 * const result = await session.agent('main').prompt({
 *   input: [{ type: 'text', text: 'hi' }],
 * });
 * // ... assert result ...
 * ctx.cleanup();
 * ```
 *
 * ## What is stubbed
 *
 * - **IFileSystemStorageService** → `InMemoryStorageService` (no disk I/O)
 * - **IProtocolAdapterRegistry** → provider stub (canned LLM responses)
 * - **IConfigService** → in-memory config with a test provider/model
 * - **IModelOAuthTokens** → rejects (no real OAuth in tests)
 * - **IHostEnvironment** → Linux/x64 stub
 * - **ILogOptions** → level 'off'
 * - **IAppendLogStore** → in-memory (no journal files)
 * - **IBlobStore** → in-memory
 *
 * Everything else (DI scopes, wire service, agent lifecycle, workspace
 * lifecycle, session lifecycle) uses the REAL production code — that is
 * the point: the harness proves the engine works in-process.
 */

/**
 * Import the barrel to ensure all `registerScopedService` side effects load.
 * Without this, the scoped registry is empty and `bootstrap()` cannot find
 * services like `IProviderService`, `IModelService`, etc.
 */
import '#/index';

import { Emitter, Event } from '#/_base/event';
import { SyncDescriptor } from '#/_base/di/descriptors';
import {
  bootstrap,
  type BootstrapInput,
  type BootstrapResult,
  resolveHostArgs,
  stubClientIdentity,
} from '#/app/bootstrap/bootstrap';
import type { IBootstrapService } from '#/app/bootstrap/bootstrap';
import {
  InMemoryStorageService,
  IFileSystemStorageService,
} from '#/persistence/interface/storage';
import { IConfigService } from '#/app/config/config';
import { IAppendLogStore } from '#/persistence/interface/appendLogStore';
import { IAtomicDocumentStore } from '#/persistence/interface/atomicDocumentStore';
import { IBlobStore, BlobStoreService } from '#/persistence/interface/blobStore';
import { ILogService } from '#/_base/log/log';
import { ILogOptions } from '#/_base/log/logConfig';
import { IHostEnvironment } from '#/os/interface/hostEnvironment';
import { IModelOAuthTokens } from '#/kosong/model/modelOAuth';
import { IHostTerminalService } from '#/os/interface/terminal';
import { IProtocolAdapterRegistry } from '#/kosong/protocol/protocol';
import { IProviderService, type ProviderConfig, type ProvidersSection } from '#/kosong/provider/provider';
import { IModelService, type ModelsSection } from '#/kosong/model/model';
// Side-effect imports: these modules call registerScopedService() at load time.
// Importing only the type/interface barrels above does NOT trigger registration,
// so the DI container would throw "unknown service 'providerService'" without these.
import '#/kosong/provider/providerService';
import '#/kosong/model/modelService';
import {
  DEFAULT_MODEL_SECTION,
  DEFAULT_PROVIDER_SECTION,
  MODELS_SECTION,
  PROVIDERS_SECTION,
} from '#/app/kosongConfig/configSection';
import { configWithEnvOverrides } from '#/app/kosongConfig/envOverlay';

import { createKlient, type MemoryKlientOptions } from '@mirri-ai/klient/memory';
import type { Klient, SessionHandle, AgentHandle } from '@mirri-ai/klient/core/klient';
import type { SessionMeta } from '#/session/sessionMetadata/sessionMetadata';

import { createTempHome, type TempHome } from './temp-home';
import { createProviderStub, type ProviderStub } from './provider-stub';

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

export interface MirriHarness {
  /** The bootstrapped engine (App scope). Dispose via `cleanup()`. */
  readonly core: BootstrapResult;
  /** The klient facade wired to the engine over the memory transport. */
  readonly klient: Klient;
  /** The provider stub — queue canned LLM responses before each turn. */
  readonly stub: ProviderStub;
  /** The temporary home directory (auto-cleaned on `cleanup()`). */
  readonly home: TempHome;

  /**
   * Create a session rooted at `workDir` and return a handle.
   * Convenience wrapper around `klient.global.sessions.create()`.
   */
  session(input?: { workDir?: string; title?: string }): Promise<SessionHandle>;

  /** Dispose the engine scope and remove the temp home directory. */
  cleanup(): void;
}

export interface HarnessOptions {
  /** Override the work directory for sessions. Defaults to `home.homeDir`. */
  readonly workDir?: string;
  /** Custom BootstrapInput fields (merged with defaults). */
  readonly bootstrapOverrides?: Partial<BootstrapInput>;
}

/**
 * Create the mirri TDD harness. Boots `bootstrap()` in-process with
 * InMemoryStorage and a provider stub, then wires `klient/memory`.
 *
 * Call `cleanup()` when the test finishes to dispose the engine and
 * remove the temporary home directory.
 */
export function harness(opts?: HarnessOptions): MirriHarness {
  const home = createTempHome('mirri-v2-harness');
  const stub = createProviderStub();

  const homeDir = home.homeDir;
  const workDir = opts?.workDir ?? homeDir;

  // Build the config object that the in-memory IConfigService reads.
  const testConfig = makeTestConfig();

  // Assemble the extra seeds that bootstrap() will layer onto the App scope.
  // These override or supplement what bootstrap() itself seeds.
  const extraSeeds = [
    // In-memory storage — no disk I/O
    [IFileSystemStorageService as unknown as ServiceId, new SyncDescriptor(InMemoryStorageService, [])] as const,
    [IBlobStore as unknown as ServiceId, new SyncDescriptor(BlobStoreService)] as const,
    // Provider stub — canned LLM responses
    stub.seed,
    // In-memory config with test provider/model
    [IConfigService as unknown as ServiceId, makeConfigService(testConfig)] as const,
    // In-memory append-log (no journal files)
    [IAppendLogStore as unknown as ServiceId, makeInMemoryAppendLog()] as const,
    [IAtomicDocumentStore as unknown as ServiceId, makeInMemoryAtomicDocument()] as const,
    // No real OAuth in tests
    [IModelOAuthTokens as unknown as ServiceId, {
      _serviceBrand: undefined,
      hasCachedAccessToken: () => Promise.resolve(false),
      getAccessToken: () => Promise.reject(new Error('IModelOAuthTokens not available in test harness')),
    }] as const,
    // Log level off
    [ILogOptions as unknown as ServiceId, {
      level: 'off',
      globalLogPath: `${homeDir}/logs/mirri-code.log`,
      globalMaxBytes: 6 * 1024 * 1024,
      globalFiles: 1,
      sessionMaxBytes: 5 * 1024 * 1024,
      sessionFiles: 1,
    }] as const,
    // Host environment stub
    [IHostEnvironment as unknown as ServiceId, {
      _serviceBrand: undefined,
      osKind: 'Linux',
      osArch: 'x64',
      osVersion: 'test',
      shellName: 'bash',
      shellPath: '/bin/bash',
      pathClass: 'posix',
      homeDir,
      ready: Promise.resolve(),
    }] as const,
    // Host terminal stub
    [IHostTerminalService as unknown as ServiceId, makeHostTerminalService()] as const,
    // Log service stub
    [ILogService as unknown as ServiceId, makeLogService()] as const,
  ];

  const input: BootstrapInput = {
    homeDir,
    clientIdentity: stubClientIdentity,
    args: {
      requestHeaders: {},
      displayName: 'mirri-test',
    },
    ...opts?.bootstrapOverrides,
  };

  const core = bootstrap(input, extraSeeds);

  // Hydrate the kosong registries from the test config so
  // IProviderService/IModelService resolve before any session is created.
  const configSvc = core.app.accessor.get(IConfigService);
  core.app.accessor.get(IProviderService).loadAll(
    configSvc.get<ProvidersSection>(PROVIDERS_SECTION) ?? {},
    configSvc.get<string>(DEFAULT_PROVIDER_SECTION),
  );
  core.app.accessor.get(IModelService).loadAll(
    configSvc.get<ModelsSection>(MODELS_SECTION) ?? {},
    configSvc.get<string>(DEFAULT_MODEL_SECTION),
  );

  const klient = createKlient({ scope: core.app, validate: false });

  let cleaned = false;

  return {
    core,
    klient,
    stub,
    home,
    session: async (input?: { workDir?: string; title?: string }) => {
      const meta = await klient.global.sessions.create({
        workDir: input?.workDir ?? workDir,
        title: input?.title,
      });
      return klient.session(meta.id);
    },
    cleanup: () => {
      if (cleaned) return;
      cleaned = true;
      core.app.dispose();
      home.cleanup();
    },
  };
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/** Type-erased ServiceIdentifier for seed tuples. */
type ServiceId = { _serviceBrand: undefined };

/** The test provider config — mirrors the existing harness MOCK_PROVIDER. */
const TEST_PROVIDER = {
  type: 'kimi',
  apiKey: 'test-key',
  baseUrl: 'https://api.example.test/v1',
  model: 'mock-model',
} as const;

const TEST_PROVIDER_NAME = 'test-provider';

function makeTestConfig(): Record<string, unknown> {
  return {
    providers: {
      [TEST_PROVIDER_NAME]: {
        type: TEST_PROVIDER.type,
        apiKey: TEST_PROVIDER.apiKey,
        baseUrl: TEST_PROVIDER.baseUrl,
      } satisfies ProviderConfig,
    },
    models: {
      [TEST_PROVIDER.model]: {
        provider: TEST_PROVIDER_NAME,
        model: TEST_PROVIDER.model,
        maxContextSize: 1_000_000,
      },
    },
    defaultProvider: TEST_PROVIDER_NAME,
    defaultModel: TEST_PROVIDER.model,
  };
}

/**
 * In-memory IConfigService that reads from a plain object.
 * Mirrors the pattern in the existing test harness (agent.ts).
 */
function makeConfigService(readConfig: Record<string, unknown>): IConfigService {
  const effectiveConfig = () => {
    const effective = { ...configWithEnvOverrides(readConfig as never) } as Record<string, unknown>;
    return effective;
  };
  const memory = new Map<string, unknown>();
  const sectionEmitter = new Emitter<{
    readonly domain: string;
    readonly source: 'set';
    readonly value: unknown;
    readonly previousValue: unknown;
  }>();
  const valueFor = (domain: string): unknown =>
    memory.has(domain)
      ? memory.get(domain)
      : (effectiveConfig() as Record<string, unknown>)[domain];

  return {
    _serviceBrand: undefined,
    ready: Promise.resolve(),
    onDidChangeConfiguration: () => ({ dispose() {} }),
    onDidSectionChange: sectionEmitter.event,
    get: <T>(domain: string) => valueFor(domain) as T,
    inspect: (domain: string) => {
      const value = (effectiveConfig() as Record<string, unknown>)[domain];
      return { value, defaultValue: undefined, userValue: undefined, memoryValue: value };
    },
    getAll: () => effectiveConfig() as never,
    set: (domain: string, patch: unknown) => {
      const current = valueFor(domain);
      const value =
        typeof current === 'object' && current !== null && typeof patch === 'object' && patch !== null
          ? { ...current, ...patch }
          : patch;
      const previousValue = current;
      memory.set(domain, value);
      sectionEmitter.fire({ domain, source: 'set', value, previousValue });
      return Promise.resolve();
    },
    replace: (domain: string, value: unknown) => {
      const previousValue = valueFor(domain);
      memory.set(domain, value);
      sectionEmitter.fire({ domain, source: 'set', value, previousValue });
      return Promise.resolve();
    },
    reload: () => Promise.resolve(),
    diagnostics: () => [],
  } as unknown as IConfigService;
}

function makeInMemoryAppendLog(): IAppendLogStore {
  const logs = new Map<string, unknown[]>();
  return {
    _serviceBrand: undefined,
    append(scope: string, key: string, record: unknown): void {
      const k = `${scope}/${key}`;
      const list = logs.get(k) ?? [];
      list.push(record);
      logs.set(k, list);
    },
    async *read<R>(scope: string, key: string): AsyncIterable<R> {
      const k = `${scope}/${key}`;
      for (const record of logs.get(k) ?? []) {
        yield record as R;
      }
    },
    rewrite<R>(scope: string, key: string, records: readonly R[]): Promise<void> {
      logs.set(`${scope}/${key}`, [...records]);
      return Promise.resolve();
    },
    flush(): Promise<void> { return Promise.resolve(); },
    close(): Promise<void> { return Promise.resolve(); },
    acquire(): { dispose(): void } { return { dispose() {} }; },
  };
}

function makeInMemoryAtomicDocument(): IAtomicDocumentStore {
  const docs = new Map<string, unknown>();
  return {
    _serviceBrand: undefined,
    async get<R>(scope: string, key: string): Promise<R | undefined> {
      return docs.get(`${scope}/${key}`) as R | undefined;
    },
    async set(scope: string, key: string, value: unknown): Promise<void> {
      docs.set(`${scope}/${key}`, value);
    },
    async delete(scope: string, key: string): Promise<void> {
      docs.delete(`${scope}/${key}`);
    },
    async list(scope: string): Promise<readonly string[]> {
      const prefix = `${scope}/`;
      return [...docs.keys()].filter((k) => k.startsWith(prefix)).map((k) => k.slice(prefix.length));
    },
    watch(): Event<void> { return Event.None; },
    acquire(): { dispose(): void } { return { dispose() {} }; },
  };
}

function makeHostTerminalService(): IHostTerminalService {
  return {
    _serviceBrand: undefined,
    onDidWriteData: Event.None as never,
    createTerminal: () => { throw new Error('ITerminalService.createTerminal not available in test harness'); },
  };
}

function makeLogService(): ILogService {
  const noop = () => {};
  return {
    _serviceBrand: undefined,
    trace: noop,
    debug: noop,
    info: noop,
    warn: noop,
    error: noop,
    critical: noop,
    flush: () => {},
    createChild: () => makeLogService(),
  } as unknown as ILogService;
}

// Re-export for convenience
export { createProviderStub, type ProviderStub, type ToolCallDeclaration, type StubResponseOptions } from './provider-stub';
export { createTempHome, type TempHome } from './temp-home';
