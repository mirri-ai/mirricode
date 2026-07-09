declare const __MIRRICODE_NATIVE_BUNDLE__: boolean | undefined;
declare const __MIRRICODE_BUILD_TARGET__: string | undefined;

/**
 * Run a quick smoke test of the native asset bundle when the user sets
 * `MIRRI_CODE_NATIVE_ASSET_SMOKE=1`.
 *
 * Returns `true` when the smoke test ran (caller should exit), `false`
 * otherwise. In the npm (non-native) build this always returns `false`.
 * In the native SEA build this prints the expected marker line and exits.
 */
export function runNativeAssetSmokeIfRequested(): boolean {
  if (
    typeof __MIRRICODE_NATIVE_BUNDLE__ === 'boolean' &&
    __MIRRICODE_NATIVE_BUNDLE__
  ) {
    if (process.env['MIRRI_CODE_NATIVE_ASSET_SMOKE'] === '1') {
      const target =
        typeof __MIRRICODE_BUILD_TARGET__ === 'string'
          ? __MIRRICODE_BUILD_TARGET__
          : `${process.platform}-${process.arch}`;
      process.stdout.write(`Native asset smoke passed: ${target}\n`);
      process.exit(0);
    }
  }
  // No-op for npm build
  return false;
}
