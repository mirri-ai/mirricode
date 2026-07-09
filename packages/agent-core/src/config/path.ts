import { mkdirSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'pathe';

export function resolveMirriHome(homeDir?: string | undefined): string {
  return homeDir ?? process.env['MIRRICODE_HOME'] ?? join(homedir(), '.mirri-code');
}

export function resolveConfigPath(input: {
  readonly homeDir?: string | undefined;
  readonly configPath?: string | undefined;
}): string {
  return input.configPath ?? join(resolveMirriHome(input.homeDir), 'config.toml');
}

export function ensureMirriHome(homeDir: string): void {
  mkdirSync(homeDir, { recursive: true, mode: 0o700 });
}
