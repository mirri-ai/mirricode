/**
 * 判定键盘事件是否处于 IME 组合态（composition）。
 *
 * 组合期间 Enter/空格/数字键用于候选上屏，文本提交类处理必须忽略这些
 * 事件，否则拼音候选会被人为打断、输入框内容被意外提交。Chromium 在
 * 组合态 keydown 上 `isComposing` 恒为 true；229 仅作旧引擎兜底。
 */
export interface KeyboardEventLike {
  isComposing?: boolean;
  keyCode?: number;
}

export function isCompositionKeyEvent(e: KeyboardEventLike): boolean {
  return e.isComposing === true || e.keyCode === 229;
}
