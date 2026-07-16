/**
 * `sessionExport` domain (L6) — `ISessionExportService` implementation.
 *
 * Coordinates live session flushing through `sessionLifecycle`, derives session
 * paths from `bootstrap`, reads persisted summaries through `sessionIndex`, and
 * packages diagnostic files through the local zip writer. Bound at App scope.
 */

import { readFile } from 'node:fs/promises';

import { resolve } from 'pathe';

import { ErrorCodes, MirriError } from '@mirri-ai/agent-core';

import { buildExportManifest, type ExportSessionManifestSummary } from './manifest';
import {
  type ExportSessionPayload,
  type ExportSessionResult,
  type ISessionExportService,
} from './sessionExport';
import { scanSessionWire } from './wire-scan';
import {
  type ExtraZipEntry,
  collectFilesRecursive,
  writeExportZip,
} from './zip';

const SESSION_LOG_REL = 'logs/mirri-code.log';
const GLOBAL_LOG_REL = 'logs/global/mirri-code.log';

export interface SessionSummary {
  readonly id: string;
  readonly title?: string;
  readonly workspaceId: string;
}

export interface SessionExportServiceDeps {
  readonly getHomeDir: () => string;
  readonly getSessionDir: (workspaceId: string, sessionId: string) => string;
  readonly getSessionSummary: (sessionId: string) => Promise<SessionSummary | undefined>;
  readonly getWorkspaceRoot: (workspaceId: string) => Promise<string | undefined>;
  readonly flushLogs: () => Promise<void>;
  readonly warn: (message: string, context?: Record<string, unknown>) => void;
}

export class SessionExportService implements ISessionExportService {
  declare readonly _serviceBrand: undefined;

  constructor(private readonly deps: SessionExportServiceDeps) {}

  async export(input: ExportSessionPayload): Promise<ExportSessionResult> {
    if (input.version.trim().length === 0) {
      throw new MirriError(
        ErrorCodes.SESSION_EXPORT_MISSING_VERSION,
        'Session export requires a host version.',
        { details: { sessionId: input.sessionId } },
      );
    }

    const summary = await this.deps.getSessionSummary(input.sessionId);
    if (summary === undefined) {
      throw new MirriError(
        ErrorCodes.SESSION_NOT_FOUND,
        `Session "${input.sessionId}" does not exist`,
        { details: { sessionId: input.sessionId } },
      );
    }

    const workspaceRoot = await this.deps.getWorkspaceRoot(summary.workspaceId);
    const sessionDir = this.deps.getSessionDir(summary.workspaceId, summary.id);

    const exportSummary: ExportSessionDirectorySummary = {
      id: summary.id,
      title: summary.title,
      workspaceDir: workspaceRoot,
      sessionDir,
    };

    if (input.includeGlobalLog === true) {
      await this.warnIfFails('export global log flush failed', () => this.deps.flushLogs(), {
        retry: true,
      });
    }

    return exportSessionDirectory({
      request: input,
      summary: exportSummary,
      globalLogPath: resolve(this.deps.getHomeDir(), 'logs', 'mirri-code.log'),
    });
  }

  private async warnIfFails(
    message: string,
    operation: () => Promise<void>,
    options: { readonly retry?: boolean } = {},
  ): Promise<void> {
    try {
      await operation();
      return;
    } catch (error) {
      this.deps.warn(message, { error });
    }
    if (options.retry !== true) return;
    try {
      await operation();
    } catch {}
  }
}

export interface ExportSessionDirectorySummary extends ExportSessionManifestSummary {
  readonly sessionDir: string;
}

export async function exportSessionDirectory(input: {
  readonly request: ExportSessionPayload;
  readonly summary: ExportSessionDirectorySummary;
  readonly globalLogPath?: string | undefined;
}): Promise<ExportSessionResult> {
  const sessionDir = input.summary.sessionDir;
  const sessionFiles = await collectFilesRecursive(sessionDir);
  if (sessionFiles.length === 0) {
    throw new MirriError(
      ErrorCodes.SESSION_EXPORT_NOT_FOUND,
      `Session "${input.summary.id}" has no exportable directory at "${sessionDir}"`,
      { details: { sessionId: input.summary.id, sessionDir } },
    );
  }

  const sessionScan = await scanSessionWire(sessionDir);
  const hasSessionLog = sessionFiles.some((f) =>
    f.endsWith(`/${SESSION_LOG_REL}`) || f.endsWith(`\\${SESSION_LOG_REL.replaceAll('/', '\\')}`),
  );

  const extras: ExtraZipEntry[] = [];
  let bundledGlobal = false;
  if (input.request.includeGlobalLog === true && input.globalLogPath !== undefined) {
    const data = await readOptionalFile(input.globalLogPath);
    if (data !== undefined) {
      extras.push({ data, target: GLOBAL_LOG_REL });
      bundledGlobal = true;
    }
  }

  const manifest = buildExportManifest({
    summary: input.summary,
    now: new Date(),
    version: input.request.version,
    sessionScan,
    sessionLogPath: hasSessionLog ? SESSION_LOG_REL : undefined,
    globalLogPath: bundledGlobal ? GLOBAL_LOG_REL : undefined,
    installSource: input.request.installSource,
    shellEnv: input.request.shellEnv,
  });

  const outputPath =
    input.request.outputPath !== undefined
      ? resolve(input.request.outputPath)
      : resolve(`${input.summary.id}.zip`);

  const entries = await writeExportZip({
    outputPath,
    manifest,
    sessionDir,
    sessionFiles,
    extraEntries: extras,
  });

  return {
    zipPath: outputPath,
    entries,
    sessionDir,
    manifest,
  };
}

async function readOptionalFile(path: string): Promise<Buffer | undefined> {
  try {
    return await readFile(path);
  } catch {
    return undefined;
  }
}
