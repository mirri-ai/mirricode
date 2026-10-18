// apps/mirri-web/test/i18n-locales.test.ts
// Regression guard (batch item 5): the managed OAuth login surface is gone and
// the product is no longer branded "Kimi". Every string literal in every locale
// module is scanned so no old brand name can sneak back into the UI copy.

import { describe, expect, it } from 'vitest';
import messages from '../src/i18n/locales';
import zhApproval from '../src/i18n/locales/zh/approval';

interface Offender {
  path: string;
  value: string;
}

/** Walk a locale object tree, flagging string values that contain the old brand. */
function walk(node: unknown, path: string, offenders: Offender[]): void {
  if (typeof node === 'string') {
    if (node.includes('Kimi')) offenders.push({ path, value: node });
    return;
  }
  if (Array.isArray(node)) {
    node.forEach((item, index) => walk(item, `${path}[${index}]`, offenders));
    return;
  }
  if (node !== null && typeof node === 'object') {
    for (const [key, value] of Object.entries(node)) {
      walk(value, `${path}.${key}`, offenders);
    }
  }
}

/** Collect string leaves matching a term predicate, for terminology guards. */
function collectTermOffenders(
  matcher: (value: string) => boolean,
  node: unknown,
  path: string,
  offenders: Offender[],
): void {
  if (typeof node === 'string') {
    if (matcher(node)) offenders.push({ path, value: node });
    return;
  }
  if (Array.isArray(node)) {
    node.forEach((item, index) => collectTermOffenders(matcher, item, `${path}[${index}]`, offenders));
    return;
  }
  if (node !== null && typeof node === 'object') {
    for (const [key, value] of Object.entries(node)) {
      collectTermOffenders(matcher, value, `${path}.${key}`, offenders);
    }
  }
}

function summarize(offenders: Offender[]): string {
  return offenders.map((o) => `${o.path}: ${JSON.stringify(o.value)}`).join('\n');
}

/** en: the chat entity must be called "session", never "conversation". */
const EN_BANNED = [
  /conversation/i,
  /new chat/i,
  /rename task/i,
  /archive task/i,
  /new task name/i,
  /moon (bright|dark)/i,
];

/** zh: 「对话」/「蜂群」banned; 「任务」only banned in session-entity phrases. */
const ZH_BANNED = [
  /对话/,
  /蜂群/,
  /重命名任务/,
  /归档任务/,
  /任务名称/,
  /月之亮面/,
  /月之暗面/,
];

describe('i18n terminology consistency (audit 05)', () => {
  it('should call the chat entity a session instead of conversation when the en locale is scanned', () => {
    const offenders: Offender[] = [];
    collectTermOffenders((v) => EN_BANNED.some((re) => re.test(v)), messages.en, 'en', offenders);
    expect(offenders, `en locale violates the session terminology:\n${summarize(offenders)}`).toEqual([]);
  });

  it('should avoid 对话, 蜂群, and session-as-task wording when the zh locale is scanned', () => {
    const offenders: Offender[] = [];
    collectTermOffenders((v) => ZH_BANNED.some((re) => re.test(v)), messages.zh, 'zh', offenders);
    expect(offenders, `zh locale violates the session terminology:\n${summarize(offenders)}`).toEqual([]);
  });
});

describe('i18n locale strings', () => {
  it('should contain no old "Kimi" brand name in any string when every locale module is scanned', () => {
    const offenders: Offender[] = [];
    walk(messages, 'messages', offenders);
    const summary = offenders
      .map((o) => `${o.path}: ${JSON.stringify(o.value)}`)
      .join('\n');
    expect(offenders, `locale strings still reference the old brand:\n${summary}`).toEqual([]);
  });
});

describe('zh approval locale translation', () => {
  it('should show a Chinese approval badge when the zh locale renders the required state', () => {
    expect(zhApproval.required).toMatch(/[\u4E00-\u9FFF]/);
  });

  it('should use half-width question marks consistently across all approval titles when the zh locale lists them', () => {
    for (const value of Object.values(zhApproval.title)) {
      expect(value.endsWith('？')).toBe(false);
    }
  });
});
