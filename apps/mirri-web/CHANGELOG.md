# @mirri-ai/mirri-web

## 0.1.4

### Patch Changes

- [#102](https://github.com/mirri-ai/mirricode/pull/102) [`a73f48b`](https://github.com/mirri-ai/mirricode/commit/a73f48b9c9f6e7888d04c09ad1b3bcaa7aee3e1f) Thanks [@im-bravo](https://github.com/im-bravo)! - Optimize the unit formatting of the context usage display: switch from 1000-based to 1024-based units (262144 → "256k"), use ceiled whole-number percentages, and share a single formatter across TUI and web.

- [#102](https://github.com/mirri-ai/mirricode/pull/102) [`a73f48b`](https://github.com/mirri-ai/mirricode/commit/a73f48b9c9f6e7888d04c09ad1b3bcaa7aee3e1f) Thanks [@im-bravo](https://github.com/im-bravo)! - web: Fix the sidebar resize handle being covered by the chat composer background.

- [#102](https://github.com/mirri-ai/mirricode/pull/102) [`a73f48b`](https://github.com/mirri-ai/mirricode/commit/a73f48b9c9f6e7888d04c09ad1b3bcaa7aee3e1f) Thanks [@im-bravo](https://github.com/im-bravo)! - web: Fix mobile safe-area handling, including the composer floating above the on-screen keyboard on iOS, doubled landscape insets, the PWA top bar under the notch, and toasts overlapping the composer as it grows.

- [#89](https://github.com/mirri-ai/mirricode/pull/89) [`bcba690`](https://github.com/mirri-ai/mirricode/commit/bcba69090a4c447de2202112478db63b7198a107) Thanks [@im-bravo](https://github.com/im-bravo)! - Port from kimi-code [#1587](https://github.com/mirri-ai/mirricode/issues/1587): feat(web): cap markdown table column width at 700px

- [#99](https://github.com/mirri-ai/mirricode/pull/99) [`1f74d21`](https://github.com/mirri-ai/mirricode/commit/1f74d211cf00f29108d88bd4fa4232efb51fcdb3) Thanks [@im-bravo](https://github.com/im-bravo)! - Port from kimi-code [#1606](https://github.com/mirri-ai/mirricode/issues/1606): fix(web): restore the goal card after a page refresh

- [#95](https://github.com/mirri-ai/mirricode/pull/95) [`4c567d8`](https://github.com/mirri-ai/mirricode/commit/4c567d8ac172cfb0bb14cb0c32dfdfb2b2b572a8) Thanks [@im-bravo](https://github.com/im-bravo)! - Port from kimi-code [#1615](https://github.com/mirri-ai/mirricode/issues/1615): chore(web): drop the /help, /model, /provider, and /permission slash commands

- [#96](https://github.com/mirri-ai/mirricode/pull/96) [`606fb58`](https://github.com/mirri-ai/mirricode/commit/606fb586b37ad94b5528459d0aae03bc60277f99) Thanks [@im-bravo](https://github.com/im-bravo)! - Port from kimi-code [#1669](https://github.com/mirri-ai/mirricode/issues/1669): fix(web): refine goal mode controls

  Refines goal mode controls in web UI for better UX with animated expand/collapse, budget-aware progress indicator, and design-system confirmation dialog. Adds target and pause icons to mirri-web icon set.

- [#102](https://github.com/mirri-ai/mirricode/pull/102) [`a73f48b`](https://github.com/mirri-ai/mirricode/commit/a73f48b9c9f6e7888d04c09ad1b3bcaa7aee3e1f) Thanks [@im-bravo](https://github.com/im-bravo)! - web: Refresh the model catalog for all providers when opening the model picker, so newly available models always show up.

## 0.1.3

### Patch Changes

- [#77](https://github.com/mirri-ai/mirricode/pull/77) [`be1b3de`](https://github.com/mirri-ai/mirricode/commit/be1b3ded79ecdcc2803ee1c6a426249e915c7e94) Thanks [@im-bravo](https://github.com/im-bravo)! - Port from kimi-code [#1552](https://github.com/mirri-ai/mirricode/issues/1552): fix(web): keep ReadMediaFile media rendering after session resume

  After a session resume or reload, the REST snapshot now passes media content
  (image/video/audio) through as raw content parts instead of flattening them to
  text. The web layer's media URL parser was updated to handle both the mapped
  protocol types (from REST snapshots) and the original kosong types (from the
  live event stream).

- [#72](https://github.com/mirri-ai/mirricode/pull/72) [`450d155`](https://github.com/mirri-ai/mirricode/commit/450d155ceca1521792a85aa58956458d2b3d1fb7) Thanks [@im-bravo](https://github.com/im-bravo)! - Port from kimi-code [#1556](https://github.com/mirri-ai/mirricode/issues/1556): feat(web): type absolute paths directly in the workspace picker

- [#79](https://github.com/mirri-ai/mirricode/pull/79) [`455b19e`](https://github.com/mirri-ai/mirricode/commit/455b19e6dbde083abfe825faedea5783a61a5d54) Thanks [@im-bravo](https://github.com/im-bravo)! - Port from kimi-code [#1567](https://github.com/mirri-ai/mirricode/issues/1567): add server-auth test coverage for credential persistence, TTL expiry, legacy migration, and cross-tab clearing

## 0.1.2

### Patch Changes

- [#1085](https://github.com/mirri-ai/mirricode/pull/1085) [`f1fad72`](https://github.com/mirri-ai/mirricode/commit/f1fad7222ccd3f66c1cae6c5b9c009230227cd2f) - Fix stuttery streaming in the web chat by coalescing rapid token updates into a single render per frame.
