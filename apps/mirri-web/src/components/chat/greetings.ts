// apps/mirri-web/src/components/chat/greetings.ts
// Time-of-day greetings for the empty-session landing state. The copy is product
// voice, not UI chrome, so it lives here rather than in the i18n locale modules:
// the two languages are written to match each other sentence for sentence, and
// keeping the pairs side by side is what stops them drifting apart.

/** One bucket per mood the landing state can catch the user in. */
export type GreetingPeriod =
  | 'morning'
  | 'forenoon'
  | 'noon'
  | 'afternoon'
  | 'evening_offwork'
  | 'night'
  | 'late_night';

/**
 * Where each period starts, in local wall-clock hours. A period runs until the
 * next entry; `late_night` closes the wrap past midnight, so 00:00–04:59 needs
 * no second entry. Ordered ascending — the lookup walks it in order.
 *
 * Boundaries (confirmed with the product owner): 05 morning · 09 forenoon ·
 * 12 noon · 14 afternoon · 17 off-work · 19 night · 22 late night.
 */
const PERIOD_STARTS: ReadonlyArray<{ startHour: number; period: GreetingPeriod }> = [
  { startHour: 5, period: 'morning' },
  { startHour: 9, period: 'forenoon' },
  { startHour: 12, period: 'noon' },
  { startHour: 14, period: 'afternoon' },
  { startHour: 17, period: 'evening_offwork' },
  { startHour: 19, period: 'night' },
  { startHour: 22, period: 'late_night' },
];

/** The period whose pool the clock falls into (local time). */
export function greetingPeriodAt(date: Date): GreetingPeriod {
  const hour = date.getHours();
  // Before the first boundary is still the tail of last night.
  let period: GreetingPeriod = 'late_night';
  for (const entry of PERIOD_STARTS) {
    if (hour >= entry.startHour) period = entry.period;
  }
  return period;
}

/** Each language's pool for one period. A non-empty tuple, so picking from it
 *  is total — there is no "empty pool" state to defend against. */
type GreetingPool = readonly [string, ...string[]];

type GreetingCopy = Record<'zh' | 'en', GreetingPool>;

/**
 * The pools. Each pair of lines is the same thought in both languages, in the
 * same order — keep the shapes parallel when editing. Chinese is the authored
 * voice; English reads as its equivalent, not as a literal translation.
 */
const GREETINGS: Record<GreetingPeriod, GreetingCopy> = {
  morning: {
    zh: ['早安，今天想做点什么？', '早，想好要开发什么了吗', '早安，有想法尽管说', '新的一天，开工吧', '早，代码的事交给我'],
    en: [
      'Good morning — what shall we build today?',
      'Morning. Got something you want to build?',
      'Good morning — say the word',
      'A new day. Let’s get going.',
      'Morning. Leave the code to me.',
    ],
  },
  forenoon: {
    zh: ['上午好，打算做些什么？', '有开发想法直接说', '上午好，我随时待命', '想好需求，我来实现', '尽管提，我帮你编码'],
    en: [
      'Morning — what’s the plan?',
      'Tell me the idea, straight up',
      'Morning — I’m standing by',
      'Think it through, I’ll build it',
      'Just ask — I’ll write the code',
    ],
  },
  noon: {
    zh: ['午安，先歇一歇吧', '午饭时间，放松下', '有需求，晚点也可以', '你先休息，我在待命', '歇会儿，编码交给我'],
    en: [
      'Midday — take a breather',
      'Lunch time, go easy',
      'Need something? Later works too',
      'You rest, I’ll hold the fort',
      'Take a break — the code can wait',
    ],
  },
  afternoon: {
    zh: ['下午好，有新想法吗？', '尽管把需求告诉我', '下午好，准备开工', '构思好了，交给我', '繁琐代码我来写'],
    en: [
      'Afternoon — any new ideas?',
      'Tell me what you need',
      'Afternoon. Ready to go',
      'Once you’ve thought it out, hand it over',
      'I’ll take the tedious code',
    ],
  },
  evening_offwork: {
    zh: ['下班啦，好好放松吧', '想法留下，安心收工', '今天就到这里，好好休息', '想好了需求，我来处理', '放下工作，歇一歇'],
    en: [
      'You’re off the clock — go relax',
      'Leave the idea, head home easy',
      'That’s enough for today — rest well',
      'Got the plan? I’ll take it from here',
      'Put the work down for a bit',
    ],
  },
  night: {
    zh: [
      '晚上好，慢慢来，别着急',
      '定下目标，明天来验收',
      '夜色正好，有想法再说',
      '设好目标，明早看结果',
      '晚间放松，不必赶进度',
      '定好目标，给你留惊喜',
      '晚上好，别太累啦',
      '想好了，我慢慢推进',
    ],
    en: [
      'Evening — take your time',
      'Set a goal, review it tomorrow',
      'Nice night for an idea',
      'Set the goal, see results in the morning',
      'Relax tonight — no rush on progress',
      'Set a goal and I’ll have a surprise for you',
      'Evening — don’t push too hard',
      'Thought it through? I’ll move it along',
    ],
  },
  late_night: {
    zh: ['夜深了，早点休息吧', '别熬啦，身体要紧', '夜深，事情留到明天', '太晚啦，好好休息', '先休息，代码不急'],
    en: [
      'It’s late — get some rest',
      'Don’t stay up, your health comes first',
      'Late night — leave it for tomorrow',
      'Too late now, get some sleep',
      'Sleep first — the code isn’t urgent',
    ],
  },
};

/**
 * Pick one line for the period the clock falls into.
 *
 * `random` is injectable so callers (and tests) can pin the choice; the
 * component passes nothing and gets `Math.random`.
 */
export function pickGreeting(date: Date, locale: string, random: () => number = Math.random): string {
  const pool = GREETINGS[greetingPeriodAt(date)][locale.startsWith('zh') ? 'zh' : 'en'];
  const index = Math.min(Math.floor(random() * pool.length), pool.length - 1);
  // `noUncheckedIndexedAccess` cannot see that the clamp above is in range; the
  // pool is a non-empty tuple, so index 0 is always there to fall back on.
  return pool[index] ?? pool[0];
}
