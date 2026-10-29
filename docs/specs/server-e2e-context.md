# server-e2e 场景测试域 — 现状快照（context）

> **文档类型**：参考（Reference）。只记录当前状态事实，不含决策。快照日期：2026-10-28。

## 旧场景套件现状

- `packages/server-e2e` 的 `src/`（client/http/ws/envelope/wait/report/reverse-rpc）为已退役的 TS 引擎协议编写
- **WS 层断点（实测确认）**：客户端发 `{type:'subscribe', id, payload:{session_ids:[...]}}` 并等待 `ack` 帧；rust 服务器的两个 WS 端点都不说此方言：
  - `/api/v1/ws`（legacy）：`{type:'subscribe', sessionId}` → 回 `subscribed` 帧
  - `/api/v1/rpc`（统一协议，web 生产使用）：JSON-RPC 2.0，`session.subscribe` 方法 + `event.<type>` 通知，带 `{seq, epoch}` 游标回放
- REST 层部分兼容（`createSession` 等已在 rust server 上实测通过；但错误码/部分路径未全量核对）
- 旧场景共 **12 个可执行场景文件**（`scenarios/01-12*.ts`，另有 README 与 `_template.ts` 非用例）；另有两个单元测试文件（client/log 等）。全部场景依赖 WS 层 → 当前全部无法运行

## 引擎侧进程内黑盒测试现状（`rust/crates/mirri-server/tests/`）

cargo 实测清点（`cargo test -- --list`，2026-10-28）：

| 文件 | 用例数 |
|---|---|
| ws_rpc_tests | 34 |
| http_tests | 31 |
| files_tests | 15 |
| ws_tests | 10 |
| e2e_mock_test | 4 |
| ipc_tests | 4 |
| skill_e2e_tests | 2 |
| 合计 | 100 |

（源码中 `#[tokio::test]` 属性计数更高——68/62/20 等——因含被 `#[ignore]` 或条件编译的用例，以 cargo 实跑清单为准。）

## docker 流程现状（2026-10-28 实测修复后）

- 根 `Dockerfile` + `packages/server-e2e/Dockerfile` 为本地未跟踪文件（`.gitignore:21` 忽略 `Dockerfile`，从未提交）；`.dockerignore` 已补（构建上下文 45GB → 4.6KB）
- `run-docker-e2e.sh` 已改造为：容器内 `cargo build` rust 二进制（可写 target 目录）→ `mirri serve` → 场景执行
- 模型配置：`MIRRICODE_SERVER_E2E_FAKE_URL` 环境变量 + 容器内写入 config.toml（`default_model` 必须是 TOML 顶层键——放 table 内会被引擎忽略，`mirri doctor` 可检出该错误）
- `scripts/fake-provider.ts` 已存在：e2e-harness 替身绑定所有网卡、轮询补队列、输出 `host.docker.internal` URL（macOS 防火墙拦截 LAN 直连，网关地址不拦）
- 已验证到：容器内 rust 构建 ✓、server 启动 ✓、config 加载 ✓、createSession ✓；卡在 WS 协议断点

## 可复用基础设施

- `@mirri-ai/e2e-harness` 的 `createFakeProviderServer`（已支持自定义绑定地址）
- web rust e2e（`apps/mirri-web/test/`）内含 spawn rust server + fake provider 的成熟模式可参考（仅参考，不建立代码依赖）
- `mirri migrate` 子命令存在于 rust CLI（`rust/crates/mirri-cli/src/migrate.rs`）：把旧 TS sessions 布局复制并改造为 rust v3 存储根（复制不移动，legacy 根保持不动）

## 环境事实

- 本机 colima（VZ）运行 docker；virtiofs 对宿主新写文件有目录列表缓存陈旧问题（容器内重写 config 规避）
- macOS 应用防火墙拦截 docker VM → 宿主 LAN IP 入站；`host.docker.internal`（网关）不拦
