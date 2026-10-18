import { createDecorator } from "#/_base/di/instantiation";
import type { ContextMessage, SkillActivationOrigin } from '#/agent/contextMemory/types';
import type { WithSkillsEntry } from '#/agent/contextMemory/protocolMessage';
import type { Turn } from '#/agent/loop/loop';
import type { PromptHandle } from '#/agent/prompt/prompt';

export interface SkillActivationInput {
  readonly name: string;
  readonly args?: string;
}

/** A command reference found inline in a user message, surfaced for the submitting client. */
export interface PendingInlineCommand {
  readonly name: string;
  readonly display: string;
  readonly target: string;
}

/** A reference whose skill could not be activated; the link stays plain text. */
export interface SkippedInlineReference {
  readonly name: string;
  readonly target: string;
  readonly reason: 'skill_not_found' | 'skill_not_activatable';
}

export interface InlineSkillWrapper {
  readonly skillName: string;
  readonly target: string;
  /** Render the activation wrapper and record the activation fact. */
  materialize(): Promise<ContextMessage>;
}

export interface InlineSkillReferenceAnalysis {
  /** Message text with successful skill references replaced by `[display]`. */
  readonly body: string;
  /** Activation wrappers in appearance order, deduplicated by skill path. */
  readonly wrappers: readonly InlineSkillWrapper[];
  /** Command references — the engine never executes these. */
  readonly pendingCommands: readonly PendingInlineCommand[];
  /** Failed skill references with a reason direction. */
  readonly skipped: readonly SkippedInlineReference[];
  /** Activated skills for the with_skills record on the rewritten body (ADR-006). */
  readonly activatedSkills: WithSkillsEntry[];
}

export interface IAgentSkillService {
  readonly _serviceBrand: undefined;

  activate(input: SkillActivationInput): Promise<Turn>;
  /**
   * Queue a skill activation as a raw user-slash intent and return
   * immediately. Validation (skill exists, user-activatable) stays
   * synchronous; the rendered prompt, `skill.activate` fact, and telemetry
   * only happen when the queued prompt materializes (dequeue time), so the
   * caller is never blocked behind a running turn.
   */
  enqueueDeferred(input: SkillActivationInput): Promise<PromptHandle>;
  recordModelToolActivation(origin: SkillActivationOrigin): void;
  /**
   * Parse a user message's inline skill/command references (US3.6 / ADR-004)
   * at admission time and build the turn-message-sequence plan: activation
   * wrappers, the reference-stripped body, pending commands, and skipped
   * references. The path is the resolution authority; activation ids derive
   * from the message id so retries are idempotent.
   */
  analyzeInlineReferences(text: string, messageId: string): Promise<InlineSkillReferenceAnalysis>;
}

/**
 * Skill-analysis capability the prompt scheduler needs for inline reference
 * injection. Declared as its own optional interface so the prompt domain does
 * not depend on the whole skill service: tests and hosts without the skill
 * domain simply never see inline analysis, and plain messages skip it anyway
 * (cheap pre-check, no catalog round-trip).
 */
export interface IInlineSkillAnalyzer {
  readonly _serviceBrand: undefined;
  analyzeInlineReferences(text: string, messageId: string): Promise<InlineSkillReferenceAnalysis>;
}

export const IAgentSkillService =
  createDecorator<IAgentSkillService>('agentSkillService');

export const IInlineSkillAnalyzer =
  createDecorator<IInlineSkillAnalyzer>('inlineSkillAnalyzer');
