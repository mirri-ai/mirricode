/**
 * Converter registry.
 *
 * This module registers all available argument converters.
 * Add new converters here to make them available in the conversion pipeline.
 */

import type { ToolArgConverter } from '../types';
import { NumericStringConverter } from './numeric-string';

/** All registered converters, applied in order */
export const converters: ToolArgConverter[] = [
  new NumericStringConverter(),
];
