# 执行环境域 — ROADMAP

> 本文件是执行环境域的**状态与开放项唯一维护点**（需求/设计内容见 [execution-environment-requirements.md](execution-environment-requirements.md) / [execution-environment-architecture.md](execution-environment-architecture.md)，现状盘点见 [execution-environment-context.md](execution-environment-context.md)，快照 2026-09-12）。
> Rust 侧整体对齐进度不在本域单独跟踪，统一见 [rust-core-design/ROADMAP.md](rust-core-design/ROADMAP.md) 与 `plan/rust-v2-method-parity.md`。

## 1. 基线建立状态

| 事项 | 状态 |
|---|---|
| 四件套新建（本域为新建域，无既有内容可增量扩展） | ✅ 2026-09-12 完成 |
| requirements：8 个 Epic + 5 项提案，R1-R9 自检 | ✅ 完成 |
| architecture：双栈机制 + 接口形状 + 相邻域边界 | ✅ 完成 |
| context：代码事实 file:line + 差异矩阵 + v1 标记 | ✅ 完成 |
| SDR/ADR | 无（首次基线，无需求/设计变更） |

## 2. 覆盖状态（按能力）

| 能力 | 文档覆盖 | 备注 |
|---|---|---|
| 统一执行环境抽象（本地/远程/桥接/上下文绑定） | 已覆盖（US1.1-1.4） | 远程与桥接为 v1-only，已标记 |
| 主机环境探测（OS/壳/登录 PATH/就绪门槛） | 已覆盖（US2.1-2.4） | 变量名分叉以设计增强要求统一 |
| 文件系统原语（读/写/目录/元数据/编码/错误契约） | 已覆盖（US3.1-3.3, 3.6） | |
| 文件监视 | 已覆盖（US3.4） | |
| 远程文件语义 | 已覆盖（US3.5） | v1-only |
| 进程创建/生命周期/输出流 | 已覆盖（US4.1-4.3） | |
| 远程进程语义 | 已覆盖（US4.4） | v1-only |
| 终端仿真（创建/回放/生命周期） | 已覆盖（US5.1-5.3） | 输入/resize 通道缺口见开放项 O1（Rust 面已核验通过；web 接线已落地 B11/T3，2026-09-13）；默认壳 Windows 回退缺口见 O8（已修，2026-09-13） |
| 执行沙箱面（路径约束/附加目录/诚实边界） | 已覆盖（US6.1-6.3） | |
| rg 供给（定位/下载/降级/限额） | 已覆盖（US7.1-7.2） | 两套定位器不一致见 O3 |
| 非功能（时间预算/资源上限/可观测/并发隔离） | 已覆盖（US8） | |
| v1 无宿主服务（fs 系/终端/coreProcess/environment/workspaceFs 浏览器） | 契约已记录并标记 | 处置见 O4 |

**有意不覆盖**（归相邻域，已声明边界）：工具逐工具参数语义（tool-calling-builtin-tools.md）、Bash 超时转后台与任务面板（background-tasks）、工作区注册/信任（workspace-management）、终端帧投影与断线重连游标（conversation-rendering）。

## 3. 开放项（需人工/后续裁决）

| # | 开放项 | 类型 | 说明 |
|---|---|---|---|
| O1 | v2 终端输入/调整尺寸通道未接线 | TS 现状缺口 | 服务层/PTY 层/WS schema 完备，REST 只注册 list/create/get/close，write/resize/attach 无暴露通道（context §3.3-4）。US5.1 相应验收项在 v2 现网无消费面。**范围注（2026-09-12，B0）**：随本程序 **B11** 落地（缺口 D-35——web 终端输入/resize/attach 面核验与补齐，rust+web 验收）。**状态（2026-09-13，B11 Rust 轨）**：Rust 侧核验通过——pool 全 API + REST（`mirri-server/src/http.rs:71-77`）+ WS 五控制帧与乱序安全 attach 协议（`ws_rpc.rs:813-830`）完备（context §6.4）；web 入口接线（B11/T3）随 web 队列另批实施，待接线后关闭。**状态（2026-09-13，B11/T3 web 轨）**：web 接线落地——Terminal 面板挂载进共享 detail 层（ChatHeader + MobileTopBar 入口、i18n 化、Escape 穿透、v3 单连接 additive sinks 修复，context §6.5），**O1 关闭**；A1 终端实跑验收留 B12 统筹 |
| O2 | 壳覆盖变量名统一（MIRRI_SHELL_PATH vs MIRRICODE_SHELL_PATH） | 分叉 | Rust 侧需选一个规范名；requirements US2.2 已按终态要求"单一规范变量名"。**范围注（B0）**：env 命名规范随本程序 **B11** 归档进本域与 non-functional 域文档（缺口 D-14～D-18/D-22，以 `MIRRI_*` 为规范名）。**状态（2026-09-13，B11）**：已落地——规范表归档（context §6.1）+ Rust 采用 `MIRRI_SHELL_PATH` 主名 + `MIRRICODE_SHELL_PATH` 别名（`rust/crates/mirri-os/src/shell.rs`）；TS 侧不修（DEC-1） |
| O3 | rg 供给两套（工具层带下载 vs 搜索层无下载） | TS 现状不一致 | 提案 P3 建议统一；Rust 实现取舍待裁决 |
| O4 | v1 无宿主服务的处置 | 遗产 | v1 fs/terminal/coreProcess/environment/workspaceFs 浏览器在 v1 server 删除后无生产宿主（context §2）；公开导出面仍在。**已裁决推迟（2026-09-12，总计划 DEC-2 v1 遗留）**：处置维持"TS 曾支持的基线"保留口径，本程序不动 |
| O5 | SSH 远程执行环境是否产品化 | 提案 | 库层能力完整（除 osEnv 探测），无产品入口（提案 P2）。**已裁决推迟（DEC-2 v1 遗留：D-36 SSH 产品化）**——与 O7（host key，D-04/D-05）一并推迟，待远程能力立项时再裁 |
| O6 | OS 级沙箱 | 提案 | 现状仅路径约束层，命令执行不受约束（US6.3 诚实边界）；提案 P1。**已裁决推迟（DEC-2：全局提案 P-006）**——涉及安全模型需专项裁决，不在本程序 |
| O7 | host key 校验放行 | SSH 现状 | SSHKaos 连接对主机指纹无条件放行；**已裁决推迟（DEC-2 v1 遗留：D-04/D-05）**——随 O5 一并，Rust 产品化远程能力时须补 known_hosts 语义 |
| O8 | v2 终端默认壳无 Windows 回退 | TS 现状缺口 | v2 默认壳 `$SHELL \|\| '/bin/sh'` 无平台分支，Windows 上 PTY spawn 会失败（context §3.3-7）；requirements US5.1 已按终态要求跨平台回退。**范围注（B0）**：随本程序 **B11** 落地（缺口 D-02/D-03 的 Rust 等价核验/修复）。**状态（2026-09-13，B11/T1）**：已落地（Rust 侧）——`mirri-os/src/shell.rs` 跨平台解析（显式 > `MIRRI_SHELL_PATH` > `MIRRICODE_SHELL_PATH` 别名 > `$SHELL` > Windows pwsh→powershell→cmd / Unix `/bin/sh`），`terminal_pool.rs` detect_shell 薄委派；单测+集成测试覆盖（context §6.2）。Windows 真机验证留待用户（A2 场景） |

## 4. 后续批次建议（不构成承诺）

- 若 Rust 侧启动远程执行（O5），先补：远程主机环境探测（对应 kaos osEnv 未接线缺口）+ host key 校验（O7）。
- 探测合一（提案 P4）落地时同步在 glossary 注册"主机环境探测"跨域术语（当前留给发起方合并，见最终汇报）。

## 5. 落地程序范围标注（baseline-gap-landing 程序 · 2026-09-12）

> 依据总计划 [`plan/baseline-gap-landing.md`](../../plan/baseline-gap-landing.md)（§一 DEC-1/DEC-2 范围裁决、§二 批次定义、§四 进度跟踪）。本节只做状态与范围管理。

**已裁决推迟（本程序排除，DEC-2 v1 遗留）**：D-04/D-05（SSH host key，即 O7）、D-36（SSH 产品化，即 O5）、O4（v1 无宿主服务处置）、P-006（OS 沙箱，即 O6）——理由：v1 遗留/涉及安全模型需专项裁决，不在本程序。

**落地中（本程序进行中的缺口项与批次）**：

| 批次 | 缺口项 |
|---|---|
| B11 执行环境核验与补齐 | D-02/D-03 的 Rust 等价核验（含 Windows 壳回退，即 O8）、D-35 的 rust+web 覆盖核验（web 终端输入/resize/attach UI，即 O1）、D-19～D-21 文档站勘误（skills 域联动）、D-14～D-18/D-22 env 命名规范归档（`MIRRI_*` 为规范名，即 O2 与 non-functional 域联动） |

## 6. 变更日志

- 2026-09-12：新建本域四件套（ts-capability-baseline：execution-environment）。
- 2026-09-12：按双 reviewer（requirements 完整性 + architecture/context 一致性）首轮 findings 修订：stderr 合并语义改述（句柄同流为基线、内容合并为设计增强）、本地/远程行终止符差异显式化、终端默认壳跨平台回退终态化（新增 O8）、rg 供给分层表述、E6 与工具调用域路径守卫口径边界澄清、ACP 桥接文本面限定、v1-only 项补标（100 路径上限/一次性告警）、若干 TS 现状注记补齐。
- 2026-09-12（B0 治理批次）：按总计划 DEC-2 完成范围标注——O4/O5/O6/O7 已裁决推迟（v1 遗留/安全专项）、O1/O2/O8 归入本程序 B11 落地中；新增 §5 落地程序范围标注。
- 2026-09-13（B11 Rust+docs 轨）：O2 关闭（env 命名规范表归档 + Rust 采用 `MIRRI_SHELL_PATH` 主名/别名模式）、O8 关闭（Rust Windows 壳回退修复，真机验证留待用户）、O1 部分推进（Rust 面核验通过，web 接线随 T3 另批）；D-03 核验结论（Rust 无 mergeStderr 同类问题）与 D-19/D-20/D-21 文档站勘误落地，详见 context §6。
- 2026-09-13（B11/T3 web 轨）：O1 关闭——web 终端面板接线落地（detail 层挂载 + ChatHeader/MobileTopBar 入口 + i18n + additive terminalSinks 修复 v3 单连接劫持隐患，context §6.5）；A1 终端实跑验收留 B12 统筹。
