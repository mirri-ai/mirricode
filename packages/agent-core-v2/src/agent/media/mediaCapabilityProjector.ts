/**
 * `media` domain — capability-driven media projection.
 *
 * When the bound model cannot accept a modality present in the conversation
 * (e.g. the model lacks `image_in` but the prompt contains an image), the raw
 * media part must be dropped from the wire request sent to the provider —
 * sending it would either fail outright or produce a confusing "I can't see
 * images" reply. In its place a descriptive text placeholder is substituted so
 * the LLM can still act: the placeholder carries the persisted file path and
 * guidance on dispatching a multimodal sub-agent to read the file.
 *
 * This is the wire-layer counterpart to `capabilityGapInjection` (which adds a
 * context-level hint). The injection tells the LLM "you lack this modality";
 * this projector actually strips the unsupported part from the request and
 * leaves the path behind so the LLM can hand it to a sub-agent.
 *
 * The projection is pure: it never mutates the input messages and returns the
 * same array reference when no part needed replacement.
 */

import type { ContentPart, Message } from '#/kosong/contract/message';
import type { ModelCapability } from '#/kosong/contract/capability';
import { buildMediaPlaceholder } from '#/agent/messagePipeline/mediaPlaceholder';

/**
 * The input modalities a model may or may not accept. Extracted as a focused
 * view over `ModelCapability` so this module does not depend on the full
 * capability record (which also carries context-length and tool-use fields
 * unrelated to media adaptation).
 */
export interface MediaInputCapability {
  readonly image_in: boolean;
  readonly video_in: boolean;
  readonly audio_in: boolean;
}

/**
 * Derive the media-input capability view from a full model capability record.
 */
export function mediaInputCapabilityOf(capability: ModelCapability): MediaInputCapability {
  return {
    image_in: capability.image_in,
    video_in: capability.video_in,
    audio_in: capability.audio_in,
  };
}

/**
 * The kind of media a single content part carries. `null` marks a non-media
 * part (text, thinking) that always passes through unchanged.
 */
type MediaKind = 'image' | 'video' | 'audio' | null;

function mediaKindOf(part: ContentPart): MediaKind {
  switch (part.type) {
    case 'image_url':
      return 'image';
    case 'video_url':
      return 'video';
    case 'audio_url':
      return 'audio';
    default:
      return null;
  }
}

/**
 * The persisted file path attached to a media part, when one was recorded by
 * the prompt-persistence layer. The path is stored in the part's `id` field
 * and is the handle a sub-agent needs to `ReadMediaFile` the original bytes.
 *
 * Only filesystem-style paths are recognised. Protocol identifiers such as
 * `ms://file-1` (placed by other subsystems) are rejected so a non-path `id`
 * is never mistaken for a file the sub-agent can open.
 */
function persistedFilePathOf(part: ContentPart): string | undefined {
  let id: string | undefined;
  if (part.type === 'image_url') id = part.imageUrl.id;
  else if (part.type === 'video_url') id = part.videoUrl.id;
  else if (part.type === 'audio_url') id = part.audioUrl.id;
  else return undefined;
  return isFileSystemPath(id) ? id : undefined;
}

function isFileSystemPath(id: string | undefined): id is string {
  if (id === undefined || id === '') return false;
  if (id.includes('://')) return false;
  return id.startsWith('/') || id.startsWith('./') || id.startsWith('../') || id.includes('/');
}

/**
 * Replace every media part the bound model cannot accept with a text
 * placeholder. The placeholder carries the persisted file path (when one was
 * recorded) plus guidance on dispatching a multimodal sub-agent, so the LLM
 * can recover by delegating to a sub-agent whose model supports the modality.
 *
 * Returns the input array reference unchanged when the model supports every
 * modality present — no allocation, no copy. Otherwise returns a new array
 * with only the affected messages replaced; untouched messages keep their
 * reference.
 */
export function projectMediaForModelCapability(
  messages: readonly Message[],
  capability: MediaInputCapability,
): readonly Message[] {
  const unsupportedModalities = unsupportedMediaKinds(capability);
  if (unsupportedModalities.size === 0) return messages;

  let mutated = false;
  const projected = messages.map((message) => {
    if (!messageHasMedia(message)) return message;
    let messageChanged = false;
    const nextContent = message.content.map((part): ContentPart => {
      const kind = mediaKindOf(part);
      if (kind === null || !unsupportedModalities.has(kind)) return part;
      messageChanged = true;
      mutated = true;
      return { type: 'text', text: mediaCapabilityGapPlaceholder(kind, persistedFilePathOf(part)) };
    });
    return messageChanged ? { ...message, content: nextContent } : message;
  });

  return mutated ? projected : messages;
}

function unsupportedMediaKinds(capability: MediaInputCapability): Set<MediaKind> {
  const kinds = new Set<MediaKind>();
  if (!capability.image_in) kinds.add('image');
  if (!capability.video_in) kinds.add('video');
  if (!capability.audio_in) kinds.add('audio');
  return kinds;
}

function messageHasMedia(message: Message): boolean {
  return message.content.some((part) => mediaKindOf(part) !== null);
}

/**
 * Build the placeholder text substituted for an unsupported media part.
 *
 * Delegates to the shared `buildMediaPlaceholder` so the capability-gap
 * wording stays consistent with the size-limit and format-rejected
 * placeholders produced by `contextProjectorService`. The wrapper keeps the
 * nullable `kind` signature for call sites that inspect a part before
 * knowing whether it is media.
 */
export function mediaCapabilityGapPlaceholder(kind: MediaKind, filePath: string | undefined): string {
  if (kind === null) {
    throw new Error('mediaCapabilityGapPlaceholder requires a media kind; received null');
  }
  return buildMediaPlaceholder({ kind, reason: 'capability_gap', filePath });
}
