/**
 * `messagePipeline` domain — the explicit, ordered chain of transformations
 * that wire-ready `Message[]` pass through between the `contextProjector`
 * output and the `ModelRequester.request` call.
 *
 * The pipeline makes the previously-implicit transformation sequence
 * (video reference resolution → model-capability media adaptation) first-class:
 * each step is a named, independently testable unit with a declared ordering
 * constraint, registered into the Agent-scope `IMessagePipelineService`.
 *
 * Ordering invariants (enforced by registration order in
 * `messagePipelineService.ts`):
 *   1. `videoResolution` — rewrites `kimi-file://` references to provider-acceptable
 *      parts. Must run before capability adaptation because it may emit new
 *      media parts (inline base64) that the capability projector must see.
 *   2. `mediaCapabilityAdaptation` — drops media parts the bound model cannot
 *      accept and substitutes a path-bearing placeholder. Must run last, right
 *      before the wire, so no unsupported part ever reaches the provider.
 *
 * The `contextProjector` projections (`project` / `projectStrict` /
 * `projectMediaDegraded` / `projectMediaStripped`) are NOT steps in this
 * pipeline — they consume `ContextMessage[]` (pre-wire) and are invoked by
 * `llmRequesterService` to produce the `Message[]` that enters this pipeline.
 */

import { createDecorator } from '#/_base/di/instantiation';
import type { IDisposable } from '#/_base/di/lifecycle';
import type { Message } from '#/kosong/contract/message';
import type { Model } from '#/kosong/model/catalog';
import type { ModelRequester } from '#/kosong/model/modelRequester';

/**
 * The projection mode a request is running under. Mirrors the
 * `RequestProjection` union used by `llmRequesterService`'s retry loop so a
 * step may opt in or out based on the active recovery path. Most steps apply
 * to every projection; the value is carried for the rare step that must skip
 * (e.g. a step that would re-introduce a part the current projection just
 * stripped).
 */
export type RequestProjection = 'normal' | 'strict' | 'media-degraded' | 'media-stripped';

/**
 * Everything a projection step needs to decide whether to run and to perform
 * its transformation. `model` and `requester` are the bound model and its
 * requester for the current turn; `signal` lets an async step abort cleanly.
 */
export interface MessageProjectionContext {
  readonly messages: readonly Message[];
  readonly model: Model;
  readonly requester: ModelRequester;
  readonly projection: RequestProjection;
  readonly signal: AbortSignal | undefined;
}

/**
 * A single named transformation applied to the wire-bound message stream.
 *
 * A step must be pure with respect to its input: it returns a new array
 * (or the same reference when nothing changed) and never mutates the input.
 * Steps are executed in registration order; each receives the output of the
 * previous step.
 */
export interface MessageProjectionStep {
  /** Stable, human-readable identifier used for logging and debugging. */
  readonly name: string;
  /**
   * Returns `true` when the step should run under the given projection.
   * Defaults to running on every projection. A step that should skip on
   * `media-stripped` (because it would re-add a part that was just stripped)
   * returns `false` here.
   */
  readonly appliesTo?: (projection: RequestProjection) => boolean;
  /**
   * Transform the messages. May be async (e.g. video upload). Must return
   * the same array reference when no part was changed, so downstream steps
   * and the caller can short-circuit on reference equality.
   */
  readonly project: (
    context: MessageProjectionContext,
  ) => Promise<readonly Message[]> | readonly Message[];
}

export interface IMessagePipelineService {
  readonly _serviceBrand: undefined;

  /**
   * Register a step. Steps run in registration order. The returned disposable
   * removes the step — used by steps that own a lifecycle (e.g. an
   * Agent-scope service that registers itself on construction).
   */
  register(step: MessageProjectionStep): IDisposable;

  /**
   * Run every registered step (whose `appliesTo` permits the given
   * projection) in order, starting from `messages`. Returns the final
   * transformed array. When no step is registered or every step is a no-op,
   * returns the input reference unchanged.
   */
  run(
    messages: readonly Message[],
    model: Model,
    requester: ModelRequester,
    projection: RequestProjection,
    signal: AbortSignal | undefined,
  ): Promise<readonly Message[]>;
}

export const IMessagePipelineService = createDecorator<IMessagePipelineService>(
  'agentMessagePipelineService',
);
