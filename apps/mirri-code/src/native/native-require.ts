declare const __MIRRICODE_NATIVE_BUNDLE__: boolean | undefined;

/**
 * Load a native package that is bundled inside the SEA executable.
 *
 * In the native SEA build, this resolves the package from the bundled asset
 * directory. In the npm (non-native) build it always returns `null` — callers
 * fall back to the normal `require()` resolution from `node_modules`.
 *
 * @param name - The npm package name to load (e.g. `@mariozechner/clipboard`).
 * @returns The loaded module, or `null` when not in a native bundle.
 */
export function loadNativePackage<T>(name: string): T | null {
  if (
    typeof __MIRRICODE_NATIVE_BUNDLE__ === 'boolean' &&
    __MIRRICODE_NATIVE_BUNDLE__
  ) {
    // TODO: native SEA implementation
  }
  // Not available in the npm build
  return null;
}
