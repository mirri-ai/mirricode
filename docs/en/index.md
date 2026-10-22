---
layout: home
hero:
  name: Mirri Code
  text: The Next-Gen Agent That Lives on Your Desktop
  tagline: Reads your codebase, plans, executes, and verifies — end to end. Spawns parallel subagents, extends through MCP and Skills. Ships as a desktop app with a Rust core inside — faster, leaner, and natively yours.
  actions:
    - theme: brand
      text: Download Desktop
      link: https://github.com/mirri-ai/mirricode/releases/latest
    - theme: alt
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
  - icon: 🖥️
    title: Desktop app, zero setup
    details: One-click installers for macOS, Windows, and Linux (signed and notarized on macOS) — no runtime to preinstall. The same Rust engine also ships as a CLI — mirri web serves the full interface locally, mirri print runs one prompt and exits.
    link: guides/getting-started
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
    title: Rust core, light on resources
    details: The engine ships as a single Rust binary with zero Node.js runtime — under 200MB of memory in daily use (~150MB measured with an active session). It stays comfortable on modest machines and long sessions alike.
    link: guides/getting-started
quickstart:
  title: Get started in one line
  lede: The desktop app is the primary channel. The CLI is the same Rust engine without the window — install it with one command, then start it as a local server and work in your browser.
  copyHint: Copy
  copiedHint: Copied
  steps:
    - label: macOS / Linux
      prompt: '$'
      command: 'curl -fsSL https://mirricode.com/install.sh | bash'
    - label: Windows (PowerShell)
      prompt: 'PS>'
      command: 'irm https://mirricode.com/install.ps1 | iex'
    - label: Start the web UI
      prompt: '$'
      command: 'mirri web'
      variant: run
  moreText: Read the full install guide
  moreLink: guides/getting-started
---
