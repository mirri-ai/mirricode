// apps/mirri-web/src/lib/workspacePathInput.ts
// Shared path-parsing and validation utilities for the workspace picker's
// absolute-path entry mode. Handles POSIX (~, /), Windows drive (C:\) and UNC
// (\\server\share) paths so the same logic can be used both in the browser
// (where the daemon does the actual filesystem browse) and in tests.

const PATH_LIKE = /^(?:\/|~(?:\/|$)|[A-Za-z]:[\\/]|\\\\)/;
const WINDOWS_DRIVE = /^[A-Za-z]:[\\/]/;
const FORWARD_UNC = /^\/\/(?!\/)/;

export type WorkspacePathSeparator = '/' | '\\';

export interface ParsedWorkspacePathInput {
  target: string;
  parent: string;
  base: string;
  separator: WorkspacePathSeparator;
}

export function isWorkspacePathInput(raw: string): boolean {
  return PATH_LIKE.test(raw.trim());
}

function expandTilde(raw: string, homePath: string): string {
  if (raw === '~') return homePath || raw;
  if (raw.startsWith('~/')) return (homePath || '~') + raw.slice(1);
  return raw;
}

function normalizeForwardSlashes(path: string): string {
  if (FORWARD_UNC.test(path)) {
    return `//${path.slice(2).replaceAll(/\/{2,}/g, '/')}`;
  }
  return path.replaceAll(/\/{2,}/g, '/');
}

function isWindowsPath(path: string): boolean {
  return WINDOWS_DRIVE.test(path) || path.startsWith('\\\\') || path.startsWith('//');
}

function rootLength(path: string): number {
  if (WINDOWS_DRIVE.test(path)) return 3;
  if (path.startsWith('\\\\') || path.startsWith('//')) return 2;
  if (path.startsWith('/')) return 1;
  return 0;
}

export function parseWorkspacePathInput(
  raw: string,
  homePath: string,
): ParsedWorkspacePathInput {
  let target = normalizeForwardSlashes(expandTilde(raw.trim(), homePath));
  const windowsPath = isWindowsPath(target);
  const isRoot =
    target === '/' ||
    target === '~' ||
    WINDOWS_DRIVE.test(target) && target.length <= 3 && (target.length === 2 || target[2] === '/' || target[2] === '\\');

  if (isRoot) {
    return {
      target,
      parent: target,
      base: '',
      separator: windowsPath ? '\\' : '/',
    };
  }

  // Remove trailing slashes for splitting (preserve root-only).
  const trimmed = target.replace(/[/\\]+$/, '');
  const lastSlash = Math.max(trimmed.lastIndexOf('/'), trimmed.lastIndexOf('\\'));

  if (lastSlash < rootLength(target)) {
    // No parent above the root (e.g. "/foo" or "~/bar").
    const sep: WorkspacePathSeparator = windowsPath ? '\\' : '/';
    return { target, parent: target.slice(0, rootLength(target)) || sep, base: trimmed.slice(rootLength(target)), separator: sep };
  }

  const parent = trimmed.slice(0, lastSlash) || (windowsPath ? '\\' : '/');
  const base = trimmed.slice(lastSlash + 1);
  const sep: WorkspacePathSeparator = trimmed[lastSlash] === '\\' ? '\\' : '/';
  return { target, parent, base, separator: sep };
}

/**
 * Build the display path for a validated workspace target. The lexical input
 * (`typedPath`) is preferred over the daemon's canonical (realpath) result so
 * that symlinked workspace ids stay based on the user's typed root.
 */
export function currentValidatedWorkspacePath(
  raw: string,
  homePath: string,
  typedPath: string | null,
): string {
  if (typedPath) return typedPath;
  return normalizeForwardSlashes(expandTilde(raw.trim(), homePath));
}

/**
 * Join a parent path with a candidate name, preserving the separator style
 * the user typed (forward vs backslash).
 */
export function joinWorkspacePathCandidate(
  parent: string,
  name: string,
  sep: WorkspacePathSeparator,
): string {
  // Normalize the parent: strip trailing separators so the join adds exactly one.
  const base = parent.replace(/[/\\]+$/, '');
  return `${base}${sep}${name}`;
}
