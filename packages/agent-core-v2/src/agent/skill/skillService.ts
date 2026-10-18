/**
 * `skill` domain — `IAgentSkillService` implementation.
 *
 * Resolves skills from the session catalog, renders the activation prompt,
 * records the activation as a `skill.activate` fact through `wire.dispatch`
 * (a stateless, identity-apply Op), derives the `skill.activated` event
 * through the Op's `toEvent`, drives user-slash activations into a new turn via
 * `prompt`, and reports `skill_invoked` / `flow_invoked` through `telemetry`.
 * Inline message references (US3.6 / ADR-004) are analyzed through
 * `analyzeInlineReferences`, which returns the turn-message-sequence plan the
 * prompt scheduler materializes. `wire.replay` reapplies the fact as a no-op,
 * so neither the event nor telemetry fires on resume (matching the former
 * `restoring` guard). Bound at Agent scope.
 */

import { randomUUID } from 'node:crypto';
import { realpathSync } from 'node:fs';
import { LifecycleScope, ScopeActivation, registerScopedService } from '#/_base/di/scope';
import { parseInlineReferences, COMMAND_TARGET_PREFIX } from '@mirri-ai/protocol';

import type { ContextMessage, SkillActivationOrigin } from '#/agent/contextMemory/types';
import type { WithSkillsEntry } from '#/agent/contextMemory/protocolMessage';
import { renderUserInlineSkillPrompt, renderUserSlashSkillPrompt } from './prompt';
import { ISessionContext } from '#/session/sessionContext/sessionContext';
import { Disposable } from '#/_base/di/lifecycle';
import { ErrorCodes, Error2 } from '#/errors';
import { isUserActivatableSkillType, type SkillDefinition } from '#/app/skillCatalog/types';
import { IAgentPromptService, type PromptHandle } from '#/agent/prompt/prompt';
import { ITelemetryService } from '#/app/telemetry/telemetry';
import type { Turn } from '#/agent/loop/loop';
import { IWireService } from '#/wire/wire';
import {
  IAgentSkillService,
  IInlineSkillAnalyzer,
  type InlineSkillReferenceAnalysis,
  type InlineSkillWrapper,
  type PendingInlineCommand,
  type SkillActivationInput,
  type SkippedInlineReference,
} from './skill';
import { skillActivate } from './skillOps';
import { ISessionSkillCatalog } from '#/session/sessionSkillCatalog/skillCatalog';

export class AgentSkillService extends Disposable implements IAgentSkillService {
  declare readonly _serviceBrand: undefined;

  constructor(
    @ISessionSkillCatalog private readonly skillCatalog: ISessionSkillCatalog,
    @IAgentPromptService private readonly prompt: IAgentPromptService,
    @IWireService private readonly wire: IWireService,
    @ITelemetryService private readonly telemetry: ITelemetryService,
    @ISessionContext private readonly sessionContext: ISessionContext,
  ) {
    super();
    // Catalog changes (source rescan, plugin reload) invalidate cached
    // symlink aliases so a moved skill file cannot resolve through a
    // stale mapping.
    this._register(this.skillCatalog.onDidChange(() => pathAliasCache.clear()));
  }

  async activate(input: SkillActivationInput): Promise<Turn> {
    const handle = await this.enqueueDeferred(input);
    const turn = await handle.launched;
    if (turn === undefined) {
      throw new Error2(
        ErrorCodes.TURN_AGENT_BUSY,
        'Cannot activate skill while another turn is active',
      );
    }
    return turn;
  }

  async enqueueDeferred(input: SkillActivationInput): Promise<PromptHandle> {
    const skill = await this.assertUserActivatable(input);
    const skillArgs = input.args ?? '';
    const origin: SkillActivationOrigin = {
      kind: 'skill_activation',
      activationId: randomUUID(),
      skillName: skill.name,
      trigger: 'user-slash',
      skillType: skill.metadata.type,
      skillPath: skill.path,
      skillSource: skill.source,
      skillArgs: input.args,
    };
    const intent: ContextMessage = {
      role: 'user',
      content: [
        {
          type: 'text',
          text: `/${skill.name}${skillArgs.length > 0 ? ` ${skillArgs}` : ''}`,
        },
      ],
      toolCalls: [],
      origin,
    };
    return this.prompt.enqueue({
      message: intent,
      // Dequeue time: render the skill body, record the activation fact, and
      // emit telemetry — none of this may happen while the request is merely
      // parked behind a running turn.
      materialize: async () => {
        this.recordActivation(origin);
        return {
          role: 'user',
          content: [
            {
              type: 'text',
              text: renderUserSlashSkillPrompt({
                skillName: skill.name,
                skillArgs,
                skillContent: this.renderSkillPrompt(skill, skillArgs),
                skillSource: skill.source,
                skillDir: skill.dir,
              }),
            },
          ],
          toolCalls: [],
          origin,
        };
      },
    });
  }

  recordModelToolActivation(origin: SkillActivationOrigin): void {
    this.recordActivation(origin);
  }

  /**
   * Parse a user message's inline references and build the turn-message-sequence
   * plan for the moment the message materializes into a turn (US3.6 / ADR-004):
   * one activation wrapper per unique skill path (appearance order), plus the
   * body with successful skill references replaced by their bare display name.
   * Skill resolution, gating, rendering, and activation recording all happen
   * here — at call time, which is the message's real admission time, not its
   * submission time — so a skill's state changes while the message sat queued
   * are reflected as they are. Activation ids derive from
   * `<messageId>#<reference index>` so retries never double-record. Failed
   * references degrade to plain text (kept verbatim in the body) and are
   * reported with a reason direction; command references are surfaced as
   * pending commands for the submitting client — the engine never executes
   * them.
   */
  async analyzeInlineReferences(text: string, messageId: string): Promise<InlineSkillReferenceAnalysis> {
    await this.skillCatalog.ready;
    const parsed = parseInlineReferences(text);

    const wrappers: InlineSkillWrapper[] = [];
    const pendingCommands: PendingInlineCommand[] = [];
    const skipped: SkippedInlineReference[] = [];
    const seenPaths = new Set<string>();
    const activatedSkills: WithSkillsEntry[] = [];
    const bodySpans: Array<{ start: number; end: number; replacement: string }> = [];
    let referenceIndex = 0;

    for (const reference of parsed.references) {
      if (reference.kind === 'command') {
        pendingCommands.push({
          name: reference.target.slice(COMMAND_TARGET_PREFIX.length),
          display: reference.display,
          target: reference.target,
        });
        // Command links stay verbatim in the body — the engine does not
        // execute commands; the client dispatches them from the response.
        continue;
      }

      const skill = this.resolveSkillByPath(reference.target);
      if (skill === undefined) {
        skipped.push({ name: reference.display, target: reference.target, reason: 'skill_not_found' });
        continue;
      }
      if (!isUserActivatableSkillType(skill.metadata.type)) {
        skipped.push({
          name: reference.display,
          target: reference.target,
          reason: 'skill_not_activatable',
        });
        continue;
      }

      const index = referenceIndex++;
      if (seenPaths.has(skill.path)) {
        // Repeated reference to the same resolved skill: collapse to the
        // first activation (path is the fold key — same path, one wrapper).
        bodySpans.push({ start: reference.start, end: reference.end, replacement: `[${reference.display}]` });
        continue;
      }
      seenPaths.add(skill.path);
      activatedSkills.push({ name: skill.name, target: skill.path, trigger: 'user-inline' });

      const origin: SkillActivationOrigin = {
        kind: 'skill_activation',
        activationId: `${messageId}#${index}`,
        skillName: skill.name,
        trigger: 'user-inline',
        skillType: skill.metadata.type,
        skillPath: skill.path,
        skillSource: skill.source,
      };
      wrappers.push({
        skillName: skill.name,
        target: skill.path,
        materialize: () => {
          this.recordActivation(origin);
          return Promise.resolve({
            role: 'user',
            content: [
              {
                type: 'text',
                text: renderUserInlineSkillPrompt({
                  skillName: skill.name,
                  skillArgs: '',
                  skillContent: this.renderSkillPrompt(skill, ''),
                  skillSource: skill.source,
                  skillDir: skill.dir,
                }),
              },
            ],
            toolCalls: [],
            origin,
          } satisfies ContextMessage);
        },
      });
      bodySpans.push({ start: reference.start, end: reference.end, replacement: `[${reference.display}]` });
    }

    return {
      body: applyReplacements(text, bodySpans),
      wrappers,
      pendingCommands,
      skipped,
      activatedSkills,
    };
  }

  /**
   * Resolve a skill by its SKILL.md path — the inline-reference resolution
   * authority (ADR-004: which file the link points at is which skill gets
   * activated). The catalog's paths are realpaths; a reference written from a
   * symlinked prefix (e.g. macOS `/var` vs `/private/var`) gets one realpath
   * fallback so the same physical file still resolves.
   */
  private resolveSkillByPath(path: string): SkillDefinition | undefined {
    const skills = this.skillCatalog.catalog.listSkills();
    const exact = skills.find((item) => item.path === path);
    if (exact !== undefined) return exact;
    // Positive results only: a failed realpath (skill not created yet) must
    // not be cached, or a later retry on the same path would stay broken
    // until process restart.
    let resolved = pathAliasCache.get(path);
    if (resolved === undefined) {
      resolved = realpathSyncSafe(path);
      if (resolved === undefined || resolved === path) return undefined;
      pathAliasCache.set(path, resolved);
    }
    return skills.find((item) => item.path === resolved);
  }

  private async assertUserActivatable(input: SkillActivationInput): Promise<SkillDefinition> {
    await this.skillCatalog.ready;
    const skill = this.skillCatalog.catalog.resolveSkill(input.name);
    if (skill === undefined) {
      throw new Error2(ErrorCodes.SKILL_NOT_FOUND, `Skill "${input.name}" was not found`);
    }
    if (!isUserActivatableSkillType(skill.metadata.type)) {
      throw new Error2(
        ErrorCodes.SKILL_TYPE_UNSUPPORTED,
        `Skill "${skill.name}" cannot be activated by the user`,
      );
    }
    return skill;
  }

  private recordActivation(origin: SkillActivationOrigin): void {
    this.wire.dispatch(skillActivate({ origin }));
    this.publishActivation(origin);
  }

  private renderSkillPrompt(skill: SkillDefinition, rawArgs: string): string {
    return this.skillCatalog.catalog.renderSkillPrompt(skill, rawArgs, {
      sessionId: this.sessionContext.sessionId,
    });
  }

  private publishActivation(origin: SkillActivationOrigin): void {
    this.telemetry.track2('skill_invoked', {
      skill_name: origin.skillName,
      trigger: origin.trigger,
    });
    if (origin.skillType === 'flow') {
      this.telemetry.track2('flow_invoked', {
        flow_name: origin.skillName,
      });
    }
  }
}

/**
 * Replace inline reference spans with their display replacements, leaving
 * everything else (including failed references, which never enter `spans`)
 * verbatim. Spans are non-overlapping and ordered — parser guarantees.
 */
function applyReplacements(
  text: string,
  spans: ReadonlyArray<{ start: number; end: number; replacement: string }>,
): string {
  let result = '';
  let cursor = 0;
  for (const { start, end, replacement } of spans) {
    result += text.slice(cursor, start) + replacement;
    cursor = end;
  }
  return result + text.slice(cursor);
}

// Physical path-alias cache (symlinked prefixes such as macOS `/var`).
// The symlink resolution itself is stable per process, but entries are
// dropped on catalog change anyway so a re-scan that moves skill files
// cannot serve stale mappings. Bounded in practice: only distinct failed
// exact-path lookups ever reach here.
const pathAliasCache = new Map<string, string>();

/** Realpath a referenced skill path (best effort — undefined when it fails). */
function realpathSyncSafe(p: string): string | undefined {
  try {
    return realpathSync(p).replaceAll('\\', '/');
  } catch {
    return undefined;
  }
}

registerScopedService(
  LifecycleScope.Agent,
  IAgentSkillService,
  AgentSkillService,
  ScopeActivation.OnScopeCreated,
  'skill',
);

// The skill service is also the inline-reference analyzer for the prompt
// scheduler (US3.6): same domain, same scope, narrow interface.
registerScopedService(
  LifecycleScope.Agent,
  IInlineSkillAnalyzer,
  AgentSkillService,
  ScopeActivation.OnScopeCreated,
  'skill',
);
