declare const __MIRRICODE_NATIVE_BUNDLE__: boolean | undefined;

/**
 * Return the absolute path to the web assets directory bundled inside the
 * native SEA executable, or `null` when running from the npm build (where
 * web assets are served from `dist-web` relative to the package root).
 */
export function getNativeWebAssetsDir(): string | null {
  if (
    typeof __MIRRICODE_NATIVE_BUNDLE__ === 'boolean' &&
    __MIRRICODE_NATIVE_BUNDLE__
  ) {
    // TODO: native SEA implementation
  }
  // No web assets bundled in the npm build
  return null;
}
