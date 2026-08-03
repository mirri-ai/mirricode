/**
 * `messagePipeline` domain — shared media-placeholder text builder.
 *
 * Three distinct reasons can cause a media part to be replaced by a text
 * placeholder before it reaches the provider:
 *
 *   1. `capability_gap` — the bound model lacks the input modality entirely
 *      (e.g. `image_in: false`). The placeholder must carry the persisted
 *      file path and concrete recovery guidance so the LLM can delegate to a
 *      multimodal sub-agent.
 *   2. `size_limit` — the provider rejected the request body as too large
 *      (HTTP 413) and older media was dropped to fit. The placeholder should
 *      tell the LLM the media is gone for size reasons and can be re-read.
 *   3. `format_rejected` — the provider rejected a specific image's format
 *      and that media was stripped. The placeholder should point the LLM at
 *      conversion guidance.
 *
 * Previously each reason had its own hand-rolled string in a different module
 * (`mediaCapabilityProjector.ts` vs `contextProjectorService.ts`), and only
 * the capability-gap variant carried the file path. Unifying them here means
 * the LLM sees consistent, path-bearing information regardless of which
 * projection path removed the media.
 */

export type MediaKind = 'image' | 'video' | 'audio';

export type MediaPlaceholderReason = 'capability_gap' | 'size_limit' | 'format_rejected';

export interface MediaPlaceholderInput {
  readonly kind: MediaKind;
  readonly reason: MediaPlaceholderReason;
  /**
   * The persisted filesystem path of the original media, when one was
   * recorded by the prompt-persistence layer (stored in the part's `id`
   * field). For `capability_gap` the path is the recovery handle a sub-agent
   * needs to `ReadMediaFile`. For `size_limit` / `format_rejected` the path
   * is included when available so the LLM can still recover by re-reading.
   */
  readonly filePath?: string;
}

/**
 * Build the placeholder text substituted for a dropped media part.
 *
 * - `capability_gap` with a path: the full multi-line notice — omission
 *   statement, saved-to path, and three concrete recovery paths (dedicated
 *   multimodal sub-agent, generic sub-agent with model override, or ask the
 *   user to switch models).
 * - `capability_gap` without a path: the short omission statement only — the
 *   LLM has no file to hand to a sub-agent.
 * - `size_limit` / `format_rejected`: a short reason-specific statement,
 *   extended with the saved-to path and a re-read hint when a path is
 *   available.
 */
export function buildMediaPlaceholder(input: MediaPlaceholderInput): string {
  const { kind, reason, filePath } = input;
  if (reason === 'capability_gap') {
    return filePath === undefined
      ? `[${kind} omitted: current model has no ${kind} input]`
      : capabilityGapPlaceholderWithFile(kind, filePath);
  }
  if (reason === 'size_limit') {
    const base = `[${kind} omitted: dropped to fit the provider request size limit; re-read the file to view it]`;
    return filePath === undefined ? base : `${base}\nThe original ${kind} has been saved to: ${filePath}`;
  }
  // format_rejected
  const base = `[${kind} omitted for provider compatibility; re-read the file to view it or get conversion guidance]`;
  return filePath === undefined ? base : `${base}\nThe original ${kind} has been saved to: ${filePath}`;
}

function capabilityGapPlaceholderWithFile(kind: MediaKind, filePath: string): string {
  return (
    `[${kind} omitted: current model has no ${kind} input]\n` +
    `The original ${kind} has been saved to: ${filePath}\n` +
    `To analyze this ${kind}, try one of these approaches:\n` +
    `1. Check if a dedicated multimodal sub-agent is available (e.g. a media-reader ` +
    `profile). If so, dispatch it — its default model already supports ${kind} input, ` +
    `so no model override is needed. Instruct it to read the file via ReadMediaFile.\n` +
    `2. If no dedicated sub-agent exists, dispatch a sub-agent (e.g. coder) and set the ` +
    `"model" parameter to a model that supports ${kind} input — see the "Available ` +
    `models" list in the Agent tool description for model ids and their capabilities. ` +
    `Instruct it to read the file via ReadMediaFile.\n` +
    `3. If no multimodal model is available, tell the user you cannot process the ` +
    `${kind} and suggest they switch to a model with ${kind} input capability, or ` +
    `describe the ${kind} content in text so you can help.`
  );
}
