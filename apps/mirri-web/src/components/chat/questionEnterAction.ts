export type QuestionEnterAction = 'next' | 'submit' | 'none';

export interface QuestionEnterState {
  busy: boolean;
  step: number;
  total: number;
  currentAnswered: boolean;
  allAnswered: boolean;
}

/**
 * Enter 键在问答卡上的动作：有未答的下一个问题时前进，已在最后一题且
 * 全部作答则提交，否则无动作。从 QuestionCard 抽出以便无 DOM 单测。
 */
export function decideEnterAction(state: QuestionEnterState): QuestionEnterAction {
  if (state.busy) return 'none';
  if (state.step < state.total - 1 && state.currentAnswered) return 'next';
  if (state.allAnswered) return 'submit';
  return 'none';
}