# 工具调用 · Grep/Glob 与 Bash 竞争态势调研（Context）

> 快照日期：2026-10-22 · 类型：context 补充调研（现状盘点 + 疼痛点 + 未立项提案）
> 本文只盘点**当前状态与痛点**，不含已裁决决策（那些见 [tool-calling-requirements.md](tool-calling-requirements.md) / [tool-calling-architecture.md](tool-calling-architecture.md)）。
> 数据源：`~/.mirri-code/v3_sessions/wd_mirricode_963bbfd6e198`（159 个 v3 会话、1034 个 wire.jsonl，660MB，2026-10-17 至 10-22 的 rust 引擎真实工作数据）。

## 1. 核心结论（结论先行）

**agent 不是不知道 Grep/Glob 存在——系统提示明确引导了 "reach for it before raw shell"（已验证存在于 v3 会话的 profile.bind 系统提示中），工具本身 0 次报错。真正的原因是：agent 的真实检索单元是"搜索 + 读片段 + 列目录 + 过滤"的复合操作，而 Grep/Glob 的能力面覆盖不了这个单元，Bash 一个调用就能完成整个单元。**

数据概览：

| 指标 | 数值 |
|------|------|
| Bash 总调用 | 41,755 |
| Grep 调用 | 5,505（Bash 的 13%） |
| Glob 调用 | 332（Bash 的 0.8%） |
| Bash 中含 rg/grep/find 的"搜索类"命令 | 22,347（占 Bash 54%） |
| Grep/Glob 工具报错 | 0 次 |
| 系统提示含 dedicated-tool 引导 | 存在（已验证） |

## 2. Bash 全量行为分类学（n=41,741）

| 类别 | 次数 | 占比 | 说明 |
|------|------|------|------|
| 搜索 rg/grep | 17,844 | 42.7% | 本应是 Grep 的领地 |
| git | 7,582 | 18.2% | Bash 合理用途 |
| 读取 sed/cat/head/tail | 6,628 | 15.9% | 本应是 Read 的领地 |
| 构建/测试 cargo | 4,388 | 10.5% | Bash 合理用途 |
| 构建/测试 js | 3,067 | 7.3% | Bash 合理用途 |
| python | 553 | 1.3% | |
| ls/tree | 507 | 1.2% | 本应是 Glob 的领地 |
| find | 452 | 1.1% | 本应是 Glob 的领地 |
| 其余（echo/process/network/文件操作） | ~700 | 1.7% | |

**Bash 时间的 61% 花在与 Grep/Glob/Read 直接竞争的操作上**（搜索 + 读取 + 列目录/找文件）。

## 3. 搜索类 Bash 的解剖（n=22,347）

这是最关键的一组数据，直接揭示 agent 检索单元的真实形态：

| 特征 | 次数 | 占比 | 对 Grep/Glob 的含义 |
|------|------|------|------|
| 带管道 `\|` | 20,843 | **93%** | Grep/Glob 不支持管道，无法替代 |
| 与 sed/cat/ls/echo 混拼同一条命令 | 20,388 | **91%** | 复合操作：搜+读+列一气呵成 |
| 多命令拼接（`;`/换行） | 13,102 | **59%** | 批量并发探查 |
| 触碰 `.worktrees/`（隔离工作区） | 8,653 | **39%** | 跨工作区导航需求 |
| 指定 ≥2 个明确文件参数 | 4,219 | 19% | Grep 的 path 是单目录，不直接支持多文件列表 |
| 触碰 `~/.cargo/registry` 等外部路径 | 578 | 2.6% | 工具沙箱外，Bash 兜底 |
| 后置 `rg -v` 过滤 | 1,898 | 8% | 需要结果二次过滤能力 |
| 纯"单命令、无管道、仓库内" | 333 | **1%** | 理论上 Grep 可完全覆盖的比例 |

**只有 1% 的搜索类 Bash 是 Grep 工具"能完全覆盖"的形态。** 其余 99% 都用到了 Grep/Glob 不具备的能力（管道、复合、多命令、多文件、外部路径）。

## 4. 搜索→读取链式行为（n=21,816）

95% 的 Bash 搜索后 3 步内跟读文件操作（15,647 次 Bash sed/cat + 5,030 次 Read）。agent 的标准检索模式是：

```
rg -n "symbol" path | head        # 1. 定位（Bash）
sed -n '90,200p' file             # 2. 读片段（Bash）
ls dir/                           # 3. 确认周边（Bash）
```

三步常被拼进同一条命令（91% 混拼率），一个 Bash 调用 = 一次完整侦察。Grep 工具即使被用（5,505 次），`output_mode=content` 结果也**不含命中行的周边代码块**——agent 仍需补一次 Read/sed。Grep 工具输出均值 3,972 字节 vs Bash 搜索均值 989 字节（agent 用 `| head` 主动裁剪，Grep 反而更费 token）。

## 5. 成本对比（输出字节 & 延迟）

| 维度 | Bash 搜索类 | Grep 工具 | Glob 工具 | Read 工具 |
|------|------------|-----------|-----------|-----------|
| 均值输出 | 989 B | 3,972 B | 726 B | 6,090 B |
| p90 输出 | 2,354 B | 9,420 B | 2,819 B | 12,666 B |
| p50 延迟 | 3 ms | 5 ms | — | — |
| p99 延迟 | 1,215 ms | 58 ms | — | — |

Grep 工具单次输出是 Bash 搜索的 **4 倍**。延迟同量级，性能不是问题——差距在输出形态与编排能力。

## 5b. 环境因素：rtk 与 worktree

- **rtk 代理**（用户自装的 CLI 输出 token 过滤器）包装了 45% 的 Bash 调用（搜索类 50%）。它进一步压低 Bash 的 token 成本——但它是**放大而非制造**能力差距：即便没有 rtk，93% 带管道的搜索命令依然只有 Bash 能做。
- **worktree 工作流**：39% 的搜索类 Bash 触碰 `.worktrees/`。`.worktrees/` 在仓库内（Grep/Glob 的 path 可达），故这不是沙箱阻挡，而是"多工作区导航"的场景需求；真正被沙箱挡的是 `~/.cargo/registry` 这类外部路径（578 次，2.6%）。

## 6. Grep 工具参数使用率（n=5,505）

| 参数 | 使用 | 占比 |
|------|------|------|
| pattern / path / output_mode | ~5,500 | ~100% |
| `-n` 行号 | 4,831 | 88% |
| `head_limit` | 2,285 | 42% |
| glob | 788 | 14% |
| `-A`/`-B`/`-C` 上下文 | 1,285 | 23% |
| `-i` | 283 | 5% |
| `output_mode=files_with_matches` | 295 | 5% |
| `type` | 19 | 0.3% |
| `include_ignored` | 5 | 0.1% |
| `offset` / `multiline` | 4 / 4 | 0.1% |
| `output_mode=count_matches` | 5 | 0.1% |

对比 Bash 里 rg 的 flag 使用：`-A/-B/-C` 1,306 次、`-l` 723 次、`-i` 407 次、`head/tail` 管道截断 7,808 次。**agent 想要"命中行 + 上下文 + 截断"的形态**，Grep 已支持前两者，但缺少"结果裁剪/过滤"的编排能力。

## 7. 根因矩阵（按证据强度排序）

| # | 根因 | 证据 | 定性 |
|---|------|------|------|
| R1 | **检索单元是复合操作**（搜+读+列+过滤），Grep/Glob 是单功能工具，单元粒度不匹配 | 93% 管道、91% 混拼、95% 搜索后读、1% 纯单搜索 | **产品能力差距（主因）** |
| R2 | **Glob 能力太弱**：无结果过滤、100 条截断、无管道 | Glob 仅 332 次；`rg --files \| rg -i` 118 次仍在 Bash | **产品能力差距（主因）** |
| R3 | **Grep 输出形态不适配**：无"命中+片段"复合返回，输出是 Bash 的 4 倍大 | Grep 输出均值 3,972B vs Bash 搜索 989B；-A/-B/-C 使用率 23% | 产品能力差距 |
| R4 | 多工作区/外部路径导航需求 | 39% .worktrees、2.6% ~/.cargo | 环境因素，部分能力差距 |
| R5 | 感知不足：不了解 Grep 的 head_limit/-A 等参数 | 1% 纯替代场景里仍用 Bash | 次要（提示/schema 可改） |
| R6 | Bash 习惯性（模型先验） | 系统提示有引导仍选 Bash；rg/grep 是模型最强先验命令 | 次要（可通过描述强化对冲） |
| R7 | rtk 代理放大 Bash 成本优势（本环境特有） | 45% Bash 带 rtk | 环境因素 |

## 8. 增强提案（未立项，供产品决策）

按"能力对齐 agent 真实检索单元"排序。所有改动落 rust core（`rust/crates/mirri-core`、`rust/crates/mirri-os`），TS 侧 maintenance-freeze 不动。

### P0 — Grep 增强：从"单搜索"到"搜索+片段"一体化

**P0-1 命中即带代码块（`include_context`）**：单次 Grep 返回"文件:行号 + 命中行 + 周边代码块"，消灭 95% 的搜索→读取链。
依据 R1/R3；落点 `rust/crates/mirri-core/src/tools/grep.rs` + `rust/crates/mirri-os/src/search.rs`；风险：输出膨胀，需与 head_limit 联动（默认命中数下调）。

**P0-2 `path` 支持多路径（数组/glob）**：19% 搜索类 Bash 指定 ≥2 个明确文件，`rg a.rs b.rs` 是刚需。
依据 R1；落点 grep.rs schema + `SearchParams.path` 类型扩展。

**P0-3 结果二次过滤（`exclude_line_pattern`，原名 `exclude_pattern`，2026-10-28 改名，见 grep-design G-ADR-2 追加 SDR）**：对命中行做后置正则排除，承接 8% 的 `| rg -v` 用法。
依据 R1（1,898 次）；落点 grep.rs + search.rs 结果过滤层。

### P1 — Glob 增强：从"列文件"到"过滤式文件发现"

**P1-1 Glob 支持 `filter` 参数**（对结果按子模式二次过滤）：承接 `rg --files | rg -i 'x'`（118 次）的标准用法。
依据 R2；落点 `rust/crates/mirri-core/src/tools/glob.rs` + mirri-os glob 层。

**P1-2 上限参数化 + mtime 排序写进描述**：`limit` 参数（默认 100 → 上调空间 200~500）。
依据 R2；落点 glob.rs。

### P2 — 感知与描述强化（低成本）

**P2-1 schema 描述重写**：示范"命中+上下文+多路径+过滤"的调用形状，明确"何时用我 vs Bash"。

**P2-2 系统提示场景化引导**：从"reach for it before raw shell"升级为"需要上下文片段用 Grep include_context；列文件+过滤用 Glob filter；多文件定向搜索用 Grep paths"。系统提示在 profile.bind，需与 profile 域联动。

### P3 — 观察项（暂不动）

- 不干预 Bash 自身（避免与 rtk 冲突）。
- Grep 输出 token 成本：若 P0-1 落地，须同时设计紧凑输出形态，避免输出变大。
- `count_matches`/`files_with_matches` 使用率极低（0.1%/5%）：等 P0-1 改变使用习惯后复测。

## 9. 落地后的验证方式

用同一分析脚本对 v3 会话复测：

1. Grep/Glob 调用占比（目标：Grep ×3~5，Glob ×5~10）
2. 搜索类 Bash 占比从 54% 降至 <30%
3. "搜索后 3 步内读文件"链式率从 95% 降至 <50%

## 10. 复现

七路分析脚本存 `.tmp/`（会话临时，不入库）：`analyze_tools.py`、`r2_taxonomy.py` ~ `r7_multi.py`。数据源 `~/.mirri-code/v3_sessions/<bucket>` 下全部 wire.jsonl 的 `context.append_loop_event`（`event.type == "tool.call"`，Bash 取 `input.command`）。

## 11. 落地后基线（2026-10-28）

P0 全部落地后的定向补测，聚焦排除场景的认知错位（决策记录见 [tools/grep-design.md](tools/grep-design.md) G-ADR-2 追加 SDR）：

- **0:537 排除错失**：5.5 天观察窗（2026-10-23~28）内 Grep 排除参数（时名 `exclude_pattern`）使用 **0 次**，同期可由它承接的 Bash 排除管道 **537 次**——其中 262 次路径型排除（`rg X | rg -v test` 式，正确入口是 glob `!` 前缀）、20 次内容型（正确入口即本参数）、其余为混合/间接形态。参数存在但模型不选，且选了也用不对（下条）。
- **两 session 证据**：session-23eb25b3 访谈——被问"如何排除测试文件"时模型第一直觉即答错（把文件维度排除答到排除参数上）；session-61a0ff40 实测——模型把路径 `flake\.nix` 当排除参数传入，参数静默无效果，模型继续错误路径（无任何"没起作用"信号）。
- **本次修复（2026-10-28）**：① 参数改名 `exclude_line_pattern`（维度入名，无别名直接替换——0 使用窗口期破坏面为零）；② `SearchResult.excluded_matches` 披露被丢弃命中数；③ glob 参数描述披露 `!` 取反与单模式限制；④ 无效果提示行（note 通道）消除静默试错。
- **复测锚点（下一观察期）**：
  1. `exclude_line_pattern` 使用率 vs 同期 Bash 排除管道量（目标：537 基线的显著承接，内容型优先）；
  2. 无效果提示行的出现率与出现后的参数纠正率（提示行是否真的把路径意图引到 glob `!`）;
  3. glob 多值组合需求（"只要 .rs 且排除 test"单 glob 放不下——若复测期仍高频出现在 Bash 管道，考虑 `glob` 支持逗号拆分多值，与 `type` 参数既有惯例对齐）。
