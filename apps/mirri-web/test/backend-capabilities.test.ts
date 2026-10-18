// apps/mirri-web/test/backend-capabilities.test.ts
// collaboration 开关的能力呈现（D-24）：v3（Rust，7 协作工具 +
// collaboration.enter/exit RPC）有效；v1/v2 无引擎效果则消费端隐藏入口。
import { describe, expect, it } from 'vitest';
import { collaborationSupportedForBackend } from '../src/lib/backendCapabilities';

describe('collaborationSupportedForBackend', () => {
  it('Given the v3 rust engine Then collaboration is supported', () => {
    expect(collaborationSupportedForBackend('v3')).toBe(true);
  });

  it.each(['v1', 'v2'] as const)('Given the %s TS backend Then collaboration is NOT supported', (b) => {
    expect(collaborationSupportedForBackend(b)).toBe(false);
  });
});
