---
layout: home
hero:
  name: Mirri Code CLI
  text: The Next-Gen Agent That Lives in Your Terminal
  tagline: Reads your codebase, plans, executes, and verifies — end to end. Spawns parallel subagents, extends through MCP and Skills. The engine is moving to a full Rust core — faster, leaner, and natively yours.
  actions:
    - theme: brand
      text: Get started
      link: guides/getting-started
    - theme: alt
      text: GitHub
      link: https://github.com/mirri-ai/mirricode
features:
  - icon: 🧠
    title: Plan first, act on your signal
    details: Plan mode drafts the full battle plan and waits for your approval before touching a file. Every tool call is approvable, while YOLO / Auto modes let you dial in exactly how much rope to give — safety and speed, on your terms.
    link: guides/interaction
  - icon: 🛰️
    title: Subagents, in parallel
    details: The main agent decomposes work and dispatches coder / explore / plan subagents in parallel — isolated contexts that never pollute the main thread, backgrounded results that flow back automatically.
    link: customization/agents
  - icon: 🖼️
    title: Paste images and videos
    details: Error screenshots, design mockups, even a video clip — Ctrl-V and discuss. What you see is what it sees, no uploads or re-hosting. Video input is a Mirri Code signature capability.
    link: guides/interaction
  - icon: 🧩
    title: Shape it to your workflow
    details: MCP servers bring external tools, Agent Skills capture reusable expertise, Hooks fire local scripts at key moments — plus plugins, custom agent profiles, and themes. The extension surface goes deep.
    link: customization/mcp
  - icon: ⏱️
    title: Sessions as assets
    details: Resume, fork for experiments, and export sessions. Scheduled tasks and reminders survive restarts, and Goal mode keeps long-running work on target — no drift, no abandoned threads.
    link: guides/sessions
  - icon: 🦀
    title: Rust core, native TUI (evolving)
    details: The engine and terminal UI are going full Rust — a single self-contained binary, zero Node.js runtime, and a TUI rebuilt natively with ratatui. Memory footprint and cold start converge to native levels, and the terminal experience no longer pays a runtime tax.
    link: guides/getting-started
---
