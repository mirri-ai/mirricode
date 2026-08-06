/**
 * Minimal filesystem interface for agent-profile discovery and roots.
 *
 * This is a structural subset of both v1 and v2's `IHostFileSystem` —
 * callers pass their own `IHostFileSystem` instance and it structurally
 * satisfies this interface. The package has zero DI dependencies.
 */

export interface ProfileFs {
  readText(path: string): Promise<string>;
  readdir(path: string): Promise<readonly { readonly name: string; readonly isFile: boolean; readonly isDirectory: boolean }[]>;
  realpath(path: string): Promise<string>;
  stat(path: string): Promise<{ readonly isFile: boolean; readonly isDirectory: boolean }>;
}
