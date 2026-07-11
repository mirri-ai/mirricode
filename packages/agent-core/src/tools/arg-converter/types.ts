/**
 * Types for the tool argument converter system.
 *
 * This module defines the interfaces for the converter framework,
 * enabling extensible and composable argument transformations.
 */

/**
 * Context provided to tool argument converters.
 * Contains all information needed to make conversion decisions.
 */
export interface ToolArgConverterContext {
  /** Tool name, useful for tool-specific conversions */
  readonly toolName: string;
  /** Tool's JSON Schema parameter definitions */
  readonly toolParameters: Record<string, unknown>;
  /** The arguments to convert */
  readonly args: unknown;
}

/**
 * Interface for tool argument converters.
 * Each converter handles a specific type of argument transformation.
 */
export interface ToolArgConverter {
  /** Converter name for logging and debugging */
  readonly name: string;
  /** Determine if this converter should be applied */
  canConvert(ctx: ToolArgConverterContext): boolean;
  /** Perform the conversion */
  convert(ctx: ToolArgConverterContext): unknown;
}
