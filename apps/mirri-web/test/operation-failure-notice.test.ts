// apps/mirri-web/test/operation-failure-notice.test.ts
// Pure-logic tests for the operation-failure toast copy assembly
// (`src/lib/operationFailureNotice.ts`): the network/api/other
// classification and the default title/message fallback selection. The
// details half of the toast stays in `useMirriWebClient.ts` (it reads live
// connection/session state), so it is not covered here.

import { describe, expect, it } from 'vitest';

import { DaemonApiError, DaemonNetworkError } from '../src/api/errors';
import { classifyOperationFailure, operationFailureCopy } from '../src/lib/operationFailureNotice';

/** Stub translator that echoes the key — keeps the module's i18n-independence assertable. */
const t = (key: string): string => `T:${key}`;

const networkError = () =>
  new DaemonNetworkError({
    message: 'fetch failed',
    cause: new Error('ECONNREFUSED'),
    method: 'POST',
    path: '/api/v1/agents',
    url: 'http://127.0.0.1:58627/api/v1/agents',
    requestId: 'req-1',
    phase: 'fetch',
    timeoutMs: 15000,
  });

const apiError = () =>
  new DaemonApiError({
    code: 40001,
    msg: 'name must be kebab-case (lowercase alphanumeric segments separated by single hyphens, e.g. "code-reviewer")',
    requestId: 'req-2',
    details: { field: 'name' },
  });

describe('classifyOperationFailure', () => {
  it('given a daemon network error, when classifying, then it returns network', () => {
    expect(classifyOperationFailure(networkError())).toBe('network');
  });

  it('given a daemon api error, when classifying, then it returns api', () => {
    expect(classifyOperationFailure(apiError())).toBe('api');
  });

  it('given any other error, when classifying, then it returns other', () => {
    expect(classifyOperationFailure(new Error('boom'))).toBe('other');
    expect(classifyOperationFailure(undefined)).toBe('other');
  });
});

describe('operationFailureCopy', () => {
  it('given a network failure without overrides, when assembling copy, then it uses the daemon network title and message', () => {
    const copy = operationFailureCopy('network', networkError(), {}, t);
    expect(copy.title).toBe('T:warnings.daemonNetworkTitle');
    expect(copy.message).toBe('T:warnings.daemonNetworkMessage');
  });

  it('given an api failure without overrides, when assembling copy, then it uses the daemon api title and the server message', () => {
    const err = apiError();
    const copy = operationFailureCopy('api', err, {}, t);
    expect(copy.title).toBe('T:warnings.daemonApiTitle');
    expect(copy.message).toBe(err.message);
  });

  it('given an other failure without overrides, when assembling copy, then it uses the generic operation-failed title and message', () => {
    const copy = operationFailureCopy('other', new Error('boom'), {}, t);
    expect(copy.title).toBe('T:warnings.operationFailedTitle');
    expect(copy.message).toBe('T:warnings.operationFailedMessage');
  });

  it('given explicit title and message overrides, when assembling copy, then they win over the family defaults', () => {
    for (const kind of ['network', 'api', 'other'] as const) {
      const copy = operationFailureCopy(kind, new Error('x'), { title: 'Custom title', message: 'Custom message' }, t);
      expect(copy.title).toBe('Custom title');
      expect(copy.message).toBe('Custom message');
    }
  });
});