import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  type FakeProviderServer,
  createFakeProviderServer,
} from '#/index';

describe('FakeProviderServer', () => {
  let server: FakeProviderServer;

  beforeEach(async () => {
    server = await createFakeProviderServer();
  });

  afterEach(async () => {
    await server.close();
  });

  it('should start and accept requests on an ephemeral port', async () => {
    expect(server.baseUrl).toMatch(/^http:\/\/127\.0\.0\.1:\d+$/);
  });

  it('should return scripted text response as OpenAI SSE', async () => {
    server.nextText('hello world');

    const response = await fetch(`${server.baseUrl}/v1/chat/completions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ model: 'fake-model', messages: [] }),
    });

    expect(response.status).toBe(200);
    expect(response.headers.get('content-type')).toContain('text/event-stream');

    const text = await response.text();
    expect(text).toContain('data: ');
    expect(text).toContain('hello world');
    expect(text).toContain('[DONE]');

    expect(server.requests).toHaveLength(1);
    expect(server.requests[0]?.method).toBe('POST');
    expect(server.requests[0]?.pathname).toBe('/v1/chat/completions');
  });

  it('should return scripted tool call response', async () => {
    server.nextToolCall('bash', { command: 'echo hi' });

    const response = await fetch(`${server.baseUrl}/v1/chat/completions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ model: 'fake-model', messages: [] }),
    });

    const text = await response.text();
    expect(text).toContain('tool_calls');
    expect(text).toContain('"finish_reason":"tool_calls"');
  });

  it('should return scripted error response', async () => {
    server.nextError(429, { error: { message: 'rate limited', type: 'rate_limit' } });

    const response = await fetch(`${server.baseUrl}/v1/chat/completions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ model: 'fake-model', messages: [] }),
    });

    expect(response.status).toBe(429);
    const body = (await response.json()) as { error: { message: string } };
    expect(body.error.message).toBe('rate limited');
  });

  it('should return 500 when no response is queued', async () => {
    const response = await fetch(`${server.baseUrl}/v1/chat/completions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ model: 'fake-model', messages: [] }),
    });

    expect(response.status).toBe(500);
  });

  it('should queue multiple responses in FIFO order', async () => {
    server.nextText('first');
    server.nextText('second');

    const res1 = await fetch(`${server.baseUrl}/v1/chat/completions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ messages: [] }),
    });
    const text1 = await res1.text();
    expect(text1).toContain('first');

    const res2 = await fetch(`${server.baseUrl}/v1/chat/completions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ messages: [] }),
    });
    const text2 = await res2.text();
    expect(text2).toContain('second');
  });

  it('should report cache bucket usage on the final SSE chunk when usage detail is set', async () => {
    // Given a scripted response served after a usage detail with a non-zero
    // prompt-cache bucket was installed
    server.setUsage({ promptTokens: 10, completionTokens: 5, cachedTokens: 4 });
    server.nextText('hello');

    const response = await fetch(`${server.baseUrl}/v1/chat/completions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ messages: [] }),
    });
    const text = await response.text();

    // Then the final chunk's usage object carries the OpenAI
    // prompt_tokens_details shape the rust OpenAI dispatcher folds into
    // cache_read_tokens
    expect(text).toContain(
      '"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":4}}',
    );
  });

  it('should omit the cache bucket when no usage detail is set', async () => {
    server.nextText('hello');

    const response = await fetch(`${server.baseUrl}/v1/chat/completions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ messages: [] }),
    });
    const text = await response.text();

    // Then the default usage keeps the historical cache-less shape — the
    // new field is strictly additive
    expect(text).toContain('"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}');
    expect(text).not.toContain('prompt_tokens_details');
  });
});
