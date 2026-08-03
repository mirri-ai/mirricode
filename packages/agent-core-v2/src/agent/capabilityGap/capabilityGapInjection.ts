/**
 * Capability-gap hint injection.
 *
 * When the main agent's model lacks a modality that the user's prompt
 * requires (e.g. the model has `image_in: false` but the prompt contains an
 * image), this injector appends a system reminder telling the LLM it cannot
 * process that modality and should dispatch a subagent whose model can.
 *
 * The hint is only injected when a feasible alternative exists — i.e. at
 * least one model in the catalog supports the missing capability — so the
 * LLM is never told "dispatch a subagent" when no subagent could help.
 *
 * Registered as a `ContextInjectionProvider` named `'capability_gap'`; fires
 * on every new turn's first step (gated by `isNewTurn`), mirroring the
 * goal/plan/permission-mode injection pattern.
 */

import { Disposable } from '#/_base/di/lifecycle';
import { IAgentContextInjectorService } from '#/agent/contextInjector/contextInjector';
import { IAgentContextMemoryService } from '#/agent/contextMemory/contextMemory';
import type { ContextMessage } from '#/agent/contextMemory/types';
import { IAgentProfileService } from '#/agent/profile/profile';
import type { ModelCapability } from '#/kosong/contract/capability';
import type { ContentPart } from '#/kosong/contract/message';
import { IModelCatalog } from '#/kosong/model/catalog';
import { ISessionAgentProfileCatalog } from '#/session/sessionAgentProfileCatalog/sessionAgentProfileCatalog';

/**
 * A modality the main model may lack, with the capability key and a
 * human-readable label for the hint.
 */
interface ModalityGap {
  readonly capability: 'image_in' | 'video_in' | 'audio_in';
  readonly label: string;
  readonly detect: (parts: readonly ContentPart[]) => boolean;
}

const MODALITY_GAPS: readonly ModalityGap[] = [
  {
    capability: 'image_in',
    label: 'image',
    // Detect either a real image part OR a client-injected text notice.
    // Clients (Desktop / CLI) replace an inline image with a text notice when
    // the bound model lacks image input — the notice names the saved file so
    // the LLM can hand the path to a subagent. We must treat those notices as
    // "the user's prompt contains an image" so the gap hint still fires.
    detect: (parts) =>
      parts.some((p) => p.type === 'image_url') ||
      parts.some(
        (p) =>
          p.type === 'text' &&
          (p.text.includes('[image omitted:') ||
            p.text.includes('original image has been saved to') ||
            p.text.includes('Attached file') &&
            /\.(png|jpe?g|gif|webp|bmp|svg|avif)\b/i.test(p.text)),
      ),
  },
  {
    capability: 'video_in',
    label: 'video',
    detect: (parts) => parts.some((p) => p.type === 'video_url'),
  },
  {
    capability: 'audio_in',
    label: 'audio',
    detect: (parts) => parts.some((p) => p.type === 'audio_url'),
  },
];

export interface CapabilityGapInjectionOptions {
  /**
   * Returns the catalog id of the model the main agent is bound to, or
   * `undefined` when unknown. Used to skip the "feasible alternative" scan
   * (every model in the catalog is a candidate, including the main one —
   * but the main one is excluded because it's the one with the gap).
   */
  readonly getCallerModelId?: () => string | undefined;
}

export class CapabilityGapInjection extends Disposable {
  constructor(
    options: CapabilityGapInjectionOptions | undefined,
    @IAgentContextInjectorService dynamicInjector: IAgentContextInjectorService,
    @IAgentContextMemoryService private readonly context: IAgentContextMemoryService,
    @IAgentProfileService private readonly profile: IAgentProfileService,
    @IModelCatalog private readonly modelCatalog: IModelCatalog,
    @ISessionAgentProfileCatalog private readonly profileCatalog: ISessionAgentProfileCatalog,
  ) {
    super();
    void options; // reserved for future caller-model-id scoping
    this._register(
      dynamicInjector.register('capability_gap', ({ isNewTurn }) =>
        isNewTurn ? this.hint() : undefined,
      ),
    );
  }

  private hint(): string | undefined {
    const caps = this.profile.getModelCapabilities();
    // Find the last user message in context — that's the prompt we need to
    // check for modalities the model can't handle.
    const lastUser = lastUserMessage(this.context.get());
    if (lastUser === undefined) return undefined;

    const gaps = MODALITY_GAPS.filter(
      (g) => !caps[g.capability] && g.detect(lastUser.content),
    );
    if (gaps.length === 0) return undefined;

    // For each gap, find profiles whose bound model can cover it — so the
    // hint can name a concrete subagent the LLM should dispatch.
    const hintable = gaps.filter((g) => hasAlternativeModel(this.modelCatalog, g.capability));
    if (hintable.length === 0) return undefined;

    // Extract attachment file paths from the context so the hint can tell
    // the LLM exactly which file the subagent should read. Compression
    // captions embed the original-image path as a quoted string in a text
    // part; we scan the last user message's text parts for these paths.
    const attachmentPaths = extractAttachmentPaths(lastUser.content);

    // Build per-gap profile suggestions: for each modality gap, list the
    // profile names whose defaultModel supports that modality.
    const suggestions = hintable.map((g) => {
      const profiles = profilesForCapability(this.profileCatalog, this.modelCatalog, g.capability);
      return { gap: g, profiles };
    });

    return buildHint(suggestions, attachmentPaths);
  }
}

/**
 * Extract attachment file paths from text parts in the user message.
 *
 * Clients (Desktop / CLI) may replace an inline image with a text notice when
 * the bound model lacks image input. The notice names the saved file so the
 * LLM can hand the path to a subagent. We recognise the common shapes:
 *   - `The original image has been saved to: /path/...`
 *   - `Attached file "name" (mime, bytes): /path/...`
 *   - `The uncompressed original is saved at "/path/..."`
 * Returns paths in the order they appear (most recent first, since we scan
 * from the end). Only returns paths for modalities that have a gap.
 */
function extractAttachmentPaths(parts: readonly ContentPart[]): string[] {
  const paths: string[] = [];
  // Match save/attachment markers followed by an absolute path. Paths may be
  // quoted (`saved at "/path"`) or unquoted (`saved to: /path`), and may be
  // terminated by a quote, newline, or ` — ` (em-dash notice separator).
  //   - `saved at "/tmp/media-originals/abc.png"`  (compression caption)
  //   - `saved to: /home/user/.mirri-code/.../abc.png`  (Desktop client notice)
  //   - `bytes): /tmp/attachments/image.png — open it`  (kap-server attached-file)
  const pattern = /(?:saved (?:at|to)[:\s]*|bytes\):\s*)(["']?)(\/[^\s"'\n—]+)\1/g;
  for (let i = parts.length - 1; i >= 0; i--) {
    const part = parts[i]!;
    if (part.type !== 'text') continue;
    const text = part.text;
    let match: RegExpExecArray | null;
    pattern.lastIndex = 0;
    while ((match = pattern.exec(text)) !== null) {
      const path = match[2]!.trim();
      if (!paths.includes(path)) paths.push(path);
    }
  }
  return paths;
}

/**
 * Find the last user-role message in the context window. The prompt the LLM
 * is about to respond to is the most recent user message.
 */
function lastUserMessage(messages: readonly ContextMessage[]): ContextMessage | undefined {
  for (let i = messages.length - 1; i >= 0; i--) {
    const msg = messages[i]!;
    if (msg.role === 'user') return msg;
  }
  return undefined;
}

/**
 * Check whether any model in the catalog has the given capability set to
 * `true`. Used to determine whether a subagent with the right model could
 * actually handle the modality the main model can't.
 */
function hasAlternativeModel(
  catalog: IModelCatalog,
  capability: keyof ModelCapability,
): boolean {
  try {
    const all = (catalog as unknown as { inspect?: () => Array<{ capabilities: ModelCapability }> }).inspect?.();
    if (all === undefined) return true; // can't verify — don't suppress hint
    return all.some((m) => m.capabilities[capability] === true);
  } catch {
    return true; // on error, don't suppress the hint
  }
}

/**
 * Find agent profiles whose `defaultModel` resolves to a model that supports
 * the given capability. These are the concrete subagents the LLM should
 * dispatch to cover the modality gap. Returns profile names in catalog order.
 */
function profilesForCapability(
  profileCatalog: ISessionAgentProfileCatalog,
  modelCatalog: IModelCatalog,
  capability: keyof ModelCapability,
): string[] {
  const result: string[] = [];
  for (const profile of profileCatalog.list()) {
    const alias = profile.defaultModel;
    if (alias === undefined) continue;
    if (modelSupportsCapability(modelCatalog, alias, capability)) {
      result.push(profile.name);
    }
  }
  return result;
}

/**
 * Resolve a model id through the catalog and check if it has the given
 * capability. Returns false when the id can't be resolved.
 */
function modelSupportsCapability(
  catalog: IModelCatalog,
  alias: string,
  capability: keyof ModelCapability,
): boolean {
  const model = catalog.getById(alias);
  if (model === undefined) return false;
  return model.capabilities[capability] === true;
}

interface GapSuggestion {
  readonly gap: ModalityGap;
  readonly profiles: readonly string[];
}

function buildHint(
  suggestions: readonly GapSuggestion[],
  attachmentPaths: readonly string[],
): string {
  const parts: string[] = [];
  for (const { gap, profiles } of suggestions) {
    if (profiles.length > 0) {
      const names = profiles.map((p) => `'${p}'`).join(', ');
      parts.push(
        `${gap.label}: dispatch the ${names} subagent (pass via subagent_type)`,
      );
    } else {
      parts.push(
        `${gap.label}: dispatch a subagent whose model supports ${gap.label} ` +
        `(see the "Available models" list in the Agent tool description)`,
      );
    }
  }
  const modalities = suggestions.map((s) => s.gap.label).join(', ');
  let hint =
    `Your current model does not support ${modalities} input, but the user's ` +
    `prompt contains ${suggestions.length > 1 ? 'these modalities' : 'this modality'}. ` +
    `To process the content, ${parts.join('; ')}.`;
  // If we found attachment file paths in the context (compression captions),
  // tell the LLM exactly which file the subagent should read — without this,
  // the subagent doesn't know what to ReadMediaFile.
  if (attachmentPaths.length > 0) {
    const pathList = attachmentPaths.map((p) => `"${p}"`).join(', ');
    hint += ` The attachment${attachmentPaths.length > 1 ? 's are' : ' is'} saved at ${pathList} — instruct the subagent to ReadMediaFile on that path.`;
  }
  return hint;
}
