/**
 * `prompt` domain — the `StepRequest` types for prompt, steer, and retry
 * steps.
 *
 * `PromptStepRequest` / `SteerStepRequest` carry an already-built user
 * `ContextMessage` (image-compression captions pre-split), apply the image
 * format gate as the last funnel before the history, and materialize it
 * at pop time — caption reminders first, message second, mirroring the old
 * `appendPrompt` ordering. `PromptStepRequest` additionally prepends the
 * inline skill activation messages (US3.6) that the scheduler materialized at
 * admission time, and seeds the `turn.prompt` record from its body message.
 * `SteerStepRequest` uses `activeOrNewTurn`, is mergeable, and survives turn
 * boundaries; it records the `turn.steer` wire op on materialization and
 * unregisters itself from the service's pending-steer set once settled.
 * `RetryStepRequest` uses `newTurn`: it contributes no message and simply
 * drives one more step over the existing context. Each is constructed with
 * its collaborators captured — these are plain runtime objects, not DI
 * services.
 */

import { USER_PROMPT_ORIGIN, type ContextMessage } from '#/agent/contextMemory/types';
import { newMessageId } from '#/agent/contextMemory/messageId';
import { StepRequest, type StepRequestOptions, type TurnSeed } from '#/agent/loop/stepRequest';
import { gateImageFormatParts } from '#/agent/media/image-compress';
import type { IAgentSystemReminderService } from '#/agent/systemReminder/systemReminder';

abstract class UserMessageStepRequest extends StepRequest {
  protected readonly message: ContextMessage;
  private readonly ownerPromptId: string;

  constructor(
    message: ContextMessage,
    private readonly captions: readonly string[],
    private readonly reminders: IAgentSystemReminderService,
    options?: StepRequestOptions,
  ) {
    super(options);
    this.ownerPromptId = message.id ?? newMessageId();
    this.message = {
      ...message,
      id: this.ownerPromptId,
      content: gateImageFormatParts(message.content),
    };
  }

  override get turnSeed(): TurnSeed {
    return { input: this.message.content, origin: this.message.origin ?? USER_PROMPT_ORIGIN };
  }

  override onWillMaterialize(): void {
    for (const caption of this.captions) {
      this.reminders.appendSystemReminder(caption, {
        kind: 'injection',
        variant: 'image_compression',
        ownerPromptId: this.ownerPromptId,
      });
    }
  }

  resolveContextMessages(): readonly ContextMessage[] {
    return this.message.content.length > 0 ? [this.message] : [];
  }
}

export class PromptStepRequest extends UserMessageStepRequest {
  readonly kind = 'prompt';
  /**
   * Activation wrappers already materialized at admission time (US3.6
   * message sequence): each carries its own `skill_activation` origin with
   * trigger `user-inline` and its recorded activation fact.
   */
  private readonly inlineMessages: readonly ContextMessage[];

  constructor(
    message: ContextMessage,
    captions: readonly string[],
    reminders: IAgentSystemReminderService,
    inlineMessages: readonly ContextMessage[] = [],
  ) {
    super(message, captions, reminders, { admission: 'newTurn' });
    this.inlineMessages = inlineMessages;
  }

  override get turnSeed(): TurnSeed {
    return { input: this.message.content, origin: this.message.origin ?? USER_PROMPT_ORIGIN };
  }

  override resolveContextMessages(): readonly ContextMessage[] {
    return [
      ...this.inlineMessages,
      ...(this.message.content.length > 0 ? [this.message] : []),
    ];
  }
}

export class SteerStepRequest extends UserMessageStepRequest {
  readonly kind = 'steer';

  constructor(
    message: ContextMessage,
    captions: readonly string[],
    reminders: IAgentSystemReminderService,
    private readonly recordSteer: (message: ContextMessage) => void,
    private readonly forgetSteer: (request: SteerStepRequest) => void,
    admission: 'activeTurnOnly' | 'activeOrNewTurn' = 'activeTurnOnly',
  ) {
    super(message, captions, reminders, {
      mergeable: true,
      turnScoped: false,
      admission,
    });
  }

  override onWillMaterialize(): void {
    this.recordSteer(this.message);
    super.onWillMaterialize();
  }

  protected override onSettled(): void {
    this.forgetSteer(this);
  }
}

export class RetryStepRequest extends StepRequest {
  readonly kind = 'retry';

  constructor() {
    super({ admission: 'newTurn' });
  }

  override get turnSeed(): TurnSeed {
    return { input: [], origin: { kind: 'retry' } };
  }

  resolveContextMessages(): readonly ContextMessage[] {
    return [];
  }
}
