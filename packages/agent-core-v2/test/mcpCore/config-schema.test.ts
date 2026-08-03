import { describe, expect, it } from 'vitest';

import {
  McpServerConfigSchema,
  McpServerHttpConfigSchema,
  McpServerSseConfigSchema,
  McpServerStdioConfigSchema,
} from '#/mcpCore/config-schema';

describe('McpServerConfigSchema', () => {
  describe('auth field', () => {
    it('should accept auth: "oauth" on an stdio config', () => {
      const parsed = McpServerStdioConfigSchema.safeParse({
        transport: 'stdio',
        command: 'npx',
        auth: 'oauth',
      });
      expect(parsed.success).toBe(true);
      if (parsed.success) {
        expect(parsed.data.auth).toBe('oauth');
      }
    });

    it('should accept auth: "oauth" on an http config', () => {
      const parsed = McpServerHttpConfigSchema.safeParse({
        transport: 'http',
        url: 'https://example.com/mcp',
        auth: 'oauth',
      });
      expect(parsed.success).toBe(true);
      if (parsed.success) {
        expect(parsed.data.auth).toBe('oauth');
      }
    });

    it('should accept auth: "oauth" on an sse config', () => {
      const parsed = McpServerSseConfigSchema.safeParse({
        transport: 'sse',
        url: 'https://example.com/mcp',
        auth: 'oauth',
      });
      expect(parsed.success).toBe(true);
      if (parsed.success) {
        expect(parsed.data.auth).toBe('oauth');
      }
    });

    it('should allow auth to be omitted', () => {
      const parsed = McpServerStdioConfigSchema.safeParse({
        transport: 'stdio',
        command: 'npx',
      });
      expect(parsed.success).toBe(true);
      if (parsed.success) {
        expect(parsed.data.auth).toBeUndefined();
      }
    });

    it('should reject auth values other than "oauth"', () => {
      const parsed = McpServerStdioConfigSchema.safeParse({
        transport: 'stdio',
        command: 'npx',
        auth: 'basic',
      });
      expect(parsed.success).toBe(false);
    });

    it('should surface auth hint when parsing oauth-config MCP server through McpServerConfigSchema', () => {
      const parsed = McpServerConfigSchema.safeParse({
        transport: 'http',
        url: 'https://example.com/mcp',
        auth: 'oauth',
      });
      expect(parsed.success).toBe(true);
      if (parsed.success) {
        expect(parsed.data.auth).toBe('oauth');
      }
    });
  });
});
