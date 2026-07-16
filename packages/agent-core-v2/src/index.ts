/**
 * Minimal agent-core-v2 surface for the session-export server route.
 *
 * This package is a stub — the full agent-core-v2 module lives in the
 * upstream kimi-code repository. Only the symbols consumed by
 * `packages/server/src/routes/sessionExport.ts` are re-exported here.
 */

import {
  createDecorator,
  type ServiceIdentifier,
} from '@mirri-ai/agent-core/di/instantiation';

// ---------------------------------------------------------------------------
// Session export service
// ---------------------------------------------------------------------------

export interface ShellEnvironment {
  readonly term?: string | undefined;
  readonly termProgram?: string | undefined;
  readonly termProgramVersion?: string | undefined;
  readonly multiplexer?: string | undefined;
  readonly shell?: string | undefined;
}

export interface ExportSessionPayload {
  readonly sessionId: string;
  readonly outputPath?: string | undefined;
  readonly includeGlobalLog?: boolean | undefined;
  readonly version: string;
  readonly installSource?: string | undefined;
  readonly shellEnv?: ShellEnvironment | undefined;
}

export interface ExportSessionManifest {
  readonly sessionId: string;
  readonly exportedAt: string;
  readonly kimiCodeVersion: string;
  readonly wireProtocolVersion: string;
  readonly os: string;
  readonly nodejsVersion: string;
  readonly sessionFirstActivity?: string | undefined;
  readonly sessionLastActivity?: string | undefined;
  readonly title?: string | undefined;
  readonly workspaceDir?: string | undefined;
  readonly sessionLogPath?: string | undefined;
  readonly globalLogPath?: string | undefined;
  readonly installSource?: string | undefined;
  readonly shellEnv?: ShellEnvironment | undefined;
}

export interface ExportSessionResult {
  readonly zipPath: string;
  readonly entries: readonly string[];
  readonly sessionDir: string;
  readonly manifest: ExportSessionManifest;
}

export interface ExportSessionOptions {
  readonly webLog?: string;
  readonly signal?: AbortSignal;
  readonly maxArchiveBytes?: number;
}

export interface ISessionExportService {
  readonly _serviceBrand: undefined;
  export(input: ExportSessionPayload): Promise<ExportSessionResult>;
}

export const ISessionExportService: ServiceIdentifier<ISessionExportService> =
  createDecorator<ISessionExportService>('sessionExportService');

// ---------------------------------------------------------------------------
// Error primitives
// ---------------------------------------------------------------------------

export interface Error2Options {
  readonly details?: Readonly<Record<string, unknown>>;
  readonly cause?: unknown;
  readonly name?: string;
}

export class Error2 extends Error {
  readonly code: string;
  readonly details?: Readonly<Record<string, unknown>>;

  constructor(code: string, message: string, options?: Error2Options) {
    super(message, options?.cause === undefined ? undefined : { cause: options.cause });
    this.name = options?.name ?? 'Error2';
    this.code = code;
    this.details = options?.details;
  }
}

export function isError2(error: unknown): error is Error2 {
  return error instanceof Error2;
}

export const ErrorCodes = {
  SESSION_NOT_FOUND: 'session.not_found',
  SESSION_EXPORT_TOO_LARGE: 'session.export_too_large',
} as const;
