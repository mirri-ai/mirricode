import { describe, expect, it } from 'vitest';

import { parseFloatEnv } from '../../src/config/resolve';
import { MirriError } from '../../src/errors';

function expectConfigInvalid(fn: () => unknown): void {
  try {
    fn();
  } catch (error) {
    expect(error).toBeInstanceOf(MirriError);
    expect((error as MirriError).code).toBe('config.invalid');
    return;
  }
  throw new Error('expected function to throw');
}

describe('parseFloatEnv', () => {
  it('returns undefined when unset, empty, or blank', () => {
    expect(parseFloatEnv(undefined, 'MIRRICODE_MODEL_TEMPERATURE')).toBeUndefined();
    expect(parseFloatEnv('', 'MIRRICODE_MODEL_TEMPERATURE')).toBeUndefined();
    expect(parseFloatEnv('   ', 'MIRRICODE_MODEL_TEMPERATURE')).toBeUndefined();
  });

  it('parses valid floats and integers', () => {
    expect(parseFloatEnv('0.3', 'MIRRICODE_MODEL_TEMPERATURE')).toBe(0.3);
    expect(parseFloatEnv('1', 'MIRRICODE_MODEL_TEMPERATURE')).toBe(1);
    expect(parseFloatEnv(' 0.95 ', 'MIRRICODE_MODEL_TOP_P')).toBe(0.95);
    expect(parseFloatEnv('0', 'MIRRICODE_MODEL_TEMPERATURE')).toBe(0);
  });

  it.each(['abc', '1.2.3', 'NaN', '1,5'])(
    'throws config.invalid for non-numeric value %s',
    (value) => {
      expectConfigInvalid(() => parseFloatEnv(value, 'MIRRICODE_MODEL_TEMPERATURE'));
    },
  );
});
