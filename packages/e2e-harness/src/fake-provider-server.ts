import * as node_http from 'node:http';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface RecordedRequest {
  readonly index: number;
  readonly method: string;
  readonly url: string;
  readonly pathname: string;
  readonly headers: Record<string, string>;
  readonly bodyText: string;
  readonly bodyJson: unknown;
}

// ---------------------------------------------------------------------------
// Scripted response builders — what the fake LLM should return
// ---------------------------------------------------------------------------

export type ScriptedResponse =
  | { kind: 'text'; text: string }
  | { kind: 'think_then_text'; think: string; text: string }
  | { kind: 'tool_call'; name: string; input: object; textBefore?: string }
  | { kind: 'error'; status: number; body: object }
  | { kind: 'raw_sse'; events: unknown[] };

// ---------------------------------------------------------------------------
// FakeProviderServer
// ---------------------------------------------------------------------------

export interface FakeProviderServer {
  readonly baseUrl: string;
  readonly requests: RecordedRequest[];
  nextText(text: string): void;
  nextThinkThenText(think: string, text: string): void;
  nextToolCall(name: string, input: object, textBefore?: string): void;
  nextError(status: number, body: object): void;
  nextRawSSE(events: unknown[]): void;
  nextScript(script: Array<{ type: string; text?: string; name?: string; input?: object }>): void;
  setStreamDelay(ms: number): void;
  close(): Promise<void>;
}

export async function createFakeProviderServer(): Promise<FakeProviderServer> {
  const requests: RecordedRequest[] = [];
  const responseQueue: ScriptedResponse[] = [];
  let streamDelayMs = 0;

  const server = node_http.createServer((req, res) => {
    void (async () => {
      const method = (req.method ?? 'POST').toUpperCase();
      const requestUrl = new URL(req.url ?? '/', 'http://127.0.0.1');

      const chunks: Buffer[] = [];
      for await (const chunk of req) {
        chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
      }
      const bodyText = Buffer.concat(chunks).toString('utf8');
      let bodyJson: unknown = null;
      if (bodyText.length > 0) {
        try {
          bodyJson = JSON.parse(bodyText);
        } catch {
          bodyJson = null;
        }
      }

      const request: RecordedRequest = {
        index: requests.length,
        method,
        url: requestUrl.toString(),
        pathname: requestUrl.pathname,
        headers: normalizeHeaders(req.headers),
        bodyText,
        bodyJson,
      };
      requests.push(request);

      const response = responseQueue.shift();
      if (response === undefined) {
        sendJson(res, 500, {
          error: {
            message: 'No scripted response queued. Queue more responses with nextText/nextToolCall/etc.',
            type: 'no_response',
          },
        });
        return;
      }

      if (streamDelayMs > 0) {
        await new Promise((resolve) => setTimeout(resolve, streamDelayMs));
      }

      switch (response.kind) {
        case 'text':
          sendOpenAISSE(res, buildTextChunks(response.text));
          break;
        case 'think_then_text':
          sendOpenAISSE(res, buildThinkThenTextChunks(response.think, response.text));
          break;
        case 'tool_call':
          sendOpenAISSE(res, buildToolCallChunks(response.name, response.input, response.textBefore));
          break;
        case 'error':
          sendJson(res, response.status, response.body);
          break;
        case 'raw_sse':
          sendOpenAISSE(res, response.events);
          break;
      }
    })();
  });

  await new Promise<void>((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });

  const address = server.address();
  if (address === null || typeof address === 'string') {
    throw new Error('FakeProviderServer failed to bind a TCP port.');
  }

  return {
    get baseUrl() {
      return `http://127.0.0.1:${address.port}`;
    },
    requests,
    nextText(text: string): void {
      responseQueue.push({ kind: 'text', text });
    },
    nextThinkThenText(think: string, text: string): void {
      responseQueue.push({ kind: 'think_then_text', think, text });
    },
    nextToolCall(name: string, input: object, textBefore?: string): void {
      responseQueue.push({ kind: 'tool_call', name, input, textBefore });
    },
    nextError(status: number, body: object): void {
      responseQueue.push({ kind: 'error', status, body });
    },
    nextRawSSE(events: unknown[]): void {
      responseQueue.push({ kind: 'raw_sse', events });
    },
    nextScript(
      script: Array<{ type: string; text?: string; name?: string; input?: object }>,
    ): void {
      for (const step of script) {
        if (step.type === 'text') {
          responseQueue.push({ kind: 'text', text: step.text ?? '' });
        } else if (step.type === 'tool_call') {
          responseQueue.push({ kind: 'tool_call', name: step.name ?? 'Tool', input: step.input ?? {} });
        } else if (step.type === 'think_then_text') {
          responseQueue.push({ kind: 'think_then_text', think: step.text ?? '', text: '' });
        } else if (step.type === 'error') {
          responseQueue.push({ kind: 'error', status: 500, body: { error: { message: step.text ?? 'error' } } });
        }
      }
    },
    setStreamDelay(ms: number): void {
      streamDelayMs = ms;
    },
    async close(): Promise<void> {
      await new Promise<void>((resolve, reject) => {
        server.close((error?: Error) => {
          if (error !== undefined) {
            reject(error);
            return;
          }
          resolve();
        });
      });
    },
  };
}

// ---------------------------------------------------------------------------
// OpenAI SSE chunk builders
// ---------------------------------------------------------------------------

const CHATCMPL_ID = 'chatcmpl-fake';
const MODEL = 'fake-model';

function buildTextChunks(text: string): unknown[] {
  return [
    {
      id: CHATCMPL_ID,
      object: 'chat.completion.chunk',
      created: 1_700_000_000,
      model: MODEL,
      choices: [{ index: 0, delta: { content: text }, finish_reason: null }],
    },
    {
      id: CHATCMPL_ID,
      object: 'chat.completion.chunk',
      created: 1_700_000_000,
      model: MODEL,
      choices: [{ index: 0, delta: {}, finish_reason: 'stop' }],
      usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 },
    },
  ];
}

function buildThinkThenTextChunks(think: string, text: string): unknown[] {
  return [
    {
      id: CHATCMPL_ID,
      object: 'chat.completion.chunk',
      created: 1_700_000_000,
      model: MODEL,
      choices: [{ index: 0, delta: { reasoning_content: think }, finish_reason: null }],
    },
    {
      id: CHATCMPL_ID,
      object: 'chat.completion.chunk',
      created: 1_700_000_000,
      model: MODEL,
      choices: [{ index: 0, delta: { content: text }, finish_reason: null }],
    },
    {
      id: CHATCMPL_ID,
      object: 'chat.completion.chunk',
      created: 1_700_000_000,
      model: MODEL,
      choices: [{ index: 0, delta: {}, finish_reason: 'stop' }],
      usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 },
    },
  ];
}

function buildToolCallChunks(name: string, input: object, textBefore?: string): unknown[] {
  const args = JSON.stringify(input);
  const chunks: unknown[] = [];

  if (textBefore !== undefined && textBefore.length > 0) {
    chunks.push({
      id: CHATCMPL_ID,
      object: 'chat.completion.chunk',
      created: 1_700_000_000,
      model: MODEL,
      choices: [{ index: 0, delta: { content: textBefore }, finish_reason: null }],
    });
  }

  // First tool-call chunk: includes id + name + start of arguments
  chunks.push({
    id: CHATCMPL_ID,
    object: 'chat.completion.chunk',
    created: 1_700_000_000,
    model: MODEL,
    choices: [
      {
        index: 0,
        delta: {
          tool_calls: [
            {
              index: 0,
              id: `call_${name}`,
              type: 'function',
              function: { name, arguments: args },
            },
          ],
        },
        finish_reason: null,
      },
    ],
  });

  // Final chunk: finish_reason = tool_calls
  chunks.push({
    id: CHATCMPL_ID,
    object: 'chat.completion.chunk',
    created: 1_700_000_000,
    model: MODEL,
    choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }],
    usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 },
  });

  return chunks;
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

function normalizeHeaders(headers: node_http.IncomingHttpHeaders): Record<string, string> {
  const normalized: Record<string, string> = {};
  for (const [key, value] of Object.entries(headers)) {
    if (value === undefined) continue;
    normalized[key.toLowerCase()] = Array.isArray(value) ? value.join(', ') : value;
  }
  return normalized;
}

function sendJson(res: node_http.ServerResponse, status: number, body: unknown): void {
  const json = JSON.stringify(body);
  res.writeHead(status, { 'content-type': 'application/json; charset=utf-8' });
  res.end(json);
}

function sendOpenAISSE(res: node_http.ServerResponse, events: unknown[]): void {
  res.writeHead(200, {
    'cache-control': 'no-cache',
    connection: 'keep-alive',
    'content-type': 'text/event-stream; charset=utf-8',
  });

  for (const event of events) {
    res.write(`data: ${JSON.stringify(event)}\n\n`);
  }
  res.write('data: [DONE]\n\n');
  res.end();
}
