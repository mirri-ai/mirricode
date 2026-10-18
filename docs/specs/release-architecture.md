# 发布域 架构文档

> 本文档描述发布管道的最终形态架构与设计决策。当前状态与痛点清单见
> [`release-context.md`](./release-context.md)，执行状态见 [`release-ROADMAP.md`](./release-ROADMAP.md)。

## 目标

为「桌面优先」产品定位提供一条统一、可复现的发布管道：

- **单产品单版本**：一次发布 = 一个版本号，覆盖桌面、rust CLI、ACP 三类产物
- **单一分发渠道**：所有用户可下载产物挂在同一个 GitHub Release 下
- **日常发版体验不变**：贡献者仍然通过「changeset → release PR → 合入」触发发布，
  无需手工干预版本号或产物上传

## 架构

### 版本锚（Version Anchor）

新设一个 **发布锚包** `packages/release`（包名 `@mirri-ai/release`，private，不发布到 npm）：

- 包内容只有 `package.json`（持有 `version`）和 `CHANGELOG.md`（产品级 changelog）
- 它是 changesets 的**唯一版本源**：所有 changeset 都指向它，release PR 合入时只 bump 它
- changesets 需显式配置 `privatePackages: { version: true, tag: false }`，否则
  private 包不参与版本管理，锚包不会被 bump
- 它本身不构建、不测试、不发布——纯粹是「这次产品发布」的编号与说明载体

**为什么需要一个专门的锚包**：版本的本质是「一次产品发布」的编号。桌面、rust
CLI、ACP 都是这次发布里的产物，锚点不该是其中任何一个，而应该是产品本身。
锚在任何一个具体产物上（退役的 npm CLI、根 package.json、rust workspace）都会
让其余产物的版本语义依附于它。

### 版本同步（单向扇出）

```
packages/release (version)          ← 唯一源头
        │
        ├── rust/Cargo.toml workspace 版本（含 Cargo.lock）
        ├── apps/mirri-desktop/package.json 版本
        └── （其余产物版本由构建时注入，不落盘）
```

- 版本同步机制的终态能力：读锚包版本，写 rust workspace 与 desktop，支持漂移
  检测（漂移即 CI 红，运行同步脚本即修复）
- **任何一侧**（rust core / desktop / web / CLI）有用户可感知的变化，都写一个指向
  锚包的 changeset：升级 `minor`，修复 `patch`

### 发布触发链

```
changeset 合入 main
  → changesets bot 开/更新 release PR（版本 bump + 聚合 changelog）
  → 维护者合入 release PR
  → CI 检测锚包版本号变化（相对上一次发布的 tag）
  → 创建 GitHub Release vX.Y.Z（notes 取自锚包 CHANGELOG 当次条目）
  → 并行触发三类产物构建 + 文档站部署
  → 全部产物上传到该 Release
```

- **触发判定**：锚包 `package.json` 的 version 相比上次发布的 tag 发生变化即触发；
  不再依赖「npm 包发布成功」事件
- **冷启动规则**：不存在可比较的历史 tag 时视为不触发，首个版本由维护者手工
  创建 tag 引导（一次性动作）
- **tag 命名**：`vX.Y.Z`。存量 install 脚本对旧格式（包名@版本）tag 的兼容
  分支保留，新发布一律使用新格式
- release PR 的自动创建机制（changesets/action）保持不变
- **文档站与版本端点**：文档站部署改为锚包版本变化触发。产品版本端点（在线
  `latest.json`，供安装脚本与 CLI 读取）冻结在 npm 停发前的最后一个版本，不再
  前进——见 ADR-005 的存量用户语义裁决；新发布的产物清单一律以 GitHub Release
  上的 `manifest.json` 为准

### 产物矩阵

一次发布产出以下全部产物，统一挂载于同一 GitHub Release：

| 产物 | 形态 | 平台覆盖 | 签名 |
|---|---|---|---|
| 桌面安装包 | dmg、zip（macOS）；exe / AppImage / deb（其余） | macOS arm64+x64、Windows x64、Linux x64 | macOS Developer ID + notarize；Win/Linux 暂不签名 |
| rust CLI | `mirri-code-<target>.zip` + `.sha256` | macOS arm64+x64、Windows x64+arm64、Linux x64+arm64（含 musl） | macOS 签名 |
| ACP | `mirri-acp-<target>.zip` + `.sha256` | 与 rust CLI 同矩阵 | macOS 签名 |
| 下载清单 | `manifest.json`（覆盖 rust CLI 与 ACP；桌面段见 ADR-006） | — | — |
- 平台矩阵：桌面维持 4 目标；rust CLI / ACP 维持 6 目标（不随桌面收缩，
  ARM Linux 与 Windows on ARM 用户保留下载物）
- `manifest.json` 扩展为覆盖三类产物的下载清单；`platforms.<target>.filename/
  checksum` 段的 schema 对存量消费者（已发布到用户机器上的 install 脚本与
  CLI 更新逻辑）**语义冻结**，新产物走新增顶层段
- 桌面安装包是**主打分发物**，官网/文档的下载指引以桌面为先
- 桌面产物命名规范：文件名含产品名、版本、target（如
  `Mirri-Desktop-<version>-<target>.<ext>`），不再使用无版本的内部测试命名

### npm 侧退役

- `@mirri-ai/mirri-code`、`@mirri-ai/mirri-node` **停止发布新版本**（不标 deprecated，
  直接停发）
- changesets 配置将这两个包移入 ignore；仓库内它们退化为内部构建依赖
- npm 发布面整体退役：npm 发布脚本链、npm Trusted Publishing 权限、npm 包
  质量检查（publint/attw）随包退役；PR 预览发布（pkg.pr.new）的价值前提
  「试装即将发布的 npm 包」消失，一并退役
- **存量 npm 用户语义**：npm 上的历史版本永久可安装、可运行；在线版本端点冻结
  在最后一个 npm 版本，因此存量 CLI 不会提示升级到 npm 上不存在的版本（不会
  出现「被引导去装一个不存在的版本」的坏体验）；想迁移的用户通过文档指引改用
  桌面或 install.sh 安装 rust CLI

### 范围外（明确不做，另立门户）

- 桌面自动更新通道（electron-updater 真实 feed / update server）
- Windows / Linux 代码签名
- npm 告别版（deprecated 通知发布）
- 独立下载官网

## 错误处理

- **版本漂移**：版本同步的漂移检测在 CI 常规检查里运行，锚包与 rust
  workspace / desktop 版本不一致即红，修复方式是运行同步脚本
- **冷启动**：无可比较历史 tag 时不触发发布，首个版本由维护者手工创建 tag 引导
- **产物上传失败**：发布 job 允许重跑（`--clobber` 覆盖上传）；Release tag 已存在
  时复用而不重复创建
- **构建失败**：各产物构建 job 相互独立、可单独重跑；未完成的 Release 由维护者
  决定补传或删除

## 测试策略

- 版本同步的读取/写入/漂移检测保留单测（改读锚包路径后）
- 发布触发判定（锚包版本变化解析）有单测：同版本不触发、新版本触发、无历史
  tag 不触发
- 端到端验证通过一次真实发布演练完成（ROADMAP 中列为验收项）

## 决策记录

### ADR-001 发布产物收敛为 rust core 系（2026-10-18）

- **背景**：rust core 已成型为唯一维护的核心，产品定位转向桌面优先。原先以 npm
  CLI 包为中心的发布管道与新定位倒挂。
- **决策**：只发布 rust core 系产物——桌面安装包、rust CLI 二进制、ACP 二进制，
  全部通过 GitHub Release 分发。TS core 的 npm 包停止发布。
- **被否方案**：
  - 继续同步发 npm CLI 包：维护两套发布物成本高，且 npm 包用户非目标用户
  - CLI 降级维护（降频发）：半死状态比明确退役更难沟通
- **影响**：release 触发机制重造（原依赖 npm publish 事件）；存量 npm 用户无升级
  路径（语义裁决见 ADR-005）；npm 发布面（脚本链、OIDC 权限、质量检查、
  pkg.pr.new 预览）整体退役

### ADR-002 版本锚为独立锚包（2026-10-18）

- **背景**：停发 npm 后需要新的版本源头；候选为锚包、根 package.json、rust
  workspace 反向回写。三案中前两案都依赖 changesets 的 `privatePackages`
  机制（private 包默认不参与版本管理）。
- **决策**：新建 `packages/release`（`@mirri-ai/release`，private）作为唯一版本源，
  changesets 以它为锚（配 `privatePackages: { version: true, tag: false }`），
  版本单向扇出到 rust workspace 与 desktop。
- **被否方案**：
  - 根 package.json：同为 `privatePackages` 机制，但 root 包还需搭配额外的
    workspace 协议 bump 约束，且根版本号语义易与 monorepo 管理版本混淆；
    独立锚包无此叠加
  - rust workspace 做源：changesets 管不了 rust 版本，release PR 机制需重造，
    改造量大而收益与锚包相同
- **影响**：版本同步脚本、release 触发判定、`.changeset/config.json` 均需改造

### ADR-003 三类产物同 Release 分发（2026-10-18）

- **背景**：桌面安装包原先只存在于 CI 临时 artifact；rust CLI 二进制已有成熟的
  zip+sha256 打包管道但尚未接入发布编排（当前 Release 上挂的是 Node SEA 产物，
  见 ADR-004）。
- **决策**：桌面、rust CLI、ACP 三类产物挂同一 GitHub Release tag，共用一份
  `manifest.json` 清单；`platforms` 段 schema 对存量消费者语义冻结，新产物走
  新增顶层段；tag 命名统一为 `vX.Y.Z`。
- **被否方案**：
  - 桌面独立下载页：需要额外基础设施，当前阶段 GitHub Release 足够
  - ACP 不单独分发：需要 ACP 的用户被迫安装桌面或 CLI
- **影响**：release 编排需新增桌面上传 job 与 ACP 构建矩阵；桌面产物命名需
  规范化（含版本与 target）

### ADR-004 rust CLI 发布物从 Node SEA 切换为 rust 二进制（2026-10-18）

- **背景**：当前 Release 上的 CLI 原生产物由 Node.js SEA 管道产出（Node 运行时
  打包注入），与 rust 二进制管道并行存在；rust core 已是唯一维护的核心。
- **决策**：发布编排中的 CLI 原生产物切换为 rust 二进制（mirri-cli 产出的
  `mirri` 二进制，经 rust 打包管道 zip+sha256），Node SEA 管道退出发布链
  （仓库内保留构建脚本仅供历史排查）。
- **被否方案**：
  - 双轨并行（SEA + rust 同发）：双倍构建时长与维护面，且 SEA 产物与
    「rust first」的引擎门决策相悖
  - 仅发 SEA（rust 二进制只内嵌桌面）：CLI 用户拿不到独立 rust 二进制，
    install.sh 链路失去升级路径
- **影响**：release 编排的原生产物构建 job 从 SEA 管道切换为 rust 管道；
  产物名与 manifest 保持既有 `mirri-code-<target>.zip` 约定，对下载方无感

### ADR-005 存量 npm 用户版本端点冻结（2026-10-18）

- **背景**：存量 npm 安装的 CLI 会定期检查在线版本端点并提示升级；npm 停发后，
  若端点继续前进，npm 用户会被引导去安装 npm 上不存在的版本（升级必失败的
  坏体验）；若端点前进但产物只在新渠道，提示语也不指向正确渠道。存量客户端
  的检查行为已随旧版本发布、无法更新。
- **决策**：在线版本端点（`latest.json`）冻结在 npm 停发前的最后一个版本，不再
  前进。新渠道的活版本元数据以 GitHub Release 上的 `manifest.json` 为准，
  新的安装/升级工具（install 脚本、桌面）一律读 Release manifest。
- **被否方案**：
  - 端点继续前进：存量用户收到必然失败的升级指引，属于主动制造坏体验
  - 发最后一个「带迁移提示」的 npm 包：等同于告别版，已裁决范围外
    （用户决策：直接停止发布）
- **影响**：版本端点的产出逻辑与文档站部署解耦；install 脚本改读 Release
  manifest；文档需提供 npm → 新渠道的迁移指引

### ADR-006 manifest 暂不包含桌面安装包段（2026-10-18）

- **背景**：`manifest.json` 已扩展为覆盖 rust CLI 与 ACP 的下载清单，但桌面
  安装包段尚无程序化消费方——桌面下载目前是人工操作（用户从 Release 页/官网
  手动下载安装），自动更新通道属范围外另行立项。
- **决策**：manifest 暂不包含桌面安装包段；待更新通道立项时按其实际格式需求
  补清单段。
- **被否方案**：
  - 现在就实现桌面段：无程序化消费方意味着格式无法被真实需求校准，且未来
    electron-updater 需要的 feed 格式（含更新元数据）与手写的下载清单段不
    兼容，大概率返工
- **影响**：桌面安装包仅通过 GitHub Release 页直接分发；更新通道立项时需同步
  补 manifest 桌面段并定义其 schema
