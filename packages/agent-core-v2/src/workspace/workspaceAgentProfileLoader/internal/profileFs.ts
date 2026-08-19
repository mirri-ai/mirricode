/**
 * `workspaceAgentProfileLoader` domain — `IHostFileSystem` → `ProfileFs` adapter
 * for the shared `@mirri-ai/agent-profile` package.
 *
 * The shared package discovers agent files through its minimal `ProfileFs`
 * interface; this adapter wires the engine's `IHostFileSystem` to it and
 * carries the host-fs error semantics through the optional `isUnavailable`
 * hook — a transient whole-fs outage propagates instead of being absorbed, so
 * an already-registered contribution survives a partial scan.
 */

import type { ProfileFs } from '@mirri-ai/agent-profile';

import type { IHostFileSystem } from '#/os/interface/hostFileSystem';
import { HostFsError, OsFsErrors } from '#/os/interface/hostFsErrors';

export function toProfileFs(fs: IHostFileSystem): ProfileFs {
  const isHostErr = (error: unknown, code: string): boolean =>
    error instanceof HostFsError && error.code === code;
  return {
    readText: (path) => fs.readText(path),
    readdir: (path) => fs.readdir(path),
    stat: (path) => fs.stat(path),
    realpath: (path) => fs.realpath(path),
    isUnavailable: (error) => isHostErr(error, OsFsErrors.codes.OS_FS_UNAVAILABLE),
    isNotFound: (error) =>
      isHostErr(error, OsFsErrors.codes.OS_FS_NOT_FOUND) ||
      isHostErr(error, OsFsErrors.codes.OS_FS_NOT_DIRECTORY),
  };
}