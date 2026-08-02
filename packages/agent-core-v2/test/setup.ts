/**
 * Hermetic experimental-flag state for tests: scrub ambient
 * `MIRRICODE_EXPERIMENTAL_*` env vars inherited from the developer shell
 * (e.g. a globally exported `MIRRICODE_EXPERIMENTAL_FLAG=1`) so flag-driven
 * behavior — including tool schemas embedded in `llm.tools_snapshot`
 * snapshots — stays deterministic and matches CI. Tests opt into flags
 * explicitly via service overrides or `vi.stubEnv`.
 */
for (const key of Object.keys(process.env)) {
  if (key.startsWith('MIRRICODE_EXPERIMENTAL_')) {
    delete process.env[key];
  }
}
