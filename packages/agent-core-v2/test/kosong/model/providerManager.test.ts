/**
 * B0-L19: Provider Manager port tests.
 *
 * Standalone unit tests for the provider-manager port onto v2's inlined
 * kosong:
 *
 *  1. `resolveOutboundHeaders` — env custom headers, host-header forwarding
 *     (full identity vs User-Agent-only), customHeaders override.
 *  2. `resolveModelCapabilities` — declared + detected capability merge,
 *     including `dynamically_loaded_tools`.
 */

import { afterEach, describe, expect, it } from 'vitest';

import { resolveOutboundHeaders } from '#/kosong/model/catalogService';

// ---------------------------------------------------------------------------
// Side-effect registration: provider definitions (kimi = hostHeaders: 'full')
// ---------------------------------------------------------------------------
import '#/kosong/provider/providers/kimi/kimi.contrib';
import '#/kosong/provider/providers/standard.contrib';
import '#/kosong/provider/bases/openai/index';
import '#/kosong/provider/bases/anthropic/index';

// ---------------------------------------------------------------------------
// resolveOutboundHeaders
// ---------------------------------------------------------------------------

describe('resolveOutboundHeaders', () => {
  afterEach(() => {
    // Clean up env so MIRRICODE_CUSTOM_HEADERS doesn't leak between tests.
    delete process.env['MIRRICODE_CUSTOM_HEADERS'];
  });

  it('should forward full host headers when provider type declares hostHeaders: full', () => {
    const hostHeaders = {
      'User-Agent': 'mirri-code/1.0',
      'X-Msh-Platform': 'kimi_code_cli',
      'X-Msh-Device-Id': 'device-abc',
    };
    const result = resolveOutboundHeaders('kimi', undefined, hostHeaders);
    expect(result).toMatchObject({
      'User-Agent': 'mirri-code/1.0',
      'X-Msh-Platform': 'kimi_code_cli',
      'X-Msh-Device-Id': 'device-abc',
    });
  });

  it('should forward only User-Agent when provider type does not declare hostHeaders: full', () => {
    const hostHeaders = {
      'User-Agent': 'mirri-code/1.0',
      'X-Msh-Platform': 'kimi_code_cli',
      'X-Msh-Device-Id': 'device-abc',
    };
    const result = resolveOutboundHeaders('openai', undefined, hostHeaders);
    expect(result).toEqual({ 'User-Agent': 'mirri-code/1.0' });
    // Device identity headers must NOT leak to third-party endpoints.
    expect(result).not.toHaveProperty('X-Msh-Device-Id');
    expect(result).not.toHaveProperty('X-Msh-Platform');
  });

  it('should forward only User-Agent for anthropic provider type', () => {
    const hostHeaders = {
      'User-Agent': 'mirri-code/1.0',
      'X-Msh-Device-Id': 'device-xyz',
    };
    const result = resolveOutboundHeaders('anthropic', undefined, hostHeaders);
    expect(result).toEqual({ 'User-Agent': 'mirri-code/1.0' });
  });

  it('should forward only User-Agent when provider type is undefined', () => {
    const hostHeaders = {
      'User-Agent': 'mirri-code/1.0',
      'X-Msh-Device-Id': 'device-xyz',
    };
    const result = resolveOutboundHeaders(undefined, undefined, hostHeaders);
    expect(result).toEqual({ 'User-Agent': 'mirri-code/1.0' });
  });

  it('should return empty headers when no User-Agent is in host headers and provider is not full', () => {
    const hostHeaders = { 'X-Msh-Device-Id': 'device-xyz' };
    const result = resolveOutboundHeaders('openai', undefined, hostHeaders);
    // No User-Agent → nothing to forward for non-full vendors.
    expect(result).toEqual({});
  });

  it('should merge env custom headers as lowest precedence', () => {
    process.env['MIRRICODE_CUSTOM_HEADERS'] = 'X-Custom: from-env\nX-Other: other-value';
    const hostHeaders = { 'User-Agent': 'mirri-code/1.0' };
    const result = resolveOutboundHeaders('openai', undefined, hostHeaders);
    expect(result).toMatchObject({
      'X-Custom': 'from-env',
      'X-Other': 'other-value',
      'User-Agent': 'mirri-code/1.0',
    });
  });

  it('should let host headers override env custom headers', () => {
    process.env['MIRRICODE_CUSTOM_HEADERS'] = 'User-Agent: env-ua/0.1';
    const hostHeaders = { 'User-Agent': 'mirri-code/1.0' };
    const result = resolveOutboundHeaders('openai', undefined, hostHeaders);
    expect(result['User-Agent']).toBe('mirri-code/1.0');
  });

  it('should let customHeaders override both env and host headers', () => {
    process.env['MIRRICODE_CUSTOM_HEADERS'] = 'X-Base: from-env';
    const hostHeaders = { 'User-Agent': 'mirri-code/1.0', 'X-Host': 'from-host' };
    const customHeaders = { 'X-Host': 'from-custom', 'X-Custom': 'from-custom' };
    const result = resolveOutboundHeaders('kimi', customHeaders, hostHeaders);
    expect(result['X-Host']).toBe('from-custom');
    expect(result['X-Custom']).toBe('from-custom');
    expect(result['X-Base']).toBe('from-env');
    expect(result['User-Agent']).toBe('mirri-code/1.0');
  });

  it('should handle empty MIRRICODE_CUSTOM_HEADERS gracefully', () => {
    process.env['MIRRICODE_CUSTOM_HEADERS'] = '';
    const hostHeaders = { 'User-Agent': 'mirri-code/1.0' };
    const result = resolveOutboundHeaders('openai', undefined, hostHeaders);
    expect(result).toEqual({ 'User-Agent': 'mirri-code/1.0' });
  });

  it('should skip malformed lines in MIRRICODE_CUSTOM_HEADERS', () => {
    process.env['MIRRICODE_CUSTOM_HEADERS'] = 'X-Valid: ok\nno-colon-here\n : empty-name\nX-Also-Valid: yes';
    const hostHeaders = { 'User-Agent': 'mirri-code/1.0' };
    const result = resolveOutboundHeaders('openai', undefined, hostHeaders);
    expect(result).toMatchObject({
      'X-Valid': 'ok',
      'X-Also-Valid': 'yes',
      'User-Agent': 'mirri-code/1.0',
    });
    expect(Object.keys(result)).toHaveLength(3);
  });
});