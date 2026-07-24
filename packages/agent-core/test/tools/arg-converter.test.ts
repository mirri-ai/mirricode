import { describe, expect, it } from 'vitest';

import { convertToolArgs } from '../../src/tools/arg-converter';
import { NumericStringConverter } from '../../src/tools/arg-converter/converters/numeric-string';
import { ParameterAliasConverter } from '../../src/tools/arg-converter/converters/parameter-alias';

describe('convertToolArgs', () => {
  // 1.1 基本转换：integer类型
  it('Should convert string "10" to integer when schema expects integer', () => {
    const schema = {
      type: 'object',
      properties: { offset: { type: 'integer' } },
    };
    expect(convertToolArgs('TestTool', schema, { offset: '10' })).toEqual({ offset: 10 });
  });

  // 1.2 基本转换：number类型
  it('Should convert string "3.14" to number when schema expects number', () => {
    const schema = {
      type: 'object',
      properties: { value: { type: 'number' } },
    };
    expect(convertToolArgs('TestTool', schema, { value: '3.14' })).toEqual({ value: 3.14 });
  });

  // 1.3 负数转换
  it('Should convert negative string "-5" to integer when schema expects integer', () => {
    const schema = {
      type: 'object',
      properties: { offset: { type: 'integer' } },
    };
    expect(convertToolArgs('TestTool', schema, { offset: '-5' })).toEqual({ offset: -5 });
  });

  // 1.4 保持字符串不变
  it('Should keep string unchanged when schema expects string', () => {
    const schema = {
      type: 'object',
      properties: { path: { type: 'string' } },
    };
    expect(convertToolArgs('TestTool', schema, { path: 'hello' })).toEqual({ path: 'hello' });
  });

  // 1.5 保持非数字字符串不变
  it('Should keep non-numeric string unchanged when schema expects integer', () => {
    const schema = {
      type: 'object',
      properties: { offset: { type: 'integer' } },
    };
    expect(convertToolArgs('TestTool', schema, { offset: 'abc' })).toEqual({ offset: 'abc' });
  });

  // 1.6 保持已经是数字的值不变
  it('Should keep number unchanged when schema expects integer', () => {
    const schema = {
      type: 'object',
      properties: { offset: { type: 'integer' } },
    };
    expect(convertToolArgs('TestTool', schema, { offset: 10 })).toEqual({ offset: 10 });
  });

  // 1.7 处理anyOf类型
  it('Should convert string to integer when schema has anyOf with integer', () => {
    const schema = {
      type: 'object',
      properties: {
        offset: {
          anyOf: [
            { type: 'integer', minimum: 1 },
            { type: 'integer', minimum: -1000, maximum: -1 },
          ],
        },
      },
    };
    expect(convertToolArgs('TestTool', schema, { offset: '10' })).toEqual({ offset: 10 });
  });

  // 1.8 处理嵌套对象
  it('Should convert nested object properties when schema defines nested types', () => {
    const schema = {
      type: 'object',
      properties: {
        config: {
          type: 'object',
          properties: {
            timeout: { type: 'integer' },
          },
        },
      },
    };
    expect(convertToolArgs('TestTool', schema, { config: { timeout: '30' } })).toEqual({
      config: { timeout: 30 },
    });
  });

  // 1.9 处理数组
  it('Should convert array items when schema defines item type', () => {
    const schema = {
      type: 'object',
      properties: {
        ids: {
          type: 'array',
          items: { type: 'integer' },
        },
      },
    };
    expect(convertToolArgs('TestTool', schema, { ids: ['1', '2', '3'] })).toEqual({
      ids: [1, 2, 3],
    });
  });

  // 1.10 Read工具的真实场景
  it('Should convert Read tool line_offset and n_lines from strings to integers', () => {
    const schema = {
      type: 'object',
      properties: {
        path: { type: 'string' },
        line_offset: {
          anyOf: [
            { type: 'integer', minimum: 1 },
            { type: 'integer', minimum: -1000, maximum: -1 },
          ],
        },
        n_lines: { type: 'integer', exclusiveMinimum: 0 },
      },
    };
    const input = { path: 'test.ts', line_offset: '178', n_lines: '50' };
    const expected = { path: 'test.ts', line_offset: 178, n_lines: 50 };
    expect(convertToolArgs('Read', schema, input)).toEqual(expected);
  });

  // 1.11 不误转换版本号
  it('Should NOT convert version string when schema expects string', () => {
    const schema = {
      type: 'object',
      properties: { version: { type: 'string' } },
    };
    expect(convertToolArgs('TestTool', schema, { version: '123' })).toEqual({ version: '123' });
  });

  // 1.11b 不误转换带pattern的数字字符串
  it('Should NOT convert numeric string when schema expects string with pattern', () => {
    const schema = {
      type: 'object',
      properties: {
        zipCode: { type: 'string', pattern: '^\\d{5}$' },
        phone: { type: 'string', pattern: '^\\d{10}$' },
      },
    };
    const input = { zipCode: '12345', phone: '13800138000' };
    expect(convertToolArgs('TestTool', schema, input)).toEqual(input);
  });

  // 1.12 处理null
  it('Should handle null gracefully', () => {
    const schema = { type: 'object', properties: {} };
    expect(convertToolArgs('TestTool', schema, null)).toBeNull();
  });

  // 1.13 处理undefined
  it('Should handle undefined gracefully', () => {
    const schema = { type: 'object', properties: {} };
    expect(convertToolArgs('TestTool', schema, undefined)).toBeUndefined();
  });

  // 1.14 处理空对象
  it('Should handle empty object', () => {
    const schema = { type: 'object', properties: {} };
    expect(convertToolArgs('TestTool', schema, {})).toEqual({});
  });

  // 1.15 处理schema没有properties的情况
  it('Should pass through args when schema has no properties', () => {
    const schema = { type: 'object' };
    expect(convertToolArgs('TestTool', schema, { any: 'value' })).toEqual({ any: 'value' });
  });
});

describe('NumericStringConverter.canConvert', () => {
  const converter = new NumericStringConverter();

  it('Should return true when schema has integer type', () => {
    const schema = {
      type: 'object',
      properties: { offset: { type: 'integer' } },
    };
    expect(converter.canConvert({ toolName: 'Test', toolParameters: schema, args: {} })).toBe(true);
  });

  it('Should return true when schema has number type', () => {
    const schema = {
      type: 'object',
      properties: { value: { type: 'number' } },
    };
    expect(converter.canConvert({ toolName: 'Test', toolParameters: schema, args: {} })).toBe(true);
  });

  it('Should return true when schema has anyOf with numeric branch', () => {
    const schema = {
      type: 'object',
      properties: {
        offset: {
          anyOf: [
            { type: 'integer', minimum: 1 },
            { type: 'string' },
          ],
        },
      },
    };
    expect(converter.canConvert({ toolName: 'Test', toolParameters: schema, args: {} })).toBe(true);
  });

  it('Should return false when schema has only string types', () => {
    const schema = {
      type: 'object',
      properties: {
        path: { type: 'string' },
        name: { type: 'string' },
      },
    };
    expect(converter.canConvert({ toolName: 'Test', toolParameters: schema, args: {} })).toBe(false);
  });

  it('Should return false when schema has no properties', () => {
    const schema = { type: 'object' };
    expect(converter.canConvert({ toolName: 'Test', toolParameters: schema, args: {} })).toBe(false);
  });

  it('Should return true when nested schema has numeric type', () => {
    const schema = {
      type: 'object',
      properties: {
        config: {
          type: 'object',
          properties: {
            timeout: { type: 'integer' },
          },
        },
      },
    };
    expect(converter.canConvert({ toolName: 'Test', toolParameters: schema, args: {} })).toBe(true);
  });
});

describe('ParameterAliasConverter', () => {
  const converter = new ParameterAliasConverter();

  // Schema resembling the Read tool's real parameter shape.
  const readSchema = {
    type: 'object',
    properties: {
      path: { type: 'string' },
      line_offset: {
        anyOf: [
          { type: 'integer', minimum: 1 },
          { type: 'integer', minimum: -1000, maximum: -1 },
        ],
      },
      n_lines: { type: 'integer', exclusiveMinimum: 0 },
    },
  };

  // 2.1 offset → line_offset
  it('Should remap offset to line_offset when schema defines line_offset', () => {
    expect(
      converter.convert({ toolName: 'Read', toolParameters: readSchema, args: { offset: 42 } }),
    ).toEqual({ line_offset: 42 });
  });

  // 2.2 limit → n_lines
  it('Should remap limit to n_lines when schema defines n_lines', () => {
    expect(
      converter.convert({ toolName: 'Read', toolParameters: readSchema, args: { limit: 50 } }),
    ).toEqual({ n_lines: 50 });
  });

  // 2.3 both aliases at once
  it('Should remap multiple aliases at once', () => {
    expect(
      converter.convert({
        toolName: 'Read',
        toolParameters: readSchema,
        args: { path: 'test.ts', offset: 10, limit: 50 },
      }),
    ).toEqual({ path: 'test.ts', line_offset: 10, n_lines: 50 });
  });

  // 2.4 drop alias when canonical name is already present with same value
  it('Should drop alias when canonical name is already present with same value', () => {
    expect(
      converter.convert({
        toolName: 'Read',
        toolParameters: readSchema,
        args: { line_offset: 5, offset: 5 },
      }),
    ).toEqual({ line_offset: 5 });
  });

  // 2.4b canonical name wins when values conflict
  it('Should keep canonical value and drop alias when both present with different values', () => {
    // LLM sent both "line_offset: 5" and "offset: 10". The canonical name is
    // authoritative — the model explicitly used the schema-defined parameter
    // name, so its value wins. The alias is a hallucination residual.
    expect(
      converter.convert({
        toolName: 'Read',
        toolParameters: readSchema,
        args: { line_offset: 5, offset: 10 },
      }),
    ).toEqual({ line_offset: 5 });
  });

  // 2.5 do not remap when schema has no matching canonical name
  it('Should NOT remap alias when schema does not define the canonical name', () => {
    const schema = {
      type: 'object',
      properties: { path: { type: 'string' } },
    };
    expect(
      converter.convert({ toolName: 'Read', toolParameters: schema, args: { offset: 10 } }),
    ).toEqual({ offset: 10 });
  });

  // 2.5b do not remap when tool is not in the alias registry
  it('Should NOT remap alias when tool name is not registered for alias conversion', () => {
    // Same schema as Read, but the tool name is different — no alias
    // conversion should happen.
    expect(
      converter.convert({ toolName: 'OtherTool', toolParameters: readSchema, args: { offset: 10 } }),
    ).toEqual({ offset: 10 });
  });

  // 2.5c canConvert returns false for unregistered tool name
  it('Should return false from canConvert when tool name is not registered', () => {
    expect(
      converter.canConvert({ toolName: 'OtherTool', toolParameters: readSchema, args: { offset: 10 } }),
    ).toBe(false);
  });

  // 2.5d pipeline does not remap for unregistered tool name
  it('Should NOT remap alias through convertToolArgs when tool name is not registered', () => {
    expect(convertToolArgs('OtherTool', readSchema, { offset: 42 })).toEqual({ offset: 42 });
  });

  // 2.6 canConvert returns false when args is not an object
  it('Should return false from canConvert when args is null', () => {
    expect(converter.canConvert({ toolName: 'Read', toolParameters: readSchema, args: null })).toBe(
      false,
    );
  });

  // 2.7 canConvert returns false when schema has no properties
  it('Should return false from canConvert when schema has no properties', () => {
    expect(
      converter.canConvert({ toolName: 'Test', toolParameters: { type: 'object' }, args: {} }),
    ).toBe(false);
  });

  // 2.8 canConvert returns false when no aliases are present
  it('Should return false from canConvert when args contains only valid keys', () => {
    expect(
      converter.canConvert({
        toolName: 'Read',
        toolParameters: readSchema,
        args: { path: 'test.ts', line_offset: 10 },
      }),
    ).toBe(false);
  });
});

describe('convertToolArgs with alias + numeric pipeline', () => {
  const readSchema = {
    type: 'object',
    properties: {
      path: { type: 'string' },
      line_offset: {
        anyOf: [
          { type: 'integer', minimum: 1 },
          { type: 'integer', minimum: -1000, maximum: -1 },
        ],
      },
      n_lines: { type: 'integer', exclusiveMinimum: 0 },
    },
  };

  // 3.1 alias + string-to-number in one pipeline pass
  it('Should remap offset and convert string value to integer in one pass', () => {
    expect(convertToolArgs('Read', readSchema, { offset: '42' })).toEqual({ line_offset: 42 });
  });

  // 3.2 both aliases + both type-converted
  it('Should remap and type-convert both offset and limit', () => {
    expect(convertToolArgs('Read', readSchema, { path: 'f.ts', offset: '10', limit: '50' })).toEqual(
      { path: 'f.ts', line_offset: 10, n_lines: 50 },
    );
  });
});
