import { describe, expect, it } from 'vitest';
import { isCompositionKeyEvent } from '../src/lib/keyboard';
// Raw SFC source, so the composer's keydown guard can be asserted without
// mounting the component (the test suite has no component runner).
import ComposerSource from '../src/components/chat/Composer.vue?raw';

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

  it('should return true for Enter while composing (candidate confirmation must not submit)', () => {
    // The IME-confirm Enter carries isComposing: true (or 229) — the composer
    // must let it pass through to the IME, never treat it as "send".
    expect(isCompositionKeyEvent({ isComposing: true, keyCode: 13, key: 'Enter' })).toBe(true);
    expect(isCompositionKeyEvent({ isComposing: false, keyCode: 229, key: 'Enter' })).toBe(true);
  });

  it('should return true for digits and space while composing (candidate picking)', () => {
    expect(isCompositionKeyEvent({ isComposing: true, keyCode: 32, key: ' ' })).toBe(true);
    expect(isCompositionKeyEvent({ isComposing: true, keyCode: 49, key: '1' })).toBe(true);
  });

  it('should return true for arrow keys while composing (candidate navigation)', () => {
    expect(isCompositionKeyEvent({ isComposing: true, keyCode: 38, key: 'ArrowUp' })).toBe(true);
    expect(isCompositionKeyEvent({ isComposing: true, keyCode: 40, key: 'ArrowDown' })).toBe(true);
  });

  it('should return false for Escape outside composition', () => {
    expect(isCompositionKeyEvent({ isComposing: false, keyCode: 27, key: 'Escape' })).toBe(false);
  });
});

// The composer's keydown orchestration routes Enter to submit, arrows to the
// trigger menus and history recall. During IME composition those same keys
// belong to the IME (candidate navigation/confirmation), so the orchestration
// must bail out BEFORE any of its own handling — otherwise the confirm Enter
// submits the half-composed pinyin ("拼音被擅自上屏/发送").
describe('Composer keydown — IME composition guard', () => {
  it('should bail out on composing keydown before any orchestration runs', () => {
    const guard = /if\s*\(isCompositionKeyEvent\(e\)\)\s*return false;/.exec(ComposerSource);
    expect(guard).not.toBeNull();
  });

  it('should place the guard before the first key handling in handleEditorKeydown', () => {
    const fn = /function handleEditorKeydown\(e: KeyboardEvent\): boolean \{([\s\S]*?)\n\}/.exec(
      ComposerSource,
    );
    expect(fn).not.toBeNull();
    const body = fn![1];
    const guardIndex = body.indexOf('isCompositionKeyEvent(e)');
    expect(guardIndex).toBeGreaterThan(-1);
    // No key-specific branch (Escape handling is the first one) may run before
    // the guard — the IME owns every key while composing.
    const firstBranch = body.indexOf("e.key === 'Escape'");
    expect(firstBranch).toBeGreaterThan(guardIndex);
  });

  it('should import the shared guard helper from lib/keyboard', () => {
    expect(ComposerSource).toMatch(
      /import \{ isCompositionKeyEvent \} from '\.\.\/\.\.\/lib\/keyboard';/,
    );
  });
});
