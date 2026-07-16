/**
 * `POST /sessions/{session_id}/export` — stream a session diagnostic archive.
 *
 * The server owns archive options and temporary paths. A bounded Web JSONL log
 * may be supplied by the client and is added to the archive by sessionExport.
 */

import { createReadStream, type ReadStream } from 'node:fs';
import { mkdtemp, rm, stat } from 'node:fs/promises';
import type { ServerResponse } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import {
  ISessionExportService,
  isError2,
  ErrorCodes,
} from '@mirri-ai/agent-core-v2';
import type { IInstantiationService } from '@mirri-ai/agent-core';
import {
  ErrorCode,
  exportSessionParamsSchema,
  exportSessionRequestSchema,
} from '@mirri-ai/protocol';

import { defineRoute } from '../middleware/defineRoute';
import { errEnvelope } from '../envelope';

interface SessionExportRouteHost {
  post(
    path: string,
    options: { preHandler: unknown[]; schema?: Record<string, unknown> },
    handler: (req: unknown, reply: SessionExportReply) => unknown,
  ): unknown;
}

interface SessionExportReply {
  readonly raw: ServerResponse;
  type(mime: string): SessionExportReply;
  header(name: string, value: string | number): SessionExportReply;
  send(payload: unknown): unknown;
}

export function registerSessionExportRoute(
  app: SessionExportRouteHost,
  ix: IInstantiationService,
  options: { readonly serverVersion: string },
): void {
  const route = defineRoute(
    {
      method: 'POST',
      path: '/sessions/{session_id}/export',
      params: exportSessionParamsSchema,
      body: exportSessionRequestSchema,
      rawResponse: {
        200: { type: 'string', format: 'binary' },
      },
      errors: {
        [ErrorCode.VALIDATION_FAILED]: {},
        [ErrorCode.SESSION_NOT_FOUND]: {},
        [ErrorCode.FILE_TOO_LARGE]: {},
        [ErrorCode.INTERNAL_ERROR]: {},
      },
      description: 'Export a session and diagnostic logs as a zip archive',
      tags: ['sessions'],
    },
    async (req, reply) => {
      const response = reply as unknown as SessionExportReply;
      let aborted = false;
      let responseStream: ReadStream | undefined;
      let streaming = false;
      let tempDir: string | undefined;
      let cleanupPromise: Promise<void> | undefined;
      const exportAbort = new AbortController();

      const onResponseClose = (): void => {
        if (response.raw.writableFinished) return;
        aborted = true;
        exportAbort.abort();
        responseStream?.destroy();
      };
      response.raw.once('close', onResponseClose);

      const cleanup = async (): Promise<void> => {
        if (tempDir === undefined) return;
        cleanupPromise ??= rm(tempDir, {
          recursive: true,
          force: true,
          maxRetries: 3,
          retryDelay: 50,
        }).catch((error: unknown) => {
          console.warn('session export temporary directory cleanup failed', { error, requestId: req.id, tempDir });
        });
        await cleanupPromise;
      };

      try {
        if (aborted) return;

        const safeSessionId = sanitizeSessionId(req.params.session_id);
        tempDir = await mkdtemp(join(tmpdir(), `mirri-session-export-${safeSessionId}-`));
        if (aborted) {
          await cleanup();
          return;
        }

        const outputPath = join(tempDir, 'session.zip');
        const exportService = ix.invokeFunction((accessor) =>
          accessor.get(ISessionExportService),
        );
        await exportService.export({
          sessionId: req.params.session_id,
          outputPath,
          includeGlobalLog: true,
          version: options.serverVersion,
        });
        if (aborted) {
          await cleanup();
          return;
        }

        const archive = await stat(outputPath);
        if (aborted) {
          await cleanup();
          return;
        }

        responseStream = createReadStream(outputPath);
        responseStream.once('close', () => {
          response.raw.off('close', onResponseClose);
          void cleanup();
        });

        const sent = response
          .type('application/zip')
          .header(
            'content-disposition',
            `attachment; filename="mirri-session-${safeSessionId}.zip"`,
          )
          .header('content-length', archive.size)
          .header('cache-control', 'no-store')
          .send(responseStream);
        streaming = true;
        return sent as void;
      } catch (error) {
        const stream = responseStream;
        if (stream !== undefined && !stream.closed) {
          const closed = new Promise<void>((resolve) => {
            stream.once('close', resolve);
          });
          stream.destroy();
          await closed;
        } else {
          stream?.destroy();
        }
        await cleanup();
        if (!aborted) sendMappedError(response, req, error);
      } finally {
        if (!streaming) response.raw.off('close', onResponseClose);
      }
    },
  );

  app.post(
    route.path,
    route.options,
    route.handler as unknown as Parameters<SessionExportRouteHost['post']>[2],
  );
}

function sanitizeSessionId(sessionId: string): string {
  return sessionId.replaceAll(/[^A-Za-z0-9_-]/g, '_').slice(0, 48) || 'session';
}

function sendMappedError(reply: SessionExportReply, req: { id: string }, error: unknown): void {
  const requestId = req.id;
  if (isError2(error)) {
    if (error.code === ErrorCodes.SESSION_NOT_FOUND) {
      reply.send(errEnvelope(ErrorCode.SESSION_NOT_FOUND, error.message, requestId));
      return;
    }
    if (error.code === ErrorCodes.SESSION_EXPORT_TOO_LARGE) {
      reply.send(
        errEnvelope(
          ErrorCode.FILE_TOO_LARGE,
          'session export exceeds the 64 MiB web limit',
          requestId,
        ),
      );
      return;
    }
  }
  console.error('session export failed', { err: error });
  reply.send(
    errEnvelope(
      ErrorCode.INTERNAL_ERROR,
      error instanceof Error ? error.message : 'internal error',
      requestId,
    ),
  );
}
