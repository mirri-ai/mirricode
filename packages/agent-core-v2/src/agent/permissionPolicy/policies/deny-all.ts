/**
 * `deny-all` permission policy — unconditionally denies every tool call.
 *
 * Intended for side-question (btw) child agents that must answer with text
 * only. The policy is opt-in: call `enable()` to activate and `disable()` to
 * deactivate. When disabled (the default), `evaluate` returns `undefined` so
 * the policy chain proceeds to the next policy.
 *
 * Mirrors v1's `DenyAllPermissionPolicy` but adapts to v2's static ordered
 * chain by toggling participation instead of relying on `unshift`.
 */

import type { ResolvedToolExecutionHookContext } from '#/agent/toolExecutor/toolHooks';
import type {
  PermissionPolicy,
  PermissionPolicyResult,
} from '#/agent/permissionPolicy/types';

const DEFAULT_MESSAGE = 'Tool calls are disabled.';

export class DenyAllPermissionPolicy implements PermissionPolicy {
  readonly name = 'deny-all';

  private enabled = false;
  private message = DEFAULT_MESSAGE;

  /** Activate the policy, optionally with a custom denial message. */
  enable(message?: string): void {
    this.enabled = true;
    this.message = message ?? DEFAULT_MESSAGE;
  }

  /** Deactivate the policy so `evaluate` returns `undefined`. */
  disable(): void {
    this.enabled = false;
    this.message = DEFAULT_MESSAGE;
  }

  evaluate(
    _context: ResolvedToolExecutionHookContext,
  ): PermissionPolicyResult | undefined {
    if (!this.enabled) return undefined;
    return {
      kind: 'deny',
      message: this.message,
      reason: { source: 'side_question' },
    };
  }
}
