import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';

import {
  createMirriHarness,
  createMirriHarnessV2,
  type Event,
  type MirriHarness,
} from '@mirri-ai/mirri-code-sdk';

import type { FakeProviderServer } from '#/fake-provider-server';

// ---------------------------------------------------------------------------
// EngineKind — which engine implementation backs a fixture instance
// ---------------------------------------------------------------------------

/**
 * `'v1'` runs the legacy `createMirriHarness` core; `'v2'` runs the
 * agent-core-v2 bootstrap reached through `createMirriHarnessV2`. Both
 * factories return the same `MirriHarness` type surface, so a fixture
 * instance is engine-agnostic once it exists.
 */
export type EngineKind = 'v1' | 'v2';

// ---------------------------------------------------------------------------
// EngineFixture — engine-agnostic interface for e2e tests
// ---------------------------------------------------------------------------

export interface EngineFixture {
  readonly homeDir: string;
  readonly rawHarness: MirriHarness;

  createSession(opts: { id?: string; workDir: string; permission?: 'manual' | 'auto' | 'yolo' }): Promise<SessionFixture>;
  resumeSession(id: string): Promise<SessionFixture>;
  listSessions(): Promise<readonly SessionSummary[]>;
  close(): Promise<void>;
}

export interface SessionFixture {
  readonly id: string;
  readonly sessionDir: string;
  prompt(input: string | ContentPart[]): Promise<void>;
  steer(input: string): Promise<void>;
  cancel(): Promise<void>;
  onEvent(cb: (event: Event) => void): () => void;
  setApprovalHandler(cb: (req: ApprovalRequest) => Promise<ApprovalResponse>): void;
  setQuestionHandler(cb: (req: QuestionRequest) => Promise<QuestionResult>): void;
  setPermission(mode: 'manual' | 'auto' | 'yolo'): Promise<void>;
  waitForEvent(type: string, timeoutMs?: number): Promise<Event>;
  waitForTurnEnd(timeoutMs?: number): Promise<void>;
  close(): Promise<void>;
}

export interface SessionSummary {
  id: string;
  title: string;
  workDir: string;
}

export type ContentPart =
  | { type: 'text'; text: string }
  | { type: 'image_url'; imageUrl: { url: string } };

export interface ApprovalRequest {
  toolCallId: string;
  toolName: string;
  input: unknown;
}

export interface ApprovalResponse {
  decision: 'approved' | 'rejected';
  feedback?: string;
}

export interface QuestionRequest {
  questionId: string;
  question: string;
}

export interface QuestionResult {
  answer: string;
}

// ---------------------------------------------------------------------------
// Factory — createEngineFixture(options, engine)
// ---------------------------------------------------------------------------

export interface EngineFixtureOptions {
  readonly fakeProvider: FakeProviderServer;
  readonly homeDir?: string;
  readonly workDir?: string;
  readonly model?: string;
}

/**
 * @deprecated Old name of {@link EngineFixtureOptions}, kept for callers that
 * predate the v1/v2 engine parameter. Use `EngineFixtureOptions`.
 */
export type V1EngineFixtureOptions = EngineFixtureOptions;

/**
 * Boot an engine fixture on the requested engine. The v1 vs v2 difference is
 * contained here: both engines get the same fake OpenAI-compatible provider,
 * the same identity, and the same `defaultModel` → `fake-model` wiring.
 *
 * v1 configures the engine through `harness.setConfig(...)`. The v2 client's
 * `setConfig` is not migrated yet (falls through to `getRpc()` which throws
 * `NOT_IMPLEMENTED`), so the v2 branch writes the equivalent `config.toml`
 * directly to `$homeDir/config.toml` before the harness is constructed —
 * the file must exist before `createMirriHarnessV2` bootstraps the engine,
 * whose config hydration happens once, in the constructor.
 */
export async function createEngineFixture(
  options: EngineFixtureOptions,
  engine: EngineKind = 'v1',
): Promise<EngineFixture> {
  const homeDir = options.homeDir ?? (await makeTempDir());
  const model = options.model ?? 'fake-model';

  if (engine === 'v2') {
    await writeV2ProviderConfig({
      configPath: join(homeDir, 'config.toml'),
      fakeProvider: options.fakeProvider,
      model,
    });
  }

  const harness: MirriHarness =
    engine === 'v2'
      ? createMirriHarnessV2({
          identity: {
            userAgentProduct: 'mirri-code-e2e',
            version: '0.0.0-test',
          },
          homeDir,
        })
      : createMirriHarness({
          identity: {
            userAgentProduct: 'mirri-code-e2e',
            version: '0.0.0-test',
          },
          homeDir,
        });

  await harness.ensureConfigFile();

  if (engine === 'v1') {
    await harness.setConfig({
      providers: {
        local: {
          type: 'openai',
          apiKey: 'sk-test',
          baseUrl: `${options.fakeProvider.baseUrl}/v1`,
        },
      },
      models: {
        [model]: {
          provider: 'local',
          model,
          maxContextSize: 262_144,
        },
      },
      defaultModel: model,
    });
  }

  return new EngineFixtureImpl(harness, homeDir);
}

/** Legacy alias over `createEngineFixture(options, 'v1')` — unchanged behavior. */
export async function createV1EngineFixture(options: EngineFixtureOptions): Promise<EngineFixture> {
  return createEngineFixture(options, 'v1');
}

/** v2 twin of the v1 fixture — same fake provider, agent-core-v2 engine. */
export async function createV2EngineFixture(options: EngineFixtureOptions): Promise<EngineFixture> {
  return createEngineFixture(options, 'v2');
}

// ---------------------------------------------------------------------------
// v2 provider config — handwritten config.toml, since setConfig is not
// migrated on SDKRpcClientV2
// ---------------------------------------------------------------------------

/**
 * The agent-core-v2 engine reads snake_case sections from `config.toml`:
 * a `default_model` scalar pointer, the `[providers.<id>]` registry (type /
 * apiKey / baseUrl), and the `[models.<alias>]` table (provider pointer, wire
 * name, max context size). This mirrors exactly the camelCase patch the v1
 * branch passes to `harness.setConfig`.
 */
async function writeV2ProviderConfig(input: {
  readonly configPath: string;
  readonly fakeProvider: FakeProviderServer;
  readonly model: string;
}): Promise<void> {
  const configToml = [
    `default_model = "${input.model}"`,
    '',
    `[providers."local"]`,
    `type = "openai"`,
    `api_key = "sk-test"`,
    `base_url = "${input.fakeProvider.baseUrl}/v1"`,
    '',
    `[models."${input.model}"]`,
    `provider = "local"`,
    `model = "${input.model}"`,
    `max_context_size = 262144`,
    '',
  ].join('\n');
  await writeFile(input.configPath, configToml, 'utf8');
}

// ---------------------------------------------------------------------------
// EngineFixtureImpl — wraps the shared MirriHarness surface
// ---------------------------------------------------------------------------

class EngineFixtureImpl implements EngineFixture {
  constructor(
    private readonly harness: MirriHarness,
    readonly homeDir: string,
  ) {}

  get rawHarness(): MirriHarness {
    return this.harness;
  }

  async createSession(opts: { id?: string; workDir: string; permission?: 'manual' | 'auto' | 'yolo' }): Promise<SessionFixture> {
    const session = await this.harness.createSession({
      id: opts.id,
      workDir: opts.workDir,
      permission: opts.permission,
    });
    return new SessionFixtureImpl(session);
  }

  async resumeSession(id: string): Promise<SessionFixture> {
    const session = await this.harness.resumeSession({ id });
    return new SessionFixtureImpl(session);
  }

  async listSessions(): Promise<readonly SessionSummary[]> {
    const sessions = await this.harness.listSessions();
    return sessions.map((s) => ({
      id: s.id,
      title: s.title ?? '',
      workDir: s.workDir,
    }));
  }

  async close(): Promise<void> {
    await this.harness.close();
  }
}

// ---------------------------------------------------------------------------
// SessionFixtureImpl — wraps SDK Session
// ---------------------------------------------------------------------------

import type { Session } from '@mirri-ai/mirri-code-sdk';

class SessionFixtureImpl implements SessionFixture {
  private readonly eventListeners = new Set<(event: Event) => void>();
  private approvalHandler:
    | ((req: ApprovalRequest) => Promise<ApprovalResponse>)
    | null = null;
  private questionHandler:
    | ((req: QuestionRequest) => Promise<QuestionResult>)
    | null = null;

  constructor(private readonly session: Session) {
    session.onEvent((event: Event) => {
      for (const listener of this.eventListeners) {
        listener(event);
      }
    });
  }

  get id(): string {
    return this.session.id;
  }

  get sessionDir(): string {
    return this.session.summary?.sessionDir ?? '';
  }

  async prompt(input: string | ContentPart[]): Promise<void> {
    const content = typeof input === 'string' ? input : input;
    await this.session.prompt(content);
  }

  async steer(input: string): Promise<void> {
    await this.session.steer(input);
  }

  async cancel(): Promise<void> {
    await this.session.cancel();
  }

  onEvent(cb: (event: Event) => void): () => void {
    this.eventListeners.add(cb);
    return () => {
      this.eventListeners.delete(cb);
    };
  }

  setApprovalHandler(cb: (req: ApprovalRequest) => Promise<ApprovalResponse>): void {
    this.approvalHandler = cb;
    this.session.setApprovalHandler(async (request) => {
      if (this.approvalHandler === null) {
        throw new Error('No approval handler set');
      }
      const response = await this.approvalHandler({
        toolCallId: request.toolCallId,
        toolName: request.toolName,
        input: request,
      });
      return {
        decision: response.decision,
        feedback: response.feedback,
        scope: 'session',
      };
    });
  }

  setQuestionHandler(cb: (req: QuestionRequest) => Promise<QuestionResult>): void {
    this.questionHandler = cb;
    this.session.setQuestionHandler(async (request) => {
      if (this.questionHandler === null) {
        throw new Error('No question handler set');
      }
      const firstQuestion = request.questions[0];
      if (!firstQuestion) return null;
      const result = await this.questionHandler({
        questionId: request.toolCallId ?? firstQuestion.question,
        question: firstQuestion.question,
      });
      return { answers: { [firstQuestion.question]: result.answer } };
    });
  }

  async setPermission(mode: 'manual' | 'auto' | 'yolo'): Promise<void> {
    await this.session.setPermission(mode);
  }

  waitForEvent(type: string, timeoutMs = 10_000): Promise<Event> {
    return new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        this.eventListeners.delete(listener);
        reject(new Error(`Timed out waiting for event "${type}" after ${timeoutMs}ms`));
      }, timeoutMs);

      const listener = (event: Event): void => {
        if (event.type === type) {
          clearTimeout(timeout);
          this.eventListeners.delete(listener);
          resolve(event);
        }
      };
      this.eventListeners.add(listener);
    });
  }

  async waitForTurnEnd(timeoutMs = 10_000): Promise<void> {
    await this.waitForEvent('turn.ended', timeoutMs);
  }

  async close(): Promise<void> {
    await this.session.close();
  }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

export async function makeTempDir(): Promise<string> {
  return mkdtemp(join(tmpdir(), 'mirri-e2e-'));
}

export async function removeTempDir(dir: string): Promise<void> {
  for (let attempt = 0; attempt < 10; attempt += 1) {
    try {
      await rm(dir, { recursive: true, force: true });
      return;
    } catch (error) {
      const code = (error as NodeJS.ErrnoException).code;
      if (code !== 'ENOTEMPTY' && code !== 'EBUSY' && code !== 'EPERM') {
        throw error;
      }
      await delay(10);
    }
  }
  await rm(dir, { recursive: true, force: true });
}