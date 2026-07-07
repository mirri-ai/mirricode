declare const __MIRRICODE_NATIVE_BUNDLE__: boolean | undefined;

/**
 * Clean up stale native cache directories for the current version.
 *
 * In the npm (non-native) build this is a no-op — there is no native cache
 * to manage. In the native SEA build the implementation lives in the native
 * build pipeline.
 */
export function cleanupStaleNativeCacheForCurrent(): void {
  if (
    typeof __MIRRICODE_NATIVE_BUNDLE__ === 'boolean' &&
    __MIRRICODE_NATIVE_BUNDLE__
  ) {
    // TODO: native SEA implementation
    return;
  }
  // No-op for npm build
}
