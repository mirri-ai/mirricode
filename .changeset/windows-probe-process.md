---
"@mirri-ai/release": patch
---

Fix process probing on Windows: `probe_process` previously reported every pid as not alive, so the ACP host could never discover a running desktop server (always falling back to the embedded engine) and the startup orphan sweep misjudged every running process-kind task as dead. On Windows the probe now opens the pid via `OpenProcess` and checks `GetExitCodeProcess == STILL_ACTIVE`, reporting the `GetProcessTimes` creation time as the start ticks the orphan sweep's pid-reuse detection compares.
