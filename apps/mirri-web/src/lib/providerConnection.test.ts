import { describe, expect, it } from 'vitest';
import {
  SUPPORTED_PROVIDER_TYPES,
  buildConnectionPatch,
  fromHeaderPairs,
  toHeaderPairs,
  validateConnection,
} from './providerConnection';

describe('providerConnection', () => {
  it('should list only the protocol types this engine can build when listing options', () => {
    expect([...SUPPORTED_PROVIDER_TYPES]).toEqual([
      'openai',
      'openai_responses',
      'anthropic',
      'google-genai',
      'kimi',
    ]);
    expect(SUPPORTED_PROVIDER_TYPES).not.toContain('vertexai');
    expect(SUPPORTED_PROVIDER_TYPES).not.toContain('custom');
  });

  it('should turn a header map into ordered pairs when reading the config', () => {
    expect(toHeaderPairs({ 'X-Route': 'a', Authorization: 'b' })).toEqual([
      { name: 'Authorization', value: 'b' },
      { name: 'X-Route', value: 'a' },
    ]);
  });

  it('should answer an empty list when the provider has no headers', () => {
    expect(toHeaderPairs(undefined)).toEqual([]);
  });

  it('should drop blank names and trim values when writing the config', () => {
    expect(
      fromHeaderPairs([
        { name: '  ', value: 'ignored' },
        { name: ' X-Route ', value: ' a ' },
      ]),
    ).toEqual({ 'X-Route': 'a' });
  });

  it('should answer undefined when every pair is blank', () => {
    expect(fromHeaderPairs([{ name: '', value: '' }])).toBeUndefined();
  });

  it('should reject an empty endpoint when the provider id is not first-party', () => {
    // A gateway id with a first-party TYPE still gets no engine default.
    expect(
      validateConnection({ providerId: 'gw', type: 'openai', baseUrl: '', headers: [] }),
    ).toBe('settings.models.connection.errorBaseUrlRequired');
  });

  it('should accept an empty endpoint when the provider id is first-party', () => {
    expect(
      validateConnection({ providerId: 'openai', type: 'openai', baseUrl: '', headers: [] }),
    ).toBeNull();
  });

  it('should reject a duplicated header name when validating', () => {
    expect(
      validateConnection({
        providerId: 'openai',
        type: 'openai',
        baseUrl: '',
        headers: [
          { name: 'X-A', value: '1' },
          { name: 'x-a', value: '2' },
        ],
      }),
    ).toBe('settings.models.connection.errorDuplicateHeader');
  });

  it('should reject a non-http endpoint when validating', () => {
    expect(
      validateConnection({
        providerId: 'openai',
        type: 'openai',
        baseUrl: 'ftp://example.test',
        headers: [],
      }),
    ).toBe('settings.models.connection.errorBaseUrlScheme');
  });

  it('should build a single providers patch carrying every field when saving', () => {
    const patch = buildConnectionPatch('gw', {
      type: 'openai',
      baseUrl: 'https://gw.example.test/v1',
      headers: [{ name: 'X-Route', value: 'a' }],
      thinkingOff: 'thinking_type',
    });
    expect(patch).toEqual({
      providers: {
        gw: {
          type: 'openai',
          baseUrl: 'https://gw.example.test/v1',
          customHeaders: { 'X-Route': 'a' },
          thinkingOff: 'thinking_type',
        },
      },
    });
  });

  it('should clear the optional fields with empty values when they are blank', () => {
    const patch = buildConnectionPatch('gw', {
      type: 'openai',
      baseUrl: '  ',
      headers: [{ name: '', value: '' }],
      thinkingOff: '',
    });
    expect(patch.providers.gw).toEqual({
      type: 'openai',
      baseUrl: '',
      customHeaders: {},
      thinkingOff: '',
    });
  });
});
