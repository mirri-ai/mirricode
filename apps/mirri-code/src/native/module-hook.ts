declare const __MIRRICODE_NATIVE_BUNDLE__: boolean | undefined;

/**
 * Install a hook that intercepts `require()` calls for native modules
 * so they resolve from the bundled native asset directory instead of
 * `node_modules`.
 *
 * In the npm (non-native) build this is a no-op — modules resolve normally
 * from `node_modules`. In the native SEA build the implementation lives in
 * the native build pipeline.
 */
export function installNativeModuleHook(): void {
  if (
    typeof __MIRRICODE_NATIVE_BUNDLE__ === 'boolean' &&
    __MIRRICODE_NATIVE_BUNDLE__
  ) {
    // TODO: native SEA implementation
    return;
  }
  // No-op for npm build
}
