/**
 * Numeric string converter.
 *
 * Converts string values to numbers/integers when the JSON Schema
 * expects numeric types. This handles the common case where AI models
 * serialize numeric arguments as strings (e.g., "line_offset": "178"
 * instead of line_offset: 178).
 */

import type { ToolArgConverter, ToolArgConverterContext } from '../types';

/**
 * Check if a schema defines a numeric type (integer or number).
 * Handles anyOf with numeric branches.
 */
function isNumericSchema(schema: unknown): boolean {
  if (typeof schema !== 'object' || schema === null) {
    return false;
  }

  const s = schema as Record<string, unknown>;

  // Direct type check
  if (s['type'] === 'integer' || s['type'] === 'number') {
    return true;
  }

  // Check anyOf branches
  if (Array.isArray(s['anyOf'])) {
    return s['anyOf'].some((branch: unknown) => isNumericSchema(branch));
  }

  return false;
}

/**
 * Try to convert a string value to a number based on schema type.
 * Returns the original value if conversion is not applicable or fails.
 */
function convertValue(value: unknown, schema: unknown): unknown {
  if (typeof value !== 'string') {
    return value;
  }

  if (!isNumericSchema(schema)) {
    return value;
  }

  const trimmed = value.trim();

  // Check if it's a valid number string
  if (/^-?\d+(\.\d+)?$/.test(trimmed)) {
    const num = Number(trimmed);
    if (!Number.isNaN(num)) {
      const s = schema as Record<string, unknown>;
      // For integer type, ensure it's actually an integer
      if (s['type'] === 'integer' && !Number.isInteger(num)) {
        return value;
      }
      return num;
    }
  }

  return value;
}

/**
 * Recursively convert argument values based on JSON Schema definitions.
 */
function convertArgsBySchema(schema: unknown, args: unknown): unknown {
  // Handle null and undefined
  if (args === null || args === undefined) {
    return args;
  }

  // If schema is not an object, return args as-is
  if (typeof schema !== 'object' || schema === null) {
    return args;
  }

  const s = schema as Record<string, unknown>;

  // If args is not an object, try direct conversion
  if (typeof args !== 'object') {
    return convertValue(args, schema);
  }

  // If args is an array and schema defines items
  if (Array.isArray(args) && s['items']) {
    return args.map((item) => convertArgsBySchema(s['items'], item));
  }

  // If args is an object and schema defines properties
  if (!Array.isArray(args) && s['properties']) {
    const properties = s['properties'] as Record<string, unknown>;
    const result: Record<string, unknown> = {};

    for (const [key, value] of Object.entries(args as Record<string, unknown>)) {
      const propertySchema = properties[key];
      if (propertySchema) {
        result[key] = convertArgsBySchema(propertySchema, value);
      } else {
        result[key] = value;
      }
    }

    return result;
  }

  // For other cases, return args as-is
  return args;
}

/**
 * Check if a schema contains any numeric type definitions (recursively).
 */
function schemaHasNumericType(schema: unknown): boolean {
  if (typeof schema !== 'object' || schema === null) {
    return false;
  }

  const s = schema as Record<string, unknown>;

  // Check current node
  if (s['type'] === 'integer' || s['type'] === 'number') {
    return true;
  }

  // Check anyOf
  if (Array.isArray(s['anyOf']) && s['anyOf'].some((b: unknown) => schemaHasNumericType(b))) {
    return true;
  }

  // Check properties recursively
  if (s['properties'] && typeof s['properties'] === 'object') {
    const props = s['properties'] as Record<string, unknown>;
    if (Object.values(props).some((v) => schemaHasNumericType(v))) {
      return true;
    }
  }

  // Check items
  if (s['items'] && schemaHasNumericType(s['items'])) {
    return true;
  }

  return false;
}

/**
 * Converter that transforms string values to numbers when schema expects numeric types.
 */
export class NumericStringConverter implements ToolArgConverter {
  readonly name = 'numeric-string';

  canConvert(ctx: ToolArgConverterContext): boolean {
    return schemaHasNumericType(ctx.toolParameters);
  }

  convert(ctx: ToolArgConverterContext): unknown {
    return convertArgsBySchema(ctx.toolParameters, ctx.args);
  }
}
