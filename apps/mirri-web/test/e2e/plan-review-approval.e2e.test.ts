// @vitest-environment jsdom
// apps/mirri-web/test/e2e/plan-review-approval.e2e.test.ts
//
// End-to-end coverage of the web side of the plan-review decision flow: the
// approval card the engine parks when a plan is ready must turn each of its
// three affordances (approve / revise-with-feedback / reject-and-exit) — and
// each offered approach — into exactly one `respondApproval` call carrying the
// decision payload the engine's ExitPlanMode gate reads.
//
// The card is mounted for real (jsdom) and wired the way ChatDock + App wire
// it in production: ApprovalCard emits `decide`, the parent answers the approval
// through the client's `respondApproval(approvalId, response)`. The mock sits at
// that client boundary, so a wrong payload from the card fails the test. The
// wire serialization below that boundary is covered by
// `test/ws-rpc-approval-flow.test.ts`; the engine-side gate is out of scope.
//
// Markdown.vue is stubbed out: it pulls the whole markstream/katex/mermaid
// rendering stack (plus module workers), which has nothing to do with the
// decision payload and cannot boot in jsdom.

import { afterEach, describe, expect, it, vi } from 'vitest';
import { defineComponent, h, nextTick } from 'vue';
import { mount, type VueWrapper } from '@vue/test-utils';
import { createI18n } from 'vue-i18n';

import ApprovalCard from '../../src/components/chat/ApprovalCard.vue';
import { messages } from '../../src/i18n/locales';
import type { ApprovalBlock } from '../../src/types';

vi.mock('../../src/components/chat/Markdown.vue', () => ({
  default: defineComponent({
    name: 'Markdown',
    props: { text: { type: String, default: '' } },
    setup: (props) => () => h('div', { class: 'markdown-stub' }, props.text),
  }),
}));

// lib/icons bundles the icon set through unplugin-icons virtual modules
// (`~icons/ri/*?raw`), which Vitest 4's module runner refuses to load outside a
// browser/bundler pipeline. The decision flow does not depend on glyphs, so the
// registry is replaced with an empty one; <Icon> then renders nothing.
vi.mock('../../src/lib/icons', () => ({
  SIZE_PX: { sm: 14, md: 16, lg: 20 },
  getIcon: () => undefined,
  iconSvg: () => '',
}));

// ---------------------------------------------------------------------------
// Fixtures / helpers
// ---------------------------------------------------------------------------

const APPROVAL_ID = 'appr-plan-1';
const SESSION_ID = 's1';

const PLAN_TEXT = '# Plan\n\n1. Read the code\n2. Change it';

function planReviewBlock(options?: { label: string; description?: string }[]): ApprovalBlock {
  return {
    kind: 'plan_review',
    plan: PLAN_TEXT,
    path: '/tmp/plan.md',
    options,
  };
}

/**
 * Mount the approval card inside a stand-in for ChatDock + App: the card's
 * `decide` event answers the parked approval through the client's
 * `respondApproval(approvalId, response)` — the exact production wiring.
 */
function mountPlanReview(block: ApprovalBlock): {
  wrapper: VueWrapper;
  respondApproval: ReturnType<typeof vi.fn>;
} {
  const respondApproval = vi.fn(async () => undefined);

  const Harness = defineComponent({
    setup() {
      return () =>
        h(ApprovalCard, {
          block,
          onDecide: (response: unknown) => respondApproval(APPROVAL_ID, response),
        });
    },
  });

  const i18n = createI18n({ legacy: false, locale: 'en', fallbackLocale: 'en', messages });
  const wrapper = mount(Harness, { global: { plugins: [i18n] } });
  return { wrapper, respondApproval };
}

/** The decision buttons of the plan-review action row, in render order. */
function planButtons(wrapper: VueWrapper) {
  return wrapper.findAll('.plan-actions button');
}

/** Click the plan-review action button whose label starts with `label`. */
async function clickPlanButton(wrapper: VueWrapper, label: string): Promise<void> {
  const button = planButtons(wrapper).find((b) => (b.text() ?? '').startsWith(label));
  if (button === undefined) {
    const rendered = planButtons(wrapper).map((b) => b.text());
    throw new Error(`no plan-review button starting with '${label}'; rendered: ${JSON.stringify(rendered)}`);
  }
  await button.trigger('click');
}

afterEach(() => {
  vi.clearAllMocks();
});

// ---------------------------------------------------------------------------
// plan_review without offered approaches — approve / revise / reject-and-exit
// ---------------------------------------------------------------------------

describe('plan review approval (no offered approaches)', () => {
  it('should respond approved with no extra fields when the user approves the plan', async () => {
    const { wrapper, respondApproval } = mountPlanReview(planReviewBlock());

    await clickPlanButton(wrapper, 'Approve plan');

    expect(respondApproval).toHaveBeenCalledTimes(1);
    expect(respondApproval).toHaveBeenCalledWith(APPROVAL_ID, { decision: 'approved' });
    wrapper.unmount();
  });

  it('should respond rejected with Revise feedback when the user submits revision notes', async () => {
    const { wrapper, respondApproval } = mountPlanReview(planReviewBlock());

    await clickPlanButton(wrapper, 'Revise');
    await nextTick();

    const textarea = wrapper.find('textarea.feedback-ta');
    expect(textarea.exists()).toBe(true);
    await textarea.setValue('please simplify');
    // Enter sends (Shift+Enter breaks the line — ApprovalCard.onFeedbackKeydown).
    await textarea.trigger('keydown', { key: 'Enter' });

    expect(respondApproval).toHaveBeenCalledTimes(1);
    expect(respondApproval).toHaveBeenCalledWith(APPROVAL_ID, {
      decision: 'rejected',
      selectedLabel: 'Revise',
      feedback: 'please simplify',
    });
    wrapper.unmount();
  });

  it('should respond with the typed feedback when the user taps the visible submit button instead of Enter', async () => {
    const { wrapper, respondApproval } = mountPlanReview(planReviewBlock());

    await clickPlanButton(wrapper, 'Revise');
    await nextTick();

    const textarea = wrapper.find('textarea.feedback-ta');
    await textarea.setValue('use a smaller scope');
    // The strip must expose a visible, tappable submit target (touch users
    // cannot rely on the Enter key alone).
    const submit = wrapper.find('.feedback-actions button');
    expect(submit.exists()).toBe(true);
    await submit.trigger('click');

    expect(respondApproval).toHaveBeenCalledTimes(1);
    expect(respondApproval).toHaveBeenCalledWith(APPROVAL_ID, {
      decision: 'rejected',
      selectedLabel: 'Revise',
      feedback: 'use a smaller scope',
    });
    wrapper.unmount();
  });

  it('should close the feedback strip without deciding when the user taps the visible cancel button', async () => {
    const { wrapper, respondApproval } = mountPlanReview(planReviewBlock());

    await clickPlanButton(wrapper, 'Revise');
    await nextTick();

    const textarea = wrapper.find('textarea.feedback-ta');
    await textarea.setValue('not sent');
    const buttons = wrapper.findAll('.feedback-actions button');
    expect(buttons.length).toBe(2);
    await buttons[1]!.trigger('click');
    await nextTick();

    expect(respondApproval).not.toHaveBeenCalled();
    expect(wrapper.find('textarea.feedback-ta').exists()).toBe(false);
    wrapper.unmount();
  });

  it('should respond rejected with the Reject and Exit label when the user rejects and leaves plan mode', async () => {
    const { wrapper, respondApproval } = mountPlanReview(planReviewBlock());

    await clickPlanButton(wrapper, 'Reject and Exit');

    expect(respondApproval).toHaveBeenCalledTimes(1);
    expect(respondApproval).toHaveBeenCalledWith(APPROVAL_ID, {
      decision: 'rejected',
      selectedLabel: 'Reject and Exit',
    });
    wrapper.unmount();
  });
});

// ---------------------------------------------------------------------------
// plan_review with offered approaches — the chosen approach is the decision
// ---------------------------------------------------------------------------

describe('plan review approval (offered approaches)', () => {
  it('should keep the Approve plan button alongside the approach buttons when approaches are offered', async () => {
    const { wrapper, respondApproval } = mountPlanReview(
      planReviewBlock([
        { label: 'Approach A', description: 'Use caching' },
        { label: 'Approach B', description: 'Rewrite' },
      ]),
    );

    // The offered approaches are alternatives: the plan-as-submitted stays
    // available as its own decision, and approaches occupy the next digits.
    expect(planButtons(wrapper).map((b) => b.text())).toEqual([
      'Approve plan[1]',
      'Approach A[2]',
      'Approach B[3]',
      'Revise',
      'Reject and Exit',
    ]);

    await clickPlanButton(wrapper, 'Approve plan');

    expect(respondApproval).toHaveBeenCalledTimes(1);
    expect(respondApproval).toHaveBeenCalledWith(APPROVAL_ID, { decision: 'approved' });
    wrapper.unmount();
  });

  it('should respond approved with the chosen approach label when the user picks one', async () => {
    const { wrapper, respondApproval } = mountPlanReview(
      planReviewBlock([
        { label: 'Approach A', description: 'Use caching' },
        { label: 'Approach B', description: 'Rewrite' },
      ]),
    );

    await clickPlanButton(wrapper, 'Approach A');

    expect(respondApproval).toHaveBeenCalledTimes(1);
    expect(respondApproval).toHaveBeenCalledWith(APPROVAL_ID, {
      decision: 'approved',
      selectedLabel: 'Approach A',
    });
    wrapper.unmount();
  });
});
