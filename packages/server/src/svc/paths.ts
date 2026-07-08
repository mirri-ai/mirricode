

import { homedir } from 'node:os';
import { join } from 'node:path';

import { resolveMirriHome } from '@mirri-ai/agent-core';


export const MIRRI_SERVER_LABEL = 'ai.moonshot.kimi-server';


export const MIRRI_SERVER_PLIST_FILENAME = `${MIRRI_SERVER_LABEL}.plist`;


export const MIRRI_SERVER_SYSTEMD_UNIT = 'kimi-server.service';


export const MIRRI_SERVER_TASK_NAME = 'MirriServer';


export function launchAgentPlistPath(): string {
  return join(homedir(), 'Library', 'LaunchAgents', MIRRI_SERVER_PLIST_FILENAME);
}


export function systemdUnitPath(): string {
  return join(homedir(), '.config', 'systemd', 'user', MIRRI_SERVER_SYSTEMD_UNIT);
}


export function supervisorLogPath(): string {
  return join(resolveMirriHome(), 'server', 'server.log');
}


export function installPlanPath(): string {
  return join(resolveMirriHome(), 'server', 'install.json');
}


export function guiDomain(uid: number = process.getuid?.() ?? 0): string {
  return `gui/${uid}`;
}
