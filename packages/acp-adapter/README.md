# @mirri-ai/acp-adapter

Agent Client Protocol adapter for mirri-code. Exposes the mirri-code agent over the [Agent Client Protocol](https://agentclientprotocol.com/) so that ACP-compatible clients (editors, IDEs, custom front-ends) can drive a mirri-code session over stdio.

Part of the [Mirri Code](https://github.com/mirri-ai/mirricode) monorepo.

## Minimum usage

```ts
import { createMirriHarness } from '@mirri-ai/mirri-code-sdk';
import { runAcpServer } from '@mirri-ai/acp-adapter';

const harness = await createMirriHarness();
await runAcpServer(harness);
```

`runAcpServer` reads JSON-RPC from `process.stdin`, writes to `process.stdout`, and resolves when the client closes the connection. SIGINT and SIGTERM trigger a graceful drain that calls `harness.close()` before the process exits.

See `docs/zh/reference/mirri-acp.md` for the full capability matrix (which `Agent` methods are wired, which extensions are stubbed, image / MCP support) and `docs/zh/guides/ides.md` for Zed and JetBrains setup.

## License

MIT
