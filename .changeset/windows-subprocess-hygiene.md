---
"@mirri-ai/release": patch
---

Fix two Windows subprocess issues. Console windows no longer flash: every production spawn in the rust core now goes through a shared helper that sets the Windows `CREATE_NO_WINDOW` flag, and the desktop shell's detached spawns (server daemon, open-in-app launches) pass `windowsHide`. PTY terminals no longer leave orphaned grandchildren: on Windows the child process tree is attached to a kill-on-close Job Object (with a snapshot-based tree-kill fallback), and killing an already-exited terminal is now a success instead of surfacing an error to the client. A CI gate (`pnpm check:spawn`) now rejects raw `Command::new` calls in production rust code so new spawns cannot regress unnoticed. Job-Object and tree-kill behavior is compile-verified for Windows; live behavior still needs confirmation on a real Windows machine.
