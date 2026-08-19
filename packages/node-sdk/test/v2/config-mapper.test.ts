import { describe, expect, it } from 'vitest';

import {
  diagnosticsToConfigDiagnostics,
  planProviderRemoval,
  removeProviderFromConfig,
  resolvedConfigToMirriConfig,
} from '#/v2/config-mapper';

describe('resolvedConfigToMirriConfig', () => {
  it('should pick v1-shaped fields out of the v2 resolved config', () => {
    const resolved = {
      providers: { openai: { apiKey: 'x' } },
      defaultModel: 'gpt-4',
      planMode: true,
      unknownDomain: 'dropped',
      telemetry: false,
    };

    const result = resolvedConfigToMirriConfig(resolved);

    expect(result).toEqual({
      providers: { openai: { apiKey: 'x' } },
      defaultModel: 'gpt-4',
      planMode: true,
      telemetry: false,
    });
    expect('unknownDomain' in result).toBe(false);
  });

  it('should drop v2-only domains that v1 does not know', () => {
    const result = resolvedConfigToMirriConfig({
      providers: {},
      cron: { schedules: [] },
    });

    expect(result.providers).toEqual({});
    expect('cron' in result).toBe(false);
  });
});

describe('diagnosticsToConfigDiagnostics', () => {
  it('should flatten structured v2 diagnostics into warning strings', () => {
    const result = diagnosticsToConfigDiagnostics([
      { domain: 'hooks', severity: 'error', message: 'bad command' },
      { domain: 'mcp', severity: 'warning', message: 'server unreachable' },
    ]);

    expect(result).toEqual({
      warnings: ['bad command', 'server unreachable'],
    });
  });

  it('should report empty warnings when there are no diagnostics', () => {
    expect(diagnosticsToConfigDiagnostics([])).toEqual({ warnings: [] });
  });
});

describe('planProviderRemoval', () => {
  it('should remove the provider, its models, and dangling defaults', () => {
    const plan = planProviderRemoval({
      providers: { a: { apiKey: 'x' }, b: { apiKey: 'y' } },
      models: {
        'a-model': { provider: 'a' },
        'b-model': { provider: 'b' },
      },
      defaultModel: 'a-model',
      defaultProvider: 'a',
      providerId: 'a',
    });

    expect(plan.providers).toEqual({ b: { apiKey: 'y' } });
    expect(plan.models).toEqual({ 'b-model': { provider: 'b' } });
    expect(plan.clearDefaultModel).toBe(true);
    expect(plan.clearDefaultProvider).toBe(true);
  });

  it('should keep the defaults when they do not point at the removed provider', () => {
    const plan = planProviderRemoval({
      providers: { a: {}, b: {} },
      models: {},
      defaultModel: 'b-model',
      defaultProvider: 'b',
      providerId: 'a',
    });

    expect(plan.clearDefaultModel).toBe(false);
    expect(plan.clearDefaultProvider).toBe(false);
  });
});

describe('removeProviderFromConfig', () => {
  it('should fold the removal into a whole MirriConfig in memory', () => {
    const config = {
      providers: { a: { apiKey: 'x' }, b: {} },
      models: { 'a-model': { provider: 'a' } },
      defaultModel: 'a-model',
      defaultProvider: 'a',
      planMode: true,
    };

    const result = removeProviderFromConfig(config as never, 'a');

    expect(result.providers).toEqual({ b: {} });
    expect(result.models).toEqual({});
    expect(result.defaultModel).toBeUndefined();
    expect(result.defaultProvider).toBeUndefined();
    expect(result.planMode).toBe(true);
  });
});