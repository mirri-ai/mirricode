# @mirri-ai/mirri-desktop

## 0.2.0

### Minor Changes

- [#134](https://github.com/mirri-ai/mirricode/pull/134) [`4bc9a36`](https://github.com/mirri-ai/mirricode/commit/4bc9a36778640baad7e2f805311c6dc4ec1f6427) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Switch the Desktop backend from the v1 engine (`mirri server run`) to the v2 engine (`mirri web` / kap-server).

## 0.1.2

### Patch Changes

- [#123](https://github.com/mirri-ai/mirricode/pull/123) [`3debb68`](https://github.com/mirri-ai/mirricode/commit/3debb6877a9abd38661eb4f0c08df15b0b838bf9) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Add a hidden `--lock-name` flag to `mirri server run` so the Desktop app can run its own isolated daemon instance with a separate lock file, log file, and port range (starting at 58827). The server's session index reindex now uses a cross-process lock to avoid concurrent rebuilds when two daemons start simultaneously.

- [#123](https://github.com/mirri-ai/mirricode/pull/123) [`3debb68`](https://github.com/mirri-ai/mirricode/commit/3debb6877a9abd38661eb4f0c08df15b0b838bf9) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Open external links (target="\_blank" and window.open) in the system default browser instead of an in-app Electron window, preserving the user's browser cookies and login state.

## 0.1.1

### Patch Changes

- [#84](https://github.com/mirri-ai/mirricode/pull/84) [`d1bf93c`](https://github.com/mirri-ai/mirricode/commit/d1bf93cc01213a51559dfde184e50c055a26b98a) Thanks [@im-bravo](https://github.com/im-bravo)! - Fix desktop app icon to use Mirri branding instead of Kimi logo.
