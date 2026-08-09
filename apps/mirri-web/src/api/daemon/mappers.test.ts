// apps/mirri-web/src/api/daemon/mappers.test.ts
// Catalog wire (v2) mapping + provider write-payload builders. The web UI is
// v2-only (kap-server): catalog items arrive in the flat shape
// (capabilities[]/max_context_size/reasoning + import metadata), and provider
// creation requires an explicit id + models list.

import { describe, expect, it } from 'vitest';
import {
  toAppCatalogModel,
  toAppCatalogProvider,
  toAppTask,
  toWireAddProviderBody,
  toWireImportCatalogBody,
} from './mappers';
import type { WireBackgroundTask, WireCatalogModel, WireCatalogProvider } from './wire';

describe('catalog wire (v2) mapping', () => {
  it('maps a flat v2 catalog model to the app shape', () => {
    const wire: WireCatalogModel = {
      id: 'gpt-5.2',
      name: 'GPT-5.2',
      max_context_size: 400000,
      capabilities: ['image_in', 'thinking', 'tool_use'],
      reasoning: true,
    };
    expect(toAppCatalogModel(wire)).toEqual({
      id: 'gpt-5.2',
      name: 'GPT-5.2',
      maxContextSize: 400000,
      capabilities: ['image_in', 'thinking', 'tool_use'],
      reasoning: true,
    });
  });

  it('maps a v2 catalog provider with import metadata', () => {
    const wire: WireCatalogProvider = {
      id: 'azure',
      name: 'Azure',
      wire_type: 'openai',
      guessed: false,
      needs_base_url: true,
      rejected: false,
      reject_reason: null,
      env_key: 'AZURE_RESOURCE_NAME',
      models: [
        {
          id: 'phi-4-mini-reasoning',
          name: 'Phi-4-mini-reasoning',
          max_context_size: 128000,
          capabilities: ['thinking', 'tool_use'],
          reasoning: true,
        },
      ],
    };
    expect(toAppCatalogProvider(wire)).toEqual({
      id: 'azure',
      name: 'Azure',
      wireType: 'openai',
      guessed: false,
      needsBaseUrl: true,
      rejected: false,
      rejectReason: null,
      envKey: 'AZURE_RESOURCE_NAME',
      models: [
        {
          id: 'phi-4-mini-reasoning',
          name: 'Phi-4-mini-reasoning',
          maxContextSize: 128000,
          capabilities: ['thinking', 'tool_use'],
          reasoning: true,
        },
      ],
    });
  });

  it('keeps rejected providers visible with their reason', () => {
    const wire: WireCatalogProvider = {
      id: 'bedrock',
      name: 'Amazon Bedrock',
      wire_type: null,
      guessed: false,
      needs_base_url: false,
      rejected: true,
      reject_reason: 'proprietary-sdk',
      env_key: null,
      models: [],
    };
    const app = toAppCatalogProvider(wire);
    expect(app.rejected).toBe(true);
    expect(app.rejectReason).toBe('proprietary-sdk');
    expect(app.wireType).toBeNull();
  });
});

describe('provider write-payload builders (v2)', () => {
  it('builds the add-provider payload with snake_case models', () => {
    expect(
      toWireAddProviderBody({
        id: 'custom',
        type: 'openai',
        apiKey: 'sk-test',
        baseUrl: 'https://gateway.example.test/v1',
        models: [
          {
            model: 'gpt-5.2',
            maxContextSize: 400000,
            capabilities: ['thinking', 'tool_use'],
            supportEfforts: ['low', 'high'],
            adaptiveThinking: true,
          },
        ],
      }),
    ).toEqual({
      id: 'custom',
      type: 'openai',
      api_key: 'sk-test',
      base_url: 'https://gateway.example.test/v1',
      default_model: undefined,
      models: [
        {
          model: 'gpt-5.2',
          max_context_size: 400000,
          display_name: undefined,
          capabilities: ['thinking', 'tool_use'],
          max_output_size: undefined,
          support_efforts: ['low', 'high'],
          adaptive_thinking: true,
        },
      ],
    });
  });

  it('builds the import-catalog payload', () => {
    expect(
      toWireImportCatalogBody({
        catalogId: 'azure',
        id: 'my-azure',
        apiKey: 'sk-test',
        baseUrl: 'https://azure.example.test',
      }),
    ).toEqual({
      catalog_id: 'azure',
      id: 'my-azure',
      api_key: 'sk-test',
      base_url: 'https://azure.example.test',
    });
    expect(toWireImportCatalogBody({ catalogId: 'azure' })).toEqual({
      catalog_id: 'azure',
      id: undefined,
      api_key: undefined,
      base_url: undefined,
    });
  });
});

describe('toAppTask (REST /tasks wire → app shape)', () => {
  it('carries the resolved subagent model onto the app task', () => {
    const wire: WireBackgroundTask = {
      id: 'task-1',
      session_id: 'session-1',
      kind: 'subagent',
      description: 'explore the codebase',
      status: 'running',
      created_at: '2026-01-01T00:00:00.000Z',
      subagent_type: 'coder',
      model: 'claude-sonnet',
    };
    const task = toAppTask(wire);
    expect(task.model).toBe('claude-sonnet');
    expect(task.subagentType).toBe('coder');
  });

  it('leaves model undefined when the wire task carries none', () => {
    const wire: WireBackgroundTask = {
      id: 'task-2',
      session_id: 'session-1',
      kind: 'bash',
      description: 'pnpm test',
      status: 'running',
      created_at: '2026-01-01T00:00:00.000Z',
    };
    expect(toAppTask(wire).model).toBeUndefined();
  });
});
