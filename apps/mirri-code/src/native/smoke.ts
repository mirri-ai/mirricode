declare const __MIRRICODE_NATIVE_BUNDLE__: boolean | undefined;

/**
 * Run a quick smoke test of the native asset bundle when the user sets
 * `MIRRICODE_NATIVE_SMOKE=1`.
 *
 * Returns `true` when the smoke test ran (caller should exit), `false`
 * otherwise. In the npm (non-native) build this always returns `false`.
 * In the native SEA build the implementation lives in the native build
 * pipeline.
 */
export function runNativeAssetSmokeIfRequested(): boolean {
  if (
    typeof __MIRRICODE_NATIVE_BUNDLE__ === 'boolean' &&
    __MIRRICODE_NATIVE_BUNDLE__
  ) {
    // TODO: native SEA implementation
  }
  // No-op for npm build
  return false;
}
