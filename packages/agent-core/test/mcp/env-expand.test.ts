import { describe, expect, it } from 'vitest';

import { expandEnvString, expandEnvVars } from '../../src/mcp/env-expand';

const lookup = (vars: Record<string, string>) => (name: string): string | undefined => vars[name];

describe('expandEnvString', () => {
  it('should expand a single ${VAR} reference when the variable is set', () => {
    expect(expandEnvString('https://${HOST}/mcp', lookup({ HOST: 'example.com' }))).toBe(
      'https://example.com/mcp',
    );
  });

  it('should expand a ${env:VAR} reference when the variable is set', () => {
    expect(expandEnvString('${env:TOKEN}-suffix', lookup({ TOKEN: 'abc123' }))).toBe(
      'abc123-suffix',
    );
  });

  it('should expand multiple references in one string', () => {
    const env = lookup({ SCHEME: 'https', HOST: 'example.com', PORT: '8080' });
    expect(expandEnvString('${env:SCHEME}://${HOST}:${PORT}', env)).toBe(
      'https://example.com:8080',
    );
  });

  it('should resolve an undefined variable to an empty string', () => {
    expect(expandEnvString('pre-${MISSING}-post', lookup({}))).toBe('pre--post');
  });

  it('should return the string unchanged when it contains no references', () => {
    expect(expandEnvString('plain-text', lookup({}))).toBe('plain-text');
  });

  it('should handle variable names with underscores and digits', () => {
    expect(expandEnvString('${API_KEY_2}', lookup({ API_KEY_2: 'secret' }))).toBe('secret');
  });

  it('should not treat a leading digit in a variable name as a valid reference', () => {
    expect(expandEnvString('${1VAR}', lookup({}))).toBe('${1VAR}');
  });

  it('should leave a lone dollar sign without braces untouched', () => {
    expect(expandEnvString('cost is $5', lookup({}))).toBe('cost is $5');
  });
});

describe('expandEnvVars', () => {
  it('should recurse into nested objects and arrays while keeping keys literal', () => {
    const input = {
      mcpServers: {
        remote: {
          transport: 'http',
          url: 'https://${HOST}/mcp',
          headers: { Authorization: 'Bearer ${env:TOKEN}' },
          args: ['${env:REGION}', 'static'],
        },
      },
    };
    const env = lookup({ HOST: 'example.com', TOKEN: 'abc123', REGION: 'us-east-1' });
    expect(expandEnvVars(input, env)).toEqual({
      mcpServers: {
        remote: {
          transport: 'http',
          url: 'https://example.com/mcp',
          headers: { Authorization: 'Bearer abc123' },
          args: ['us-east-1', 'static'],
        },
      },
    });
  });

  it('should leave numbers, booleans and null untouched', () => {
    const input = { a: 1, b: true, c: null, d: 'pre-${VAR}' };
    expect(expandEnvVars(input, lookup({}))).toEqual({ a: 1, b: true, c: null, d: 'pre-' });
  });

  it('should not mutate the input object', () => {
    const input = { url: '${HOST}' };
    expandEnvVars(input, lookup({ HOST: 'example.com' }));
    expect(input).toEqual({ url: '${HOST}' });
  });

  it('should return primitives unchanged', () => {
    expect(expandEnvVars(42, lookup({}))).toBe(42);
    expect(expandEnvVars(true, lookup({}))).toBe(true);
    expect(expandEnvVars(null, lookup({}))).toBe(null);
    expect(expandEnvVars(undefined, lookup({}))).toBe(undefined);
  });

  it('should expand references inside deeply nested arrays of objects', () => {
    const input = {
      servers: [
        { url: '${A}' },
        { nested: [{ key: '${env:B}' }] },
      ],
    };
    expect(expandEnvVars(input, lookup({ A: 'a.com', B: 'b.com' }))).toEqual({
      servers: [
        { url: 'a.com' },
        { nested: [{ key: 'b.com' }] },
      ],
    });
  });
});
