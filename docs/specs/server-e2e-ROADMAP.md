# server-e2e 场景测试域 — ROADMAP

> **文档类型**：计划（ROADMAP）。本域状态与开放项的唯一维护点；需求见 [server-e2e-requirements.md](server-e2e-requirements.md)，架构见 [server-e2e-architecture.md](server-e2e-architecture.md)。

## 当前批次：用户旅程场景套件（R1-R12）

| 项 | 状态 |
|---|---|
| 需求/架构/context 三件套 | 已定稿（2026-10-28，含评审修复） |
| WS 客户端迁移到统一协议（ADR-001） | 已完成（`src/ws.ts` 统一 JSON-RPC 客户端） |
| 进程编排器 + 旅程用例 R1-R12 | 已完成（`journey-01`..`journey-07`，7/7 全绿） |
| 旧 12 场景处置（并入/删除） | 已完成（处置台账见 `packages/server-e2e/README.md`） |
| docker 执行环境对齐（ADR-005） | 已完成（脚本已按旅程模型重构：容器内构建 + `MIRRICODE_E2E_RUST_BIN`；全流程实跑待编排方执行） |

## 开放项

- 多模态输入旅程（旧场景 09）：fake provider 无法确定性表达多模态模型语义，待真实模型分层方案后补旅程用例
- `MIRRICODE_MODEL_*` 环境变量注入是否可作为 docker 流程 config seed 的轻量替代——待验证 rust 侧 `apply_env_model_config` 行为后裁决
- 旅程用例是否进 CI（当前裁决：不进；A/B/C 组宿主本机可自动化，视维护成本再议）
