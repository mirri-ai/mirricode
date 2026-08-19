/**
 * node-sdk e2e: question reverse-RPC — an `AskUserQuestion` tool call driven
 * by `setQuestionHandler` across both engines.
 *
 * The engine asks the user through a reverse RPC (v1: `requestQuestion` on the
 * RPC client; v2: the interaction bridge in `session-wiring.ts`). The handler's
 * answer is fed back to the model as the tool result, and the turn completes.
 *
 * Scripted provider flow:
 *   1. user prompt            → model tool-calls `AskUserQuestion`
 *   2. approval handler       → approve (manual permission mode)
 *   3. question handler       → answers the question
 *   4. tool result            → model answers with a final text → turn.ended
 */

import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  type EngineFixture,
  type EngineKind,
  type FakeProviderServer,
  createEngineFixture,
  createFakeProviderServer,
  makeTempDir,
  removeTempDir,
} from '@mirri-ai/e2e-harness';

import type { Event } from '#/index';

const QUESTION_TEXT = 'Which test runner should the project use?';
const ANSWER_LABEL = 'Vitest';

const ASK_USER_QUESTION_TOOL_CALL = {
  questions: [
    {
      question: QUESTION_TEXT,
      header: 'Test runner',
      options: [
        { label: 'Vitest', description: 'Fast and modern ESM' },
        { label: 'Jest', description: 'Classic and battle-tested' },
      ],
      multi_select: false,
    },
  ],
};

describe.each(['v1', 'v2'] as const)(
  'node-sdk e2e: question reverse-RPC via AskUserQuestion (engine=%s)',
  (engine: EngineKind) => {
    let fakeProvider: FakeProviderServer;
    let fixture: EngineFixture;
    let workDir: string;
    const tempDirs: string[] = [];

    beforeEach(async () => {
      fakeProvider = await createFakeProviderServer();
      const homeDir = await makeTempDir();
      workDir = await makeTempDir();
      tempDirs.push(homeDir, workDir);
      fixture = await createEngineFixture({ fakeProvider, homeDir }, engine);
    });

    afterEach(async () => {
      await fixture.close();
      await fakeProvider.close();
      for (const dir of tempDirs.splice(0)) {
        await removeTempDir(dir);
      }
    });

    it('given a provider script asking a question, when the question handler answers, should feed the chosen answer back to the model and complete the turn', async () => {
      fakeProvider.nextToolCall('AskUserQuestion', ASK_USER_QUESTION_TOOL_CALL);
      fakeProvider.nextText(`The project will use ${ANSWER_LABEL}.`);

      const harness = fixture.rawHarness;
      const session = await harness.createSession({ workDir, permission: 'manual' });

      const events: Event[] = [];
      session.onEvent((event) => events.push(event));

      // Auto-approve any permission gate (manual mode) so the ask-user tool
      // always reaches the question stage. Both engines run AskUserQuestion
      // without an approval request in manual mode — this handler is a belt
      // and suspenders line, not part of the question contract.
      session.setApprovalHandler(() => ({ decision: 'approved' as const }));

      let askedQuestion: string | undefined;
      let answerCount = 0;
      session.setQuestionHandler((request) => {
        answerCount += 1;
        askedQuestion = request.questions[0]?.question;
        return { [request.questions[0]!.question]: ANSWER_LABEL };
      });

      await session.prompt('ask me which test runner to use');

      await new Promise<void>((resolve, reject) => {
        const timeout = setTimeout(() => reject(new Error('timeout waiting for turn.ended')), 10_000);
        const unsub = session.onEvent((event) => {
          if (event.type === 'turn.ended') {
            clearTimeout(timeout);
            unsub();
            resolve();
          }
        });
      });

      // The question reached the handler with the scripted question text.
      expect(answerCount).toBe(1);
      expect(askedQuestion).toBe(QUESTION_TEXT);

      // The engine made the second provider request with the tool result.
      expect(fakeProvider.requests).toHaveLength(2);

      // The answer travelled back to the model: the second request body
      // carries a `tool` message with the ask-user tool result JSON.
      const secondBody = fakeProvider.requests[1]!.bodyJson as {
        messages?: Array<{ role?: string; content?: unknown }>;
      };
      const secondMessages = secondBody?.messages ?? [];
      const toolMessages = secondMessages.filter((message) => message.role === 'tool');
      expect(toolMessages.length).toBeGreaterThan(0);

      const answeredToolContent = JSON.stringify(toolMessages.map((message) => message.content));
      // The tool result payload is the ask-user answer JSON, e.g.
      // `{"answers":{"Which test runner...?":"Vitest"}}` (inner quotes are
      // backslash-escaped by the outer stringify).
      expect(answeredToolContent).toContain('answers');
      expect(answeredToolContent).toContain(ANSWER_LABEL);

      // The answered question is part of the resumed conversation.
      const turnEnded = events.find(
        (event) => event.type === 'turn.ended',
      ) as Extract<Event, { type: 'turn.ended' }> | undefined;
      expect(turnEnded?.reason).toBe('completed');

      const deltas = events
        .filter((event): event is Extract<Event, { type: 'assistant.delta' }> => event.type === 'assistant.delta')
        .map((event) => event.delta)
        .join('');
      expect(deltas).toContain(ANSWER_LABEL);

      await session.close();
    });
  },
);