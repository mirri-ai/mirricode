import { describe, expect, it } from 'vitest';

import { convertArgsBySchema } from '../../src/tools/arg-converter';

describe('convertArgsBySchema', () => {
  // 1.1 基本转换：integer类型
  it('Should convert string "10" to integer when schema expects integer', () => {
    const schema = {
      type: 'object',
      properties: { offset: { type: 'integer' } },
    };
    expect(convertArgsBySchema(schema, { offset: '10' })).toEqual({ offset: 10 });
  });

  // 1.2 基本转换：number类型
  it('Should convert string "3.14" to number when schema expects number', () => {
    const schema = {
      type: 'object',
      properties: { value: { type: 'number' } },
    };
    expect(convertArgsBySchema(schema, { value: '3.14' })).toEqual({ value: 3.14 });
  });

  // 1.3 负数转换
  it('Should convert negative string "-5" to integer when schema expects integer', () => {
    const schema = {
      type: 'object',
      properties: { offset: { type: 'integer' } },
    };
    expect(convertArgsBySchema(schema, { offset: '-5' })).toEqual({ offset: -5 });
  });

  // 1.4 保持字符串不变
  it('Should keep string unchanged when schema expects string', () => {
    const schema = {
      type: 'object',
      properties: { path: { type: 'string' } },
    };
    expect(convertArgsBySchema(schema, { path: 'hello' })).toEqual({ path: 'hello' });
  });

  // 1.5 保持非数字字符串不变
  it('Should keep non-numeric string unchanged when schema expects integer', () => {
    const schema = {
      type: 'object',
      properties: { offset: { type: 'integer' } },
    };
    expect(convertArgsBySchema(schema, { offset: 'abc' })).toEqual({ offset: 'abc' });
  });

  // 1.6 保持已经是数字的值不变
  it('Should keep number unchanged when schema expects integer', () => {
    const schema = {
      type: 'object',
      properties: { offset: { type: 'integer' } },
    };
    expect(convertArgsBySchema(schema, { offset: 10 })).toEqual({ offset: 10 });
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
    expect(convertArgsBySchema(schema, { offset: '10' })).toEqual({ offset: 10 });
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
    expect(convertArgsBySchema(schema, { config: { timeout: '30' } })).toEqual({
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
    expect(convertArgsBySchema(schema, { ids: ['1', '2', '3'] })).toEqual({ ids: [1, 2, 3] });
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
    expect(convertArgsBySchema(schema, input)).toEqual(expected);
  });

  // 1.11 不误转换版本号
  it('Should NOT convert version string when schema expects string', () => {
    const schema = {
      type: 'object',
      properties: { version: { type: 'string' } },
    };
    expect(convertArgsBySchema(schema, { version: '123' })).toEqual({ version: '123' });
  });

  // 1.12 处理null
  it('Should handle null gracefully', () => {
    const schema = { type: 'object', properties: {} };
    expect(convertArgsBySchema(schema, null)).toBeNull();
  });

  // 1.13 处理undefined
  it('Should handle undefined gracefully', () => {
    const schema = { type: 'object', properties: {} };
    expect(convertArgsBySchema(schema, undefined)).toBeUndefined();
  });

  // 1.14 处理空对象
  it('Should handle empty object', () => {
    const schema = { type: 'object', properties: {} };
    expect(convertArgsBySchema(schema, {})).toEqual({});
  });

  // 1.15 处理schema没有properties的情况
  it('Should pass through args when schema has no properties', () => {
    const schema = { type: 'object' };
    expect(convertArgsBySchema(schema, { any: 'value' })).toEqual({ any: 'value' });
  });
});
