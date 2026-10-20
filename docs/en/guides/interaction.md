# Interaction and input

Mirri Code's desktop app and browser UI share one interface, built around the input box and the conversation view. This page covers how to enter text, paste media, navigate the approval flow, and switch between modes.

## Input box basics

The input box accepts free-form text. Press `Enter` to send, or `Shift-Enter` / `Ctrl-J` to insert a newline. When the input box is empty, press `↑` / `↓` to browse the input history for the current working directory, including previous shell commands.

**Exiting**: close the desktop app's window; for the browser UI opened by `mirri web`, press `Ctrl-C` in the terminal to stop the server. While a response is streaming, the stop button next to the input box interrupts the current turn without ending the session.

## Pasting images and video

Mirri Code supports pasting images and video directly into the input box, so you can discuss screenshots, UI mockups, architecture diagrams, or code demos without uploading or converting files first.

**Video input is a distinctive Mirri Code capability** — you can paste a video clip and have the model analyze its content, UI flow, or code walkthrough.

How to paste:

- **macOS / Linux**: `Ctrl-V`
- **Windows**: `Alt-V`

After pasting, the input box shows a placeholder that you can edit like normal text; on submit, the placeholder is replaced with the actual content. A plain-text clipboard falls back to ordinary paste. Media support depends on the current model's multimodal capabilities (`image_in` / `video_in`), so configure a multimodal model to use it.

## Slash commands

Anything starting with `/` is treated as a slash command. Typing `/` opens a completion menu that filters in real time as you keep typing; press `Esc` to close the menu. If nothing matches, the input is sent to the agent as a regular message.

Active [Agent Skills](../customization/skills.md) are automatically registered as slash commands: ordinary external Skills are invoked with `/skill:<name>`, external sub-skills appear as dotted commands such as `/parent.child`, and built-in Skills appear directly as `/<name>` in the slash command panel. If an external skill name does not conflict with a system slash command, you can also drop the `skill:` prefix and type `/<name>` directly.

Some commands are only available when the agent is idle — you need to interrupt the current turn during streaming output or context compression before using them. Mode-toggle and query commands like `/yolo`, `/plan`, and `/btw` are always available. The menu that opens when you type `/` is the authoritative list.

## File references

Type `@` to trigger file-path completion. Selecting a path inserts its relative form into your message; the agent loads the file content directly when it reads the message. File references work in both git and non-git directories, and folder suggestions end with `/` so you can keep completing paths inside them. If the fast search helper is still downloading, Mirri Code falls back to a basic filesystem scan. Hidden paths are available, but `.git` is excluded from suggestions.

> `@` references and slash commands are two separate mechanisms: `@` gives the agent file context, while `/` invokes built-in features or Skills. A `/` typed after leading whitespace is treated as normal text, not as the slash-command menu.

## Approval flow

When the agent calls a tool that has side effects — modifying files, running commands — the UI shows an approval card for your confirmation. Approvals are not triggered in YOLO mode, nor for writes to plan files in Plan mode.

Click a button, or press `1` / `2` / `3` to choose by number.

The card typically includes an **auto-approve for this session** option; selecting it auto-approves the same kind of call for the rest of the session. For permanent rules, add allow / deny entries in [Configuration files](../configuration/config-files.md#permission).

## Mode switching

### Plan mode

In Plan mode the agent first outputs an action plan and waits for your approval before modifying any files — useful for complex or high-risk tasks.

- Toggle: `/plan`
- Clear the current plan: `/plan clear` (only while idle)

After producing a plan the agent pauses for your review — you can approve it, reject it, or ask for revisions. Exiting Plan mode requires your confirmation even if YOLO mode is also active. Auto mode is the exception: plan exits are approved automatically and marked as "Auto-approved" in the transcript.

### YOLO / Auto mode

**YOLO mode** (`/yolo`) skips the approval confirmation for almost all tool calls, making it suitable for batch tasks you know are safe. The one exception is the exit-confirmation for Plan mode.

**Auto mode** (`/auto`) is more restrained: tool approvals are handled automatically, but the agent does not ask the user clarifying questions — useful when you want unattended operation without fully disabling approvals.

::: warning
YOLO mode skips confirmation for file writes and command execution. Only use it in working directories you trust.
:::

## During streaming output

The input box stays usable while the agent is thinking or calling tools: you can keep typing and send a new message, and the stop button next to the input box interrupts the current turn.

## Next steps

- [Sessions and context](./sessions.md) — how to resume sessions, compress context, and export conversations
- [Common use cases](./use-cases.md) — prompt examples for typical tasks
