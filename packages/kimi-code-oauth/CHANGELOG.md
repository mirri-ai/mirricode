# @mirri-ai/v2-oauth

## 0.4.0

### Minor Changes

- [#134](https://github.com/mirri-ai/mirricode/pull/134) [`4bc9a36`](https://github.com/mirri-ai/mirricode/commit/4bc9a36778640baad7e2f805311c6dc4ec1f6427) Thanks [@mirri-ai](https://github.com/mirri-ai)! - Introduce the agent-core-v2 engine as a parallel backend that coexists with the v1 engine, plus a wire protocol bump to 1.5.

  - Add the v2 engine packages (agent-core-v2, kap-server, transcript, klient, v2-oauth, minidb, tree-sitter-bash) that port the 20 mirri features and close the 4 v2 gaps (G1-G4) with golden-test parity to v1.
  - Bump the wire protocol from 1.4 to 1.5 and register the v1.4→v1.5 wall-clock anchor migration so existing records backfill the missing anchor from create and resume timestamps.
  - web: Add a "start from" selector in the agent profiles panel so users can create a custom profile based on a builtin profile without the extends chain.

## 0.3.0

### Minor Changes

- [#2382](https://github.com/MoonshotAI/mirri-code/pull/2382) [`40172c7`](https://github.com/MoonshotAI/mirri-code/commit/40172c7ca96ca981b043b793588dd32e898979fa) Thanks [@liruifengv](https://github.com/liruifengv)! - Rework the host identity type: rename `userAgentProduct` to `productName` and add a required `platform` field, so every host explicitly declares the `X-Msh-Platform` value it reports instead of silently inheriting the CLI's. OAuth requests now also send the product User-Agent (with the optional runtime suffix), so the OAuth host can tell client families and surfaces apart.

## 0.2.2

### Patch Changes

- [#399](https://github.com/MoonshotAI/mirri-code/pull/399) [`232ed87`](https://github.com/MoonshotAI/mirri-code/commit/232ed874d41de777e6ff9c539ac22d830d0b5c3a) - Keep managed OAuth credentials scoped to their configured authentication and API endpoints.

## 0.2.1

### Patch Changes

- [#335](https://github.com/MoonshotAI/mirri-code/pull/335) [`7284f30`](https://github.com/MoonshotAI/mirri-code/commit/7284f30479142fd66b1e8a731fd00198b1e8684f) - Fix custom registry provider handling during re-import. Prevent loss of multi-provider entries and remove stale providers along with their model aliases and default model references.

## 0.2.0

### Minor Changes

- [#264](https://github.com/MoonshotAI/mirri-code/pull/264) [`42bb914`](https://github.com/MoonshotAI/mirri-code/commit/42bb9141d8ee7023639f943dd4c6a0f6c8fa8945) - Add `/provider` command for managing AI providers, support custom registry imports, and introduce a tabbed model selector.

### Patch Changes

- [#274](https://github.com/MoonshotAI/mirri-code/pull/274) [`a1dfbfe`](https://github.com/MoonshotAI/mirri-code/commit/a1dfbfeb16bcad0c2c8faa232d6d1ce4a2681d57) - Clarify Kimi Platform API key login labels and prompt details.

## 0.1.2

### Patch Changes

- [#52](https://github.com/MoonshotAI/mirri-code/pull/52) [`064343a`](https://github.com/MoonshotAI/mirri-code/commit/064343a6e565a525fbf38b3a1f70f7ff0235a5ed) - Correct the `X-Msh-Platform` header value to `mirri_code_cli`.

- [#11](https://github.com/MoonshotAI/mirri-code/pull/11) [`15b018f`](https://github.com/MoonshotAI/mirri-code/commit/15b018fc84a36a9ebde598970e5b44bebe5d68c6) - Surface API-provided error messages during feedback, usage, login, and model setup failures.
