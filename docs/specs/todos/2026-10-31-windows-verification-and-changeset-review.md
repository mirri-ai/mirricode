# Windows 真机验证清单 + Changeset 级别复核材料

> 生成：2026-10-31（自主交付批次完成后）
> 验证基线：`latest` 分支（7 个交付 commit + Windows 门控测试，基于 `6015a8ed4`）
> 背景：开发机为 macOS，全部 Windows 相关改动仅过了交叉编译门（`cargo xwin check --target x86_64-pc-windows-msvc`）+ 纯逻辑单测。
> **2026-10-31 更新：已新增 `#[cfg(windows)]` 门控的自动化测试（commit `7fd84f0d0`，`rust/crates/mirri-os/tests/windows_*.rs` 三个文件）——清单中标注 [自动化] 的项在 Windows 上跑 `cargo test --manifest-path rust/Cargo.toml -p mirri-os` 即自动验证；其余项仍需人工。macOS/Linux 上这些测试自动跳过（编译为空）。**

## 使用方式

**首选（自动化）**：在 Windows 真机上 clone/checkout 到 `latest` 后运行：

```
cargo test --manifest-path rust/Cargo.toml -p mirri-os
cargo test --manifest-path rust/Cargo.toml --workspace
```

第一条覆盖下表 A/B/C 组的 [自动化] 项；第二条覆盖 E 组（含 conformance 不 hang、ACP 探测、目录行为——这些测试是跨平台的，Windows 上首次真实运行即完成验证）。

**补充（人工）**：按表逐项验证 [人工] 项；每项标注对应的 GitHub issue 与修复 commit。任何一项不过，在对应 issue 下贴现象（截图/日志），回传处理。

---

## A. 会话操作不再闪出控制台窗口（#182 / commit `9dafc8e93`）

| # | 步骤 | 期望 | 方式 |
|---|---|---|---|
| A1 | rust 侧 spawn 无可见窗口（EnumWindows 断言） | 封装 spawn 的子进程无可见顶层窗口 | **[自动化]** `windows_spawn_hygiene.rs` |
| A2 | desktop 启动过程无闪窗（Electron 侧） | 除主窗口外无任何 CMD/PowerShell 一闪而过 | [人工] |
| A3 | 点击 **New Session** 无闪窗（原报告：每次弹约 3 个） | 无控制台窗口闪现 | [人工]（rust 侧已被 A1 覆盖，此项验 Electron 壳层） |
| A4 | 在 session 间切换无闪窗 | 同上 | [人工] |
| A5 | 终端面板（ConPTY 自身）不额外弹窗 | 正常出现在面板内 | [人工] |
| A6 | MCP 子进程（npx/cmd）拉起不闪窗 | 不闪窗 | [人工]（spawn 路径已被 A1 覆盖） |

> 技术背景：统一 `new_command` 封装（Windows `CREATE_NO_WINDOW`）收编了 8 处生产 spawn；Electron 侧 daemon/editor 启动加 `windowsHide: true`。最稳定的复现路径原本是「切 session → git status 3 次子进程」。

## B. 终端可用 + 关闭时清理干净（#189 / commit `9dafc8e93`）

| # | 步骤 | 期望 | 方式 |
|---|---|---|---|
| B1 | PTY 内 kill 清理整棵进程树（孙进程死亡） | `probe_process` 轮询断言孙进程死亡 | **[自动化]** `windows_pty_cleanup.rs` |
| B2 | kill 幂等（已退出/未活跃进程返回成功） | 无「failed to kill PTY child process」错误 | **[自动化]**（已有 `#[cfg(unix)]` 幂等测试 + B1 覆盖 Windows 路径） |
| B3 | 终端实时连接可用（`dir`、`echo hi` 输出流畅） | 正常交互 | [人工]（原报告的连接错误场景） |
| B4 | 终端里跑长命令后关闭再重开 | 新终端正常 | [人工] |
| B5 | 不输入命令直接关闭终端 | 无报错 | [人工]（已含在 B2 自动化语义内，人工做冒烟即可） |

> 技术背景：ConPTY 子进程挂入 `KILL_ON_JOB_CLOSE` Job Object，句柄关闭即树杀；assign 失败降级 toolhelp32 递归终止；kill 幂等化（已退出进程返回 Ok）。已知窗口：assign 之前已 spawn 的孙进程不在 Job 内（极小时序窗口，代码已注释）。

## C. ACP 模式能发现并复用桌面服务（#191 / commit `53571aea7`）

| # | 步骤 | 期望 | 方式 |
|---|---|---|---|
| C1 | probe_process 真机语义（活进程 alive + start_ticks；死 pid 判 dead） | 断言通过 | **[自动化]** `windows_probe_semantics.rs` |
| C2 | desktop_probe happy-path（pid 门 + healthz + token） | 测试通过（#191 已解除该测试的 Windows gate） | **[自动化]**（workspace 测试在 Windows 首跑即验证） |
| C3 | `mirri acp` 实际探测到运行中的 desktop server | 日志显示不降级 embedded | [人工]（端到端，需真实 desktop 进程） |
| C4 | 关闭 desktop 后 `mirri acp` 干净降级 | 无卡死/报错 | [人工]（fail-closed 路径冒烟） |

> 技术背景：`probe_process` Windows 真实现（`OpenProcess` + `GetExitCodeProcess` + `GetProcessTimes`）。已知局限：退出码恰为 259 的进程会被判活（STILL_ACTIVE 歧义，fail-safe 方向）；后台任务在 Windows 仍无进程组终止（见 #195 local-record）。

## D. 目录/模型选择体验（#183 #185 / commit `7e20853e5`，非 Windows 特有但一起验）

| # | 步骤 | 期望 |
|---|---|---|
| D1 | 断网（或屏蔽 models.dev）打开模型选择器 | **秒级**显示本地缓存目录 + 顶部失败提示（原：转圈近 9 秒） |
| D2 | 恢复网络，点刷新 | 刷新完成后新条目（如 `xiaomi-token-plan-cn`）可选 |
| D3 | 对单个 provider 点刷新（scoped） | **其余 provider 的条目仍然在**（原 bug：scoped 刷新会把整份目录截断成单条目） |

## E. 快速回归（跨平台行为，Windows 上顺手确认）

| # | 步骤 | 期望 | 方式 |
|---|---|---|---|
| E1 | `cargo test --manifest-path rust/Cargo.toml --workspace`（Windows 真机） | 全绿、有限时间内返回（A0 修复了 4 个 hang 测试；原报告环境即 Windows） | **[自动化]**（该命令本身即验证） |
| E2 | ACP 连一个带 stdio overlay MCP 的会话后退出 | 进程干净退出，`tasklist` 无残留 echo-mcp 子进程（#193 / `5ba2cf1fa`） | **[自动化]**（E1 含 conformance 套件的 pid 文件断言） |

---

## Changeset 级别复核（两项，请您拍板）

按仓库规则，`major` 必须由您确认后才能写；我目前均落 `minor`。判断依据与建议如下。

### ① `recurring` 缺省值反转（`.changeset/cron-one-shot-default.md`，现 minor）

**改动**：省略 `recurring` 字段时，从「无限循环任务」变为「一次性任务」。

| 判断角度 | 分析 |
|---|---|
| 调用方是谁 | 只有模型（`CronCreate` 工具），无公开 API/脚本契约 |
| 破坏面 | 已有的提示词/自动化若依赖「省略=循环」，行为静默改变（少触发） |
| 错误方向 | 修复的是 issue #178 论证的危险方向（缺省解析到无界消耗侧）；反向错误只是「少跑一次」，可重建 |
| minor 的理由 | 参数仍存在、传 true 行为不变；消费方是模型且历史包袱轻 |

**建议：维持 minor**。这是 fail-safe 缺省修正，不是契约破坏；若您认为「任何省略字段语义反转都算破坏性」，升 major 也站得住——您的判断优先。

### ② 移除环境变量隐式读密钥（`.changeset/explicit-provider-credentials.md`，现 minor）

**改动**：引擎不再隐式读 `OPENAI_API_KEY` 等环境变量；只认凭据库与 config 的 env 子表。这是 issue #186 的**明确产品决策**（开放产品、不绑定默认供应商）。

| 判断角度 | 分析 |
|---|---|
| 破坏面 | 依赖 shell env 的现有用户：升级后从「能用」变「清晰报错」（指路 Settings → Providers 或 `[providers.<id>.env]`） |
| 是否破坏性变更 | 功能语义上接近 major 定义（用户配置不再兼容、工作流被迫迁移），但：错误是**显性、可自解、有迁移指引**的，不是静默损坏 |
| 对比参照 | #186 issue 本身就要求这个移除；发布说明里 minor 条目已写明迁移路径 |

**建议：倾向 major 更稳妥**。理由：这是「升级后部分用户立即无法发消息」级别的变化，即便是产品意图，按 semver 惯例用户有理由预期 major 才会带来这种迁移成本；minor 发布把这种冲击混进去容易引发「为什么突然坏了」的信任损耗。若您认为发行节奏上就是想借 minor 发出（目标用户量小、都是有配置能力的开发者），维持 minor 也合理。

---

## 验证结果回传模板

```
环境：Windows 11 / Windows 10，构建 commit：cc4895c73
A1-A6：[全过 / 第 X 项异常：现象]
B1-B5：[...]
C1-C4：[...]
D1-D3：[...]
E1-E2：[...]
```
