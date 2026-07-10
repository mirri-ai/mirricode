import type { Agent } from '../..';
import type { PrepareToolExecutionResult } from '../../../loop/types';
import { isPlainRecord } from '../../turn/canonical-args';
import type { PermissionPolicy, PermissionPolicyContext, PermissionPolicyResult } from '../types';

export class PreToolCallHookPermissionPolicy implements PermissionPolicy {
  readonly name = 'pre-tool-call-hook';

  constructor(private readonly agent: Agent) {}

  async evaluate(context: PermissionPolicyContext): Promise<PermissionPolicyResult | undefined> {
    const hookResults = await this.agent.hooks?.trigger('PreToolUse', {
      matcherValue: context.toolCall.name,
      signal: context.signal,
      inputData: {
        toolName: context.toolCall.name,
        toolInput: isPlainRecord(context.args) ? context.args : {},
        toolCallId: context.toolCall.id,
      },
    });
    context.signal.throwIfAborted();
    if (hookResults === undefined || hookResults.length === 0) return;

    // Block takes priority — first block wins, preserving existing semantics.
    const block = hookResults.find((r) => r.action === 'block');
    if (block !== undefined) {
      return {
        kind: 'deny',
        message: block.reason,
      };
    }

    // Handle updatedInput when the feature flag is enabled.
    const rewriteEnabled = this.agent.experimentalFlags.enabled('hook-command-rewrite');
    if (!rewriteEnabled) return undefined;

    const modified = hookResults.find((r) => r.updatedInput !== undefined);
    if (modified !== undefined) {
      return {
        kind: 'result',
        updatedArgs: modified.updatedInput,
      } as { kind: 'result' } & PrepareToolExecutionResult;
    }

    return undefined;
  }
}
