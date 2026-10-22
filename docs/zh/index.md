---
layout: home
hero:
  name: Mirri Code
  text: 跑在你桌面上的下一代 AI Agent
  tagline: 读懂你的代码库，规划、执行、验证一条龙；并行派发子 Agent，MCP 与 Skills 随心扩展。桌面应用开箱即用，Rust 内核驱动——更快、更省、更纯粹的原生体验。
  actions:
    - theme: brand
      text: 下载桌面版
      link: https://github.com/mirri-ai/mirricode/releases/latest
    - theme: alt
      text: 开始使用
      link: guides/getting-started
    - theme: alt
      text: GitHub
      link: https://github.com/mirri-ai/mirricode
features:
  - icon: 🧠
    title: 先想清楚，再动手
    details: Plan 模式先输出行动计划，等你确认才碰文件；工具调用全程可审批，YOLO / Auto 模式随场景切换放手程度，安全与效率由你定义。
    link: guides/interaction
  - icon: 🖥️
    title: 桌面应用，开箱即用
    details: macOS、Windows、Linux 安装包一键安装（macOS 已签名公证），无需预装任何运行时；同一个 Rust 内核也提供命令行：mirri web 在本地起完整界面，mirri print 跑完一次就退出。
    link: guides/getting-started
  - icon: 🛰️
    title: 子 Agent 并行作战
    details: 主 Agent 拆解任务，并行派发 coder / explore / plan 子 Agent：独立上下文互不污染，后台执行完成自动回流，探索性日志不撑爆主对话。
    link: customization/agents
  - icon: 🖼️
    title: 图像与视频直接粘贴
    details: 报错截图、设计稿、甚至一段视频，Ctrl-V 粘贴进输入框即可讨论——所见即所问，无需上传或转存。视频输入是 Mirri Code 的特色能力。
    link: guides/interaction
  - icon: 🧩
    title: 调成你的形状
    details: MCP 服务器接入外部工具，Agent Skills 沉淀可复用技能，Hooks 在关键节点触发本地脚本，还有插件、自定义 Agent 与主题——扩展面拉满。
    link: customization/mcp
  - icon: ⏱️
    title: 会话即资产
    details: 会话可恢复、可分叉试验、可导出归档；定时任务与提醒绑定会话跨重启存活，目标模式为长任务锁定方向，不跑偏、不烂尾。
    link: guides/sessions
  - icon: 🦀
    title: Rust 内核，低资源占用
    details: 引擎以单一 Rust 二进制分发，运行面零 Node.js 依赖——日常使用内存占用 200MB 以内（实测活跃会话约 150MB），低配机器、长会话场景都能稳住。
    link: guides/getting-started
quickstart:
  title: 命令行一行装好
  lede: 桌面应用是主推渠道。命令行是同一个 Rust 内核，只是没有窗口——一条命令装好，作为本地服务启动，在浏览器里干活。
  copyHint: 复制
  copiedHint: 已复制
  steps:
    - label: macOS / Linux
      prompt: '$'
      command: 'curl -fsSL https://mirricode.com/install.sh | bash'
    - label: Windows (PowerShell)
      prompt: 'PS>'
      command: 'irm https://mirricode.com/install.ps1 | iex'
    - label: 启动 Web 界面
      prompt: '$'
      command: 'mirri web'
      variant: run
  moreText: 查看完整安装指南
  moreLink: guides/getting-started
---
