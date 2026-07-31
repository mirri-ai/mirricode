import type { Agent } from '../..';
import { isPlainRecord } from '../../turn/canonical-args';
import { blockDecision, injectHookAdditionalContext } from '../../../session/hooks';
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
    injectHookAdditionalContext(this.agent.context, hookResults, 'PreToolUse');
    const hookResult = blockDecision('PreToolUse', hookResults ?? []);
    if (hookResult === undefined) return;
    return {
      kind: 'deny',
      message: hookResult.reason,
    };
  }
}
