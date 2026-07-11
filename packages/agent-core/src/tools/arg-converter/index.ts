/**
 * Tool argument converter system.
 *
 * This module provides a framework for converting tool arguments before validation.
 * It supports extensible converters that can be registered and applied in order.
 *
 * Usage:
 * ```typescript
 * import { convertToolArgs } from '../tools/arg-converter';
 *
 * const convertedArgs = convertToolArgs(toolName, tool.parameters, parsedArgs);
 * ```
 */

import type { ToolArgConverterContext } from './types';
import { converters } from './converters';

/**
 * Convert tool arguments by applying all registered converters.
 *
 * This is the main entry point for the converter system. It iterates through
 * all registered converters and applies them in order if they can convert
 * the given arguments.
 *
 * @param toolName - Name of the tool (for tool-specific conversions)
 * @param toolParameters - Tool's JSON Schema parameter definitions
 * @param args - The arguments to convert
 * @returns The converted arguments
 */
export function convertToolArgs(
  toolName: string,
  toolParameters: Record<string, unknown>,
  args: unknown,
): unknown {
  const baseCtx: ToolArgConverterContext = { toolName, toolParameters, args };

  let result = args;
  for (const converter of converters) {
    const ctx: ToolArgConverterContext = { ...baseCtx, args: result };
    if (converter.canConvert(ctx)) {
      result = converter.convert(ctx);
    }
  }

  return result;
}

export type { ToolArgConverter, ToolArgConverterContext } from './types';
