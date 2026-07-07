declare const __MIRRICODE_VERSION__: string | undefined;
declare const __MIRRICODE_CHANNEL__: string | undefined;
declare const __MIRRICODE_COMMIT__: string | undefined;
declare const __MIRRICODE_BUILD_TARGET__: string | undefined;

export interface KimiBuildInfo {
  readonly version?: string;
  readonly channel?: string;
  readonly commit?: string;
  readonly buildTarget?: string;
}

function optionalBuildString(value: unknown): string | undefined {
  return typeof value === 'string' && value.length > 0 ? value : undefined;
}

export const KIMI_BUILD_INFO: KimiBuildInfo = {
  version:
    typeof __MIRRICODE_VERSION__ === 'string'
      ? optionalBuildString(__MIRRICODE_VERSION__)
      : undefined,
  channel:
    typeof __MIRRICODE_CHANNEL__ === 'string'
      ? optionalBuildString(__MIRRICODE_CHANNEL__)
      : undefined,
  commit:
    typeof __MIRRICODE_COMMIT__ === 'string'
      ? optionalBuildString(__MIRRICODE_COMMIT__)
      : undefined,
  buildTarget:
    typeof __MIRRICODE_BUILD_TARGET__ === 'string'
      ? optionalBuildString(__MIRRICODE_BUILD_TARGET__)
      : undefined,
};
