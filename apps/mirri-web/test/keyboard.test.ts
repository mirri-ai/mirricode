import { describe, expect, it } from 'vitest';
import { isCompositionKeyEvent } from '../src/lib/keyboard';

describe('isCompositionKeyEvent', () => {
  it('should return true when the event is marked as composing', () => {
    expect(isCompositionKeyEvent({ isComposing: true, keyCode: 13 })).toBe(true);
  });

  it('should return true when the keyCode is the IME placeholder 229', () => {
    expect(isCompositionKeyEvent({ isComposing: false, keyCode: 229 })).toBe(true);
  });

  it('should return false for a plain Enter keydown without IME flags', () => {
    expect(isCompositionKeyEvent({ isComposing: false, keyCode: 13 })).toBe(false);
  });

  it('should return false when no IME flags are present at all', () => {
    expect(isCompositionKeyEvent({})).toBe(false);
  });
});
