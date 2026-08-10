import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';

import { createMirriHarness, type Event, type MirriHarness } from '@mirri-ai/mirri-code-sdk';

import type { FakeProviderServer } from '#/fake-provider-server';

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
// V1EngineFixture — wraps createMirriHarness + FakeProviderServer
// ---------------------------------------------------------------------------

export interface V1EngineFixtureOptions {
  readonly fakeProvider: FakeProviderServer;
  readonly homeDir?: string;
  readonly workDir?: string;
  readonly model?: string;
}

export async function createV1EngineFixture(
  options: V1EngineFixtureOptions,
): Promise<EngineFixture> {
  const homeDir = options.homeDir ?? (await makeTempDir());
  const model = options.model ?? 'fake-model';

  const harness = createMirriHarness({
    identity: {
      userAgentProduct: 'mirri-code-e2e',
      version: '0.0.0-test',
    },
    homeDir,
  });

  await harness.ensureConfigFile();
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

  return new V1EngineFixture(harness, homeDir);
}

class V1EngineFixture implements EngineFixture {
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
    return new V1SessionFixture(session);
  }

  async resumeSession(id: string): Promise<SessionFixture> {
    const session = await this.harness.resumeSession({ id });
    return new V1SessionFixture(session);
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
// V1SessionFixture — wraps SDK Session
// ---------------------------------------------------------------------------

import type { Session } from '@mirri-ai/mirri-code-sdk';

class V1SessionFixture implements SessionFixture {
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
