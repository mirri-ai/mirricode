# @mirri-ai/mirri-web

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
