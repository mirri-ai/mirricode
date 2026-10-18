// apps/mirri-web/src/lib/backendCapabilities.ts
// Pure TS — 后端能力判别（消费端诚实呈现的唯一依据）。
// v3 = Rust 引擎：7 个 Collaboration* 工具 + collaboration.enter/exit RPC
// （collaboration-context.md §2）；v1/v2 = TS 引擎：prompt schema 剥离
// collaboration_mode（context §6），开关无引擎效果 → 隐藏。

/** Collaboration 模式开关是否对该后端有引擎效果。 */
export function collaborationSupportedForBackend(backend: 'v1' | 'v2' | 'v3'): boolean {
  return backend === 'v3';
}
