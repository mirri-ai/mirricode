import { describe, expect, it } from 'vitest';
import { decideEnterAction } from '../src/components/chat/questionEnterAction';

const base = { busy: false, step: 0, total: 2, currentAnswered: true, allAnswered: true };

describe('decideEnterAction', () => {
  it('should advance to the next question when the current one is answered and more remain', () => {
    expect(decideEnterAction(base)).toBe('next');
  });

  it('should submit when already on the last question and all are answered', () => {
    expect(decideEnterAction({ ...base, step: 1 })).toBe('submit');
  });

  it('should do nothing while an answer is in flight', () => {
    expect(decideEnterAction({ ...base, busy: true, step: 1 })).toBe('none');
  });

  it('should do nothing when the current question is unanswered', () => {
    expect(decideEnterAction({ ...base, currentAnswered: false, allAnswered: false })).toBe('none');
  });
});