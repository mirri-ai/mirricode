import {
  createRPC,
  ErrorCodes,
  MirriError,
  parseConfigString,
  resolveConfigPath,
  type RPCMethods,
} from '@mirri-ai/agent-core';
import { z } from 'zod';

export type MirriConfigValidationPathSegment = string | number;

export interface MirriConfigValidationIssue {
  readonly path: readonly MirriConfigValidationPathSegment[];
  readonly message: string;
}

export interface ResolveMirriConfigPathInput {
  readonly homeDir?: string | undefined;
  readonly configPath?: string | undefined;
}

export interface ValidateMirriConfigTomlInput {
  readonly text: string;
  readonly filePath?: string | undefined;
}

export interface MirriConfigRpc {
  resolveConfigPath(input?: ResolveMirriConfigPathInput): Promise<string>;
  validateConfigToml(input: ValidateMirriConfigTomlInput): Promise<void>;
}

interface MirriConfigCoreRpc {
  resolveConfigPath(input: ResolveMirriConfigPathInput): string;
  validateConfigToml(input: ValidateMirriConfigTomlInput): void;
}

interface MirriConfigClientRpc {}

class MirriConfigCoreRpcImpl implements MirriConfigCoreRpc {
  resolveConfigPath(input: ResolveMirriConfigPathInput): string {
    return resolveConfigPath(input);
  }

  validateConfigToml(input: ValidateMirriConfigTomlInput): void {
    try {
      parseConfigString(input.text, input.filePath);
    } catch (error) {
      const validationIssues = extractValidationIssues(error);
      if (validationIssues !== undefined) {
        throw toConfigValidationError(error, validationIssues);
      }
      throw error;
    }
  }
}

export class MirriConfigRpcClient implements MirriConfigRpc {
  private readonly ready: Promise<RPCMethods<MirriConfigCoreRpc>>;

  constructor() {
    const [coreRpc, clientRpc] = createRPC<MirriConfigCoreRpc, MirriConfigClientRpc>();
    void coreRpc(new MirriConfigCoreRpcImpl());
    this.ready = clientRpc({});
  }

  async resolveConfigPath(input: ResolveMirriConfigPathInput = {}): Promise<string> {
    const rpc = await this.ready;
    return rpc.resolveConfigPath(input);
  }

  async validateConfigToml(input: ValidateMirriConfigTomlInput): Promise<void> {
    const rpc = await this.ready;
    await rpc.validateConfigToml(input);
  }
}

export function createMirriConfigRpc(): MirriConfigRpc {
  return new MirriConfigRpcClient();
}

function toConfigValidationError(
  error: unknown,
  validationIssues: readonly MirriConfigValidationIssue[],
): MirriError {
  const details =
    error instanceof MirriError && error.details !== undefined
      ? { ...error.details, validationIssues }
      : { validationIssues };

  if (error instanceof MirriError) {
    return new MirriError(error.code, error.message, { details });
  }

  const message = error instanceof Error ? error.message : String(error);
  return new MirriError(ErrorCodes.CONFIG_INVALID, message, { details });
}

function extractValidationIssues(error: unknown): readonly MirriConfigValidationIssue[] | undefined {
  const zodError = findZodError(error);
  if (zodError === undefined) return undefined;
  return zodError.issues.map((issue) => ({
    path: issue.path.map((segment) =>
      typeof segment === 'number' ? segment : String(segment),
    ),
    message: issue.message,
  }));
}

function findZodError(error: unknown): z.ZodError | undefined {
  if (error instanceof z.ZodError) return error;
  if (error instanceof Error && error.cause instanceof z.ZodError) return error.cause;
  return undefined;
}
