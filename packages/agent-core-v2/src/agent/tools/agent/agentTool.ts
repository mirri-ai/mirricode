/**
 * `tools` domain — `SubagentTool` implementation (the `Agent` tool).
 *
 * The LLM-facing wrapper over the `subagent` domain: translates the tool args
 * into a Profile + Model binding, creates (or resumes) an agent through
 * `IAgentLifecycleService`, drives one turn via `ISessionSubagentService.run`,
 * and mirrors the run onto the calling agent's record stream
 * (`mirrorAgentRun`). The tool also owns the JSON schema + description,
 * approval rule, background-task registration (so the LLM can see the run
 * under TaskList/TaskOutput/TaskStop when `run_in_background=true` or after
 * detach), and terminal text formatting.
 *
 * Spawn bindings use an explicit tool choice first, then the target profile's
 * symbolic model preference, before `resolveSubagentBinding` falls back to the
 * configured secondary model or the caller's model. The selected alias is
 * resolved through the model catalog before lifecycle allocation. A resumed
 * agent keeps the model recorded in its own wire journal — with per-subagent
 * models there is no "child follows the parent's current model" invariant to
 * enforce.
 *
 * Registered via the module-level `registerAgentToolService(ISubagentTool,
 * SubagentTool)` at the bottom of this file — the same "import = register"
 * pattern used by every agent tool. The per-profile tool listings in the
 * description read the full contribution table (not the runtime registry,
 * which only holds tools the caller's own Profile activated), plus any
 * dynamically registered tools. Bound at Agent scope.
 */

import type { IAgentScopeHandle } from '#/_base/di/scope';
import {
  isAbortError,
  isUserCancellation,
  userCancellationReason,
} from '#/_base/utils/abort';
import { toInputJsonSchema } from '#/tool/input-schema';
import { matchesGlobRuleSubject } from '#/tool/rule-match';
import {
  IAgentTaskService,
  type RegisterAgentTaskOptions,
} from '#/agent/task/task';
import { IAgentProfileService } from '#/agent/profile/profile';
import {
  isToolActive as evaluateToolActive,
  resolveActiveToolNames,
} from '#/agent/toolPolicy/evaluate';
import { IAgentToolPolicyService } from '#/agent/toolPolicy/toolPolicy';
import { IAgentPermissionModeService } from '#/agent/permissionMode/permissionMode';
import { IAgentScopeContext } from '#/agent/scopeContext/scopeContext';
import { IAgentLoopService } from '#/agent/loop/loop';
import { IAgentUserToolService } from '#/agent/userTool/userTool';
import {
  ToolAccesses,
  type ExecutableToolContext,
  type ExecutableToolResult,
  type ToolExecution,
} from '#/tool/toolContract';
import {
  getAgentToolContributions,
  registerAgentToolService,
} from '#/agent/toolRegistry/toolContribution';
import { IAgentToolRegistryService, type ToolReference } from '#/agent/toolRegistry/toolRegistry';
import { type AgentProfile } from '#/app/agentProfileCatalog/agentProfileCatalog';
import { ISessionAgentProfileCatalog } from '#/session/sessionAgentProfileCatalog/sessionAgentProfileCatalog';
import { applyProfilePromptPrefix } from '#/app/agentProfileCatalog/promptPrefix';
import {
  subagentAllowlistFor,
  subagentTypeNotAllowedMessage,
} from '#/app/agentProfileCatalog/profile-shared';
import { ILogService } from '#/_base/log/log';
import { IConfigService } from '#/app/config/config';
import { IFlagService } from '#/app/flag/flag';
import { IModelCatalog } from '#/kosong/model/catalog';
import { IModelService } from '#/kosong/model/model';
import { IAgentLifecycleService } from '#/session/agentLifecycle/agentLifecycle';
import { isSubagentMeta, subagentLabels, subagentParentAgentId } from '#/session/agentLifecycle/subagentMetadata';
import { ISessionProcessRunner } from '#/session/process/processRunner';
import { ISessionMetadata } from '#/session/sessionMetadata/sessionMetadata';
import { ISessionWorkspaceContext } from '#/session/workspaceContext/workspaceContext';

import { emitAgentRunSpawned, mirrorAgentRun } from '#/session/subagent/mirrorAgentRun';
import { ISessionSubagentService } from '#/session/subagent/subagent';
import {
  buildSubagentModelDescriptions,
  formatSubagentTimeoutDescription,
  resolveSubagentBinding,
  resolveSubagentTimeoutMs,
  validateModelAlias,
  wrapSubagentModelError,
} from '#/session/subagent/configSection';
import { SECONDARY_MODEL_FLAG_ID } from '#/session/subagent/flag';
import {
  BACKGROUND_AGENT_UNAVAILABLE,
  DEFAULT_PROFILE_NAME,
  ISubagentTool,
  RESUME_WITH_TYPE_UNAVAILABLE,
  RESUMED_LABEL,
  SUBAGENT_STOPPED_MESSAGE,
  SubagentToolInputSchema,
  USER_INTERRUPTED_SUBAGENT_MESSAGE,
  type SubagentToolInput,
} from './agent';
import { SubagentTask, type SubagentHandle } from './subagent-task';

import AGENT_BACKGROUND_DISABLED_DESCRIPTION from './agent-background-disabled.md?raw';
import AGENT_BACKGROUND_DESCRIPTION from './agent-background-enabled.md?raw';
import AGENT_DESCRIPTION_BASE from './agent.md?raw';

const SUBAGENT_TOOL_PARAMETERS = toInputJsonSchema(SubagentToolInputSchema);

export class SubagentTool implements ISubagentTool {
  declare readonly _serviceBrand: undefined;
  readonly name: string = 'Agent';

  /**
   * The `model` parameter is always advertised — it accepts arbitrary model
   * aliases plus the primary/secondary keywords. When the secondary-model
   * experiment is off, primary/secondary behave as no-ops (subagents inherit
   * the caller's model), but explicit aliases still resolve through the catalog.
   */
  readonly parameters: Record<string, unknown> = SUBAGENT_TOOL_PARAMETERS;

  private readonly callerAgentId: string;
  private readonly canRunInBackground: () => boolean;

  constructor(
    @IAgentLifecycleService private readonly lifecycle: IAgentLifecycleService,
    @ISessionSubagentService private readonly subagents: ISessionSubagentService,
    @ISessionAgentProfileCatalog private readonly catalog: ISessionAgentProfileCatalog,
    @IAgentScopeContext scopeContext: IAgentScopeContext,
    @IAgentTaskService private readonly tasks: IAgentTaskService,
    @IAgentProfileService private readonly profile: IAgentProfileService,
    @IAgentToolPolicyService private readonly toolPolicy: IAgentToolPolicyService,
    @IAgentToolRegistryService private readonly toolRegistry: IAgentToolRegistryService,
    @ISessionWorkspaceContext private readonly workspace: ISessionWorkspaceContext,
    @ISessionProcessRunner private readonly processRunner: ISessionProcessRunner,
    @ISessionMetadata private readonly sessionMetadata: ISessionMetadata,
    @ILogService private readonly log: ILogService,
    @IAgentPermissionModeService private readonly permissionMode: IAgentPermissionModeService,
    @IConfigService private readonly config: IConfigService,
    @IFlagService private readonly flags: IFlagService,
    @IModelCatalog private readonly modelCatalog: IModelCatalog,
    @IModelService private readonly modelService: IModelService,
  ) {
    this.callerAgentId = scopeContext.agentId;
    this.canRunInBackground = () =>
      this.toolPolicy.isToolActive('TaskList') &&
      this.toolPolicy.isToolActive('TaskOutput') &&
      this.toolPolicy.isToolActive('TaskStop');
  }

  get description(): string {
    const backgroundDescription = this.canRunInBackground()
      ? AGENT_BACKGROUND_DESCRIPTION
      : AGENT_BACKGROUND_DISABLED_DESCRIPTION;
    let description = `${AGENT_DESCRIPTION_BASE}\n\n${backgroundDescription}`;
    const allowlist = subagentAllowlistFor(this.catalog, this.profile.data());
    const profiles =
      allowlist === undefined
        ? this.catalog.list()
        : this.catalog.list().filter((profile) => allowlist.includes(profile.name));
    const typeLines = buildProfileDescriptions(
      profiles,
      this.knownToolReferences(),
      (profile, name, source) =>
        this.toolPolicy.isToolActiveForProfile(profile, name, source),
      this.flags.enabled(SECONDARY_MODEL_FLAG_ID),
      this.modelCatalog,
    );
    if (typeLines) {
      description += `\n\nAvailable agent types (pass via subagent_type):\n${typeLines}`;
    }
    const modelLines = buildSubagentModelDescriptions(
      this.config,
      this.flags,
      this.profile.data().modelAlias,
      this.modelService.list(),
      this.modelCatalog,
    );
    if (modelLines !== undefined) {
      description += `\n\n${modelLines}`;
    }
    return description;
  }

  private knownToolReferences(): ToolReference[] {
    const refs = new Map<string, ToolReference>();
    for (const contribution of getAgentToolContributions()) {
      refs.set(contribution.options.name, {
        name: contribution.options.name,
        source: contribution.options.source ?? 'builtin',
      });
    }
    for (const ref of this.toolRegistry.listReferences()) {
      if (!refs.has(ref.name)) refs.set(ref.name, ref);
    }
    return [...refs.values()];
  }

  async resolveExecution(args: SubagentToolInput): Promise<ToolExecution> {
    const requestedProfileName = args.subagent_type?.length ? args.subagent_type : undefined;
    const resumeAgentId = args.resume?.trim();

    if (
      resumeAgentId !== undefined &&
      resumeAgentId.length > 0 &&
      requestedProfileName !== undefined
    ) {
      return { output: RESUME_WITH_TYPE_UNAVAILABLE, isError: true };
    }

    const profileNameForDisplay =
      resumeAgentId !== undefined && resumeAgentId.length > 0
        ? this.resumeProfileName(resumeAgentId) ?? RESUMED_LABEL
        : requestedProfileName ?? DEFAULT_PROFILE_NAME;
    const prefix = args.run_in_background === true ? 'Launching background' : 'Launching';
    return {
      description: `${prefix} ${profileNameForDisplay} agent: ${args.description}`,
      accesses: ToolAccesses.none(),
      display: {
        kind: 'agent_call',
        agent_name: profileNameForDisplay,
        prompt: args.prompt,
        background: args.run_in_background,
      },
      approvalRule: this.name,
      matchesRule: (ruleArgs) => matchesGlobRuleSubject(ruleArgs, profileNameForDisplay),
      execute: (ctx) => this.execution(args, ctx),
    };
  }

  private resumeProfileName(agentId: string): string | undefined {
    const target = this.lifecycle.get(agentId);
    if (target === undefined) return undefined;
    return target.accessor.get(IAgentProfileService).data().profileName;
  }

  private async launch(
    args: SubagentToolInput,
    toolCallId: string,
    controller: AbortController,
  ): Promise<SubagentHandle> {
    const requester = this.lifecycle.get(this.callerAgentId);
    if (requester === undefined) {
      throw new Error(`Caller agent "${this.callerAgentId}" does not exist`);
    }

    const resumeAgentId = args.resume?.trim();
    const isResume = resumeAgentId !== undefined && resumeAgentId.length > 0;

    let agentId: string;
    let profileName: string;
    let resolvedModel: string | undefined;
    let promptText = args.prompt;
    if (isResume) {
      const target = this.lifecycle.get(resumeAgentId);
      if (target === undefined) {
        throw new Error(`Agent instance "${resumeAgentId}" does not exist`);
      }
      await this.ensureOwnedIdleSubagent(resumeAgentId, target);
      agentId = target.id;
      profileName =
        target.accessor.get(IAgentProfileService).data().profileName ?? RESUMED_LABEL;
      resolvedModel = target.accessor.get(IAgentProfileService).data().modelAlias;
    } else {
      const requestedProfileName = args.subagent_type?.length
        ? args.subagent_type
        : DEFAULT_PROFILE_NAME;
      await this.catalog.ready;
      const own = this.profile.data();
      const allowlist = subagentAllowlistFor(this.catalog, own);
      if (allowlist !== undefined && !allowlist.includes(requestedProfileName)) {
        throw new Error(subagentTypeNotAllowedMessage(requestedProfileName, allowlist));
      }
      const profile = this.catalog.get(requestedProfileName);
      if (profile === undefined) {
        throw new Error(`Unknown agent type: "${requestedProfileName}"`);
      }
      if (own.modelAlias === undefined) {
        throw new Error('Caller agent has no model bound');
      }
      // Model resolution priority: LLM-specified → profile's defaultModel →
      // profile's modelPreference (primary/secondary) → inherit caller.
      // defaultModel (an arbitrary alias like "claude-sonnet") takes priority
      // over modelPreference (a symbolic primary/secondary keyword) because
      // it's a more specific declaration of which model the profile wants.
      const requestedModel = args.model ?? profile.defaultModel ?? profile.modelPreference;
      // Validate explicit model alias before launch; primary/secondary are
      // handled by resolveSubagentBinding, but arbitrary aliases must resolve
      // through the catalog. An invalid alias surfaces as isError feedback so
      // the LLM can correct itself instead of silently falling back.
      const modelValidation = validateModelAlias(
        requestedModel,
        this.modelCatalog,
        this.modelService.list(),
      );
      if (typeof modelValidation === 'object' && 'error' in modelValidation) {
        throw new Error(modelValidation.error);
      }
      const binding = resolveSubagentBinding(
        this.config,
        this.flags,
        { modelAlias: own.modelAlias, thinkingLevel: own.thinkingLevel },
        requestedModel,
        this.modelCatalog,
      );
      if (binding === undefined) {
        // Alias didn't resolve (should be caught by validateModelAlias above,
        // but guard against race conditions or catalog changes).
        throw new Error(`Model "${requestedModel}" is not configured. Leave the model parameter empty to use the default.`);
      }
      let created: IAgentScopeHandle;
      try {
        this.modelCatalog.get(binding.model);
        created = await this.lifecycle.create({
          binding: {
            profile: profile.name,
            model: binding.model,
            thinking: binding.thinking,
          },
          labels: subagentLabels(this.callerAgentId),
        });
      } catch (error) {
        throw wrapSubagentModelError(error, binding.model, own.modelAlias);
      }
      created.accessor.get(IAgentPermissionModeService).setMode(this.permissionMode.mode);
      created.accessor
        .get(IAgentUserToolService)
        .inheritUserTools(requester.accessor.get(IAgentUserToolService));
      agentId = created.id;
      profileName = profile.name;
      resolvedModel = binding.model;
      promptText = await applyProfilePromptPrefix(profile, args.prompt, {
        cwd: this.workspace.workDir,
        runner: this.processRunner,
        log: this.log,
      });
    }

    const runInBackground = args.run_in_background === true;
    emitAgentRunSpawned(requester, agentId, {
      profileName,
      parentToolCallId: toolCallId,
      description: args.description,
      runInBackground,
      model: resolvedModel,
    });

    const run = await this.subagents.run(
      agentId,
      { kind: 'prompt', prompt: promptText },
      { signal: controller.signal },
    );
    const mirrored = mirrorAgentRun(requester, run, {
      profileName,
      prompt: promptText,
      signal: controller.signal,
      cancel: (reason) => {
        controller.abort(reason);
      },
    });
    return {
      agentId,
      profileName,
      model: resolvedModel,
      completion: mirrored.then((r) => ({ result: r.summary, usage: r.usage })),
    };
  }

  private async ensureOwnedIdleSubagent(
    agentId: string,
    target: IAgentScopeHandle,
  ): Promise<void> {
    const meta = (await this.sessionMetadata.read()).agents?.[agentId];
    if (!isSubagentMeta(meta)) {
      throw new Error(`Agent instance "${agentId}" is not a subagent`);
    }
    if (subagentParentAgentId(meta) !== this.callerAgentId) {
      throw new Error(`Agent instance "${agentId}" does not belong to this parent agent`);
    }
    if (target.accessor.get(IAgentLoopService).status().state === 'running') {
      throw new Error(`Agent instance "${agentId}" is already running and cannot run concurrently`);
    }
  }

  private async execution(
    args: SubagentToolInput,
    { toolCallId, signal }: ExecutableToolContext,
  ): Promise<ExecutableToolResult> {
    try {
      signal.throwIfAborted();
      const runInBackground = args.run_in_background === true;
      const requestedProfileName = args.subagent_type?.length ? args.subagent_type : undefined;
      const resumeAgentId = args.resume?.trim();
      const isResume = resumeAgentId !== undefined && resumeAgentId.length > 0;

      if (isResume && requestedProfileName !== undefined) {
        return { output: RESUME_WITH_TYPE_UNAVAILABLE, isError: true };
      }

      const allowBackground = this.canRunInBackground();
      if (runInBackground && !allowBackground) {
        return { output: BACKGROUND_AGENT_UNAVAILABLE, isError: true };
      }
      const timeoutMs = resolveSubagentTimeoutMs(this.config);

      const controller = new AbortController();
      const abortBeforeRegister = (): void => {
        controller.abort(signal.reason);
      };
      if (!runInBackground) {
        signal.addEventListener('abort', abortBeforeRegister, { once: true });
      }

      let handle: SubagentHandle;
      try {
        handle = await this.launch(args, toolCallId, controller);
      } catch (error) {
        signal.removeEventListener('abort', abortBeforeRegister);
        this.log.warn('subagent launch failed', {
          toolCallId,
          runInBackground,
          operation: isResume ? 'resume' : 'spawn',
          subagentType: requestedProfileName ?? DEFAULT_PROFILE_NAME,
          resumeAgentId: isResume ? resumeAgentId : undefined,
          error,
        });
        throw error;
      }

      let taskId: string;
      try {
        const registerOptions: RegisterAgentTaskOptions = {
          detached: runInBackground,
          timeoutMs,
          signal: runInBackground ? undefined : signal,
        };
        taskId = this.tasks.registerTask(
          new SubagentTask(handle, args.description, controller),
          registerOptions,
        );
        signal.removeEventListener('abort', abortBeforeRegister);
      } catch (error) {
        controller.abort();
        void handle.completion.catch(() => {});
        signal.removeEventListener('abort', abortBeforeRegister);
        this.log?.warn('background agent task registration failed', {
          toolCallId,
          agentId: handle.agentId,
          subagentType: handle.profileName,
          error,
        });
        const message = error instanceof Error ? error.message : String(error);
        return {
          output:
            message === 'Too many detached tasks are already running.'
              ? 'Too many background tasks are already running.'
              : message,
          isError: true,
        };
      }

      if (runInBackground) {
        return {
          output: formatBackgroundAgentResult(taskId, handle, args.description, allowBackground),
        };
      }

      const release = await this.tasks.waitForForegroundRelease(taskId);
      if (release === 'detached') {
        return {
          output: formatBackgroundAgentResult(taskId, handle, args.description, allowBackground),
        };
      }
      return await this.formatForegroundResult(taskId, handle, timeoutMs);
    } catch (error) {
      return { output: `subagent error: ${launchErrorMessage(error, signal)}`, isError: true };
    }
  }

  private async formatForegroundResult(
    taskId: string,
    handle: SubagentHandle,
    timeoutMs: number,
  ): Promise<ExecutableToolResult> {
    const info = this.tasks.getTask(taskId);
    if (info?.status === 'completed') {
      return {
        output: formatForegroundAgentSuccess(handle, await this.tasks.readOutput(taskId)),
      };
    }
    const timedOut = info?.status === 'timed_out';
    const message = timedOut
      ? `Agent timed out after ${formatSubagentTimeoutDescription(timeoutMs)}.`
      : formatSubagentStoppedMessage(info?.stopReason);
    return {
      output: formatForegroundAgentFailure(handle, message, timedOut),
      isError: true,
    };
  }
}

registerAgentToolService(ISubagentTool, SubagentTool, { name: 'Agent', domain: 'subagent', description: 'Launch a subagent to handle a task.' });


function buildProfileDescriptions(
  profiles: readonly AgentProfile[],
  tools: readonly ToolReference[],
  isToolActive: (
    profile: { readonly tools?: readonly string[]; readonly disallowedTools?: readonly string[] },
    name: string,
    source: ToolReference['source'],
  ) => boolean,
  showModelPreferences: boolean,
  modelCatalog?: IModelCatalog,
): string {
  return profiles
    .map((profile) => {
      const details = [profile.description, profile.whenToUse].filter(
        (part): part is string => part !== undefined && part.length > 0,
      );
      const header = details.length === 0 ? `- ${profile.name}` : `- ${profile.name}: ${details.join(' ')}`;
      const metaLines: string[] = [];
      if (showModelPreferences && profile.modelPreference !== undefined) {
        metaLines.push(`Model preference: ${profile.modelPreference}`);
      }
      // Expose the profile's explicit defaultModel and its resolved capabilities
      // so the LLM can pick a profile whose model supports the input modality.
      if (profile.defaultModel !== undefined) {
        const caps = resolveModelCapabilityHint(profile.defaultModel, modelCatalog);
        metaLines.push(caps !== undefined ? `Default model: ${profile.defaultModel} (${caps})` : `Default model: ${profile.defaultModel}`);
      }
      // Expose declared capability tags so the LLM knows what each profile
      // is designed for (e.g. code.explore, web.search).
      if (profile.capabilitiesRequired !== undefined && profile.capabilitiesRequired.length > 0) {
        metaLines.push(`Capabilities: ${profile.capabilitiesRequired.join(', ')}`);
      }
      const headerLines = metaLines.length === 0 ? header : `${header}\n  ${metaLines.join('\n  ')}`;
      const activeTools = resolveActiveToolNames(profile);
      const externallyRestricted = tools.some(
        (tool) =>
          evaluateToolActive(profile, tool.name, tool.source) &&
          !isToolActive(profile, tool.name, tool.source),
      );
      if (externallyRestricted) {
        const effectiveTools = tools
          .filter((tool) => isToolActive(profile, tool.name, tool.source))
          .map((tool) => tool.name);
        if (effectiveTools.length === 0) {
          return `${headerLines}\n  Tools: none`;
        }
        return `${headerLines}\n  Tools: ${effectiveTools.join(', ')}`;
      }
      if (activeTools === undefined) {
        if ((profile.disallowedTools?.length ?? 0) > 0) {
          return `${headerLines}\n  Tools: all except ${profile.disallowedTools!.join(', ')}`;
        }
        return `${headerLines}\n  Tools: all`;
      }
      if (activeTools.length === 0) {
        return `${headerLines}\n  Tools: none`;
      }
      return `${headerLines}\n  Tools: ${activeTools.join(', ')}`;
    })
    .join('\n');
}

/**
 * Resolve a human-readable capability hint for a model id (e.g.
 * "media" → "supports vision, tool-use"). Returns `undefined` when
 * the catalog has no entry for the id or the model has no notable caps.
 */
function resolveModelCapabilityHint(
  alias: string,
  catalog?: IModelCatalog,
): string | undefined {
  if (catalog === undefined) return undefined;
  const model = catalog.getById(alias);
  if (model === undefined) return undefined;
  const caps = model.capabilities;
  const labels: string[] = [];
  if (caps['image_in']) labels.push('vision');
  if (caps['video_in']) labels.push('video');
  if (caps['audio_in']) labels.push('audio');
  if (caps['tool_use']) labels.push('tool-use');
  if (caps['thinking']) labels.push('thinking');
  return labels.length > 0 ? `supports ${labels.join(', ')}` : undefined;
}

function formatBackgroundAgentResult(
  taskId: string,
  handle: SubagentHandle,
  description: string,
  allowBackground: boolean,
): string {
  return [
    `task_id: ${taskId}`,
    'status: running',
    `agent_id: ${handle.agentId}`,
    `actual_subagent_type: ${handle.profileName}`,
    handle.model ? `resolved_model: ${handle.model}` : '',
    'automatic_notification: true',
    '',
    `description: ${description}`,
    '',
    allowBackground
      ? `next_step: The completion arrives automatically in a later turn — do NOT wait, poll, or call TaskOutput on it; continue with other work or hand back to the user. (If you have nothing to do until it finishes, run such tasks in the foreground next time.)`
      : 'next_step: The completion arrives automatically in a later turn.',
    `resume_hint: To continue or recover this same subagent later, call Agent(resume="${handle.agentId}", prompt="..."). The parameter is agent_id ("${handle.agentId}"), NOT task_id ("${taskId}") or source_id from a later <notification>. Recovery cases: a later <notification type="task.lost" | "task.failed" | "task.killed"> for this subagent — its conversation history is preserved across session restarts and resume will pick it up.`,
  ].filter(Boolean).join('\n');
}

function formatForegroundAgentSuccess(handle: SubagentHandle, result: string): string {
  return [
    `agent_id: ${handle.agentId}`,
    `actual_subagent_type: ${handle.profileName}`,
    handle.model ? `resolved_model: ${handle.model}` : '',
    'status: completed',
    '',
    '[summary]',
    result,
  ].filter(Boolean).join('\n');
}

function formatForegroundAgentFailure(
  handle: SubagentHandle,
  message: string,
  timedOut: boolean,
): string {
  const lines = [
    `agent_id: ${handle.agentId}`,
    `actual_subagent_type: ${handle.profileName}`,
    handle.model ? `resolved_model: ${handle.model}` : '',
    'status: failed',
    '',
    `subagent error: ${message}`,
  ].filter(Boolean);
  if (timedOut) {
    lines.push(
      `resume_hint: Continue with Agent(resume="${handle.agentId}", prompt="continue"). Use agent_id only; do not set subagent_type. The subagent retains its prior context; redo any unfinished tool call if its result was lost.`,
    );
  }
  return lines.join('\n');
}

function launchErrorMessage(error: unknown, signal: AbortSignal): string {
  if (isUserCancellation(signal.reason)) return USER_INTERRUPTED_SUBAGENT_MESSAGE;
  if (isAbortError(error)) return formatSubagentStoppedMessage(errorMessage(signal.reason));
  return error instanceof Error ? error.message : String(error);
}

function formatSubagentStoppedMessage(reason: string | undefined): string {
  const normalized = reason?.trim();
  if (normalized === userCancellationReason().message) return USER_INTERRUPTED_SUBAGENT_MESSAGE;
  if (normalized === undefined || normalized.length === 0) return SUBAGENT_STOPPED_MESSAGE;
  return `${SUBAGENT_STOPPED_MESSAGE} Reason: ${normalized}`;
}

function errorMessage(error: unknown): string | undefined {
  if (typeof error === 'string') return error;
  if (error instanceof Error) return error.message;
  return undefined;
}
