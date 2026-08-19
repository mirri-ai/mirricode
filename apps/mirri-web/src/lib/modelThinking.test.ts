import { computed, nextTick, reactive } from 'vue';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { AppModel, AppSession } from '../api/types';
import {
  useModelProviderState,
  type UseModelProviderStateDeps,
} from '../composables/client/useModelProviderState';
import type { ExtendedState } from '../composables/useMirriWebClient';
import {
  commitLevel,
  defaultThinkingLevelFor,
  effectiveThinkingLevel,
  effortLabel,
  isThinkingOn,
  levelDeclaredBy,
  modelThinkingAvailability,
  segmentsFor,
  thinkingLevelForModelSwitch,
  thinkingLevelToConfig,
} from './modelThinking';
import type { ModelThinkingInfo } from './modelThinking';

const apiMock = vi.hoisted(() => ({
  updateSession: vi.fn(),
  listModels: vi.fn(),
  setConfig: vi.fn(),
  activateSkill: vi.fn(),
}));

vi.mock('../api', () => ({
  getMirriWebApi: () => apiMock,
}));

function model(partial: ModelThinkingInfo): ModelThinkingInfo {
  return partial;
}

describe('modelThinking', () => {
  describe('modelThinkingAvailability', () => {
    it('defaults to toggle when model is unknown', () => {
      expect(modelThinkingAvailability(undefined)).toBe('toggle');
    });

    it('detects always_thinking capability', () => {
      expect(modelThinkingAvailability(model({ capabilities: ['always_thinking'] }))).toBe('always-on');
    });

    it('detects thinking capability', () => {
      expect(modelThinkingAvailability(model({ capabilities: ['thinking'] }))).toBe('toggle');
    });

    it('detects adaptive thinking', () => {
      expect(modelThinkingAvailability(model({ adaptiveThinking: true }))).toBe('toggle');
    });

    it('marks models without thinking support as unsupported', () => {
      expect(modelThinkingAvailability(model({ capabilities: ['vision'] }))).toBe('unsupported');
    });
  });

  describe('defaultThinkingLevelFor', () => {
    it('returns off for unsupported models', () => {
      expect(defaultThinkingLevelFor(model({ capabilities: [] }))).toBe('off');
    });

    it('returns the declared default effort for effort models', () => {
      expect(defaultThinkingLevelFor(model({ capabilities: ['thinking'], supportEfforts: ['low', 'high', 'max'], defaultEffort: 'high' }))).toBe('high');
    });

    it('falls back to the middle effort when no default is declared', () => {
      expect(defaultThinkingLevelFor(model({ capabilities: ['thinking'], supportEfforts: ['low', 'high', 'max'] }))).toBe('high');
      expect(defaultThinkingLevelFor(model({ capabilities: ['thinking'], supportEfforts: ['low', 'high'] }))).toBe('high');
    });

    it('returns on for boolean thinking models', () => {
      expect(defaultThinkingLevelFor(model({ capabilities: ['thinking'] }))).toBe('on');
    });
  });

  describe('segmentsFor', () => {
    it('shows off/on for boolean toggle models', () => {
      expect(segmentsFor(model({ capabilities: ['thinking'] }))).toEqual(['on', 'off']);
    });

    it('shows only on for always-on models', () => {
      expect(segmentsFor(model({ capabilities: ['always_thinking'] }))).toEqual(['on']);
    });

    it('shows only off for unsupported models', () => {
      expect(segmentsFor(model({ capabilities: [] }))).toEqual(['off']);
    });

    it('prefixes off to effort lists for toggle effort models', () => {
      expect(segmentsFor(model({ capabilities: ['thinking'], supportEfforts: ['low', 'high', 'max'] }))).toEqual(['off', 'low', 'high', 'max']);
    });

    it('omits off for always-on effort models', () => {
      expect(segmentsFor(model({ capabilities: ['always_thinking'], supportEfforts: ['low', 'high'] }))).toEqual(['low', 'high']);
    });
  });

  const effortModel = model({ capabilities: ['thinking'], supportEfforts: ['low', 'high', 'max'], defaultEffort: 'high' });
  const booleanModel = model({ capabilities: ['thinking'] });
  const alwaysOnModel = model({ capabilities: ['always_thinking'] });
  const maxOnlyModel = model({ capabilities: ['always_thinking'], supportEfforts: ['max'], defaultEffort: 'max' });
  const unsupportedModel = model({ capabilities: [] });

  describe('thinkingLevelForModelSwitch', () => {
    it('pre-selects the target model default effort on a switch', () => {
      expect(thinkingLevelForModelSwitch(effortModel, 'off', true)).toBe('high');
      expect(thinkingLevelForModelSwitch(effortModel, 'max', true)).toBe('high');
      expect(thinkingLevelForModelSwitch(effortModel, undefined, true)).toBe('high');
    });

    it('keeps the current level when re-selecting the same model', () => {
      expect(thinkingLevelForModelSwitch(effortModel, 'off', false)).toBe('off');
      expect(thinkingLevelForModelSwitch(effortModel, 'max', false)).toBe('max');
      expect(thinkingLevelForModelSwitch(effortModel, undefined, false)).toBeUndefined();
    });

    it('pre-selects on for boolean models on switch', () => {
      expect(thinkingLevelForModelSwitch(booleanModel, 'off', true)).toBe('on');
    });

    it('keeps on for boolean models when re-selecting', () => {
      expect(thinkingLevelForModelSwitch(booleanModel, 'on', false)).toBe('on');
    });

    it('pre-selects the default for always-on models on switch', () => {
      expect(thinkingLevelForModelSwitch(alwaysOnModel, 'off', true)).toBe('on');
    });

    it('pre-selects off for unsupported models on switch', () => {
      expect(thinkingLevelForModelSwitch(unsupportedModel, 'high', true)).toBe('off');
    });
  });

  describe('effectiveThinkingLevel', () => {
    it('returns the stored level when present', () => {
      expect(effectiveThinkingLevel(effortModel, 'high')).toBe('high');
    });

    it('falls back to the model default when level is undefined', () => {
      expect(effectiveThinkingLevel(effortModel, undefined)).toBe('high');
    });

    it('falls back to on for boolean models when level is undefined', () => {
      expect(effectiveThinkingLevel(booleanModel, undefined)).toBe('on');
    });

    it('falls back to off for unsupported models when level is undefined', () => {
      expect(effectiveThinkingLevel(unsupportedModel, undefined)).toBe('off');
    });

    it('returns the stored level even if the model does not declare it', () => {
      expect(effectiveThinkingLevel(effortModel, 'ultra')).toBe('ultra');
    });

    it('returns off as-is', () => {
      expect(effectiveThinkingLevel(effortModel, 'off')).toBe('off');
    });
  });

  describe('levelDeclaredBy', () => {
    it('accepts levels selectable for the model', () => {
      expect(levelDeclaredBy(effortModel, 'low')).toBe(true);
      expect(levelDeclaredBy(effortModel, 'off')).toBe(true);
      expect(levelDeclaredBy(booleanModel, 'on')).toBe(true);
      expect(levelDeclaredBy(alwaysOnModel, 'on')).toBe(true);
    });

    it('rejects levels the model does not declare', () => {
      expect(levelDeclaredBy(booleanModel, 'low')).toBe(false);
      expect(levelDeclaredBy(alwaysOnModel, 'off')).toBe(false);
      expect(levelDeclaredBy(maxOnlyModel, 'low')).toBe(false);
      expect(levelDeclaredBy(unsupportedModel, 'max')).toBe(false);
    });
  });

  describe('thinkingLevelToConfig', () => {
    it('converts off to disabled', () => {
      expect(thinkingLevelToConfig('off')).toEqual({ enabled: false });
    });

    it('converts on to enabled without effort', () => {
      expect(thinkingLevelToConfig('on')).toEqual({ enabled: true });
    });

    it('persists levels below the model top tier as the global default', () => {
      expect(thinkingLevelToConfig('low', ['low', 'high', 'max'])).toEqual({
        enabled: true,
        effort: 'low',
      });
      expect(thinkingLevelToConfig('high', ['low', 'high', 'max'])).toEqual({
        enabled: true,
        effort: 'high',
      });
    });

    it('records only enabled for the model top tier', () => {
      expect(thinkingLevelToConfig('max', ['low', 'high', 'max'])).toEqual({ enabled: true });
      expect(thinkingLevelToConfig('max', ['max'])).toEqual({ enabled: true });
    });

    it('persists concrete levels as-is when the model levels are unknown', () => {
      expect(thinkingLevelToConfig('max')).toEqual({ enabled: true, effort: 'max' });
      expect(thinkingLevelToConfig('ultra')).toEqual({ enabled: true, effort: 'ultra' });
    });
  });

  describe('effortLabel', () => {
    it('capitalizes effort names', () => {
      expect(effortLabel('off')).toBe('Off');
      expect(effortLabel('high')).toBe('High');
      expect(effortLabel('max')).toBe('Max');
    });

    it('returns empty string as-is', () => {
      expect(effortLabel('')).toBe('');
    });
  });

  describe('isThinkingOn', () => {
    it('returns false for off only', () => {
      expect(isThinkingOn('off')).toBe(false);
      expect(isThinkingOn('on')).toBe(true);
      expect(isThinkingOn('high')).toBe(true);
    });
  });

  describe('commitLevel', () => {
    it('keeps off', () => {
      expect(commitLevel(effortModel, 'off')).toBe('off');
    });

    it('resolves on to the model default', () => {
      expect(commitLevel(effortModel, 'on')).toBe('high');
    });

    it('passes concrete efforts through', () => {
      expect(commitLevel(effortModel, 'max')).toBe('max');
    });
  });
});

describe('useModelProviderState thinking on model selection', () => {
  const effortAppModel: AppModel = {
    id: 'provider/effort-model',
    provider: 'provider',
    model: 'effort-model',
    maxContextSize: 128_000,
    capabilities: ['thinking'],
    supportEfforts: ['low', 'high', 'max'],
    defaultEffort: 'high',
  };
  const booleanAppModel: AppModel = {
    id: 'provider/boolean-model',
    provider: 'provider',
    model: 'boolean-model',
    maxContextSize: 128_000,
    capabilities: ['thinking'],
  };
  const maxOnlyAppModel: AppModel = {
    id: 'provider/max-model',
    provider: 'provider',
    model: 'max-model',
    maxContextSize: 128_000,
    capabilities: ['always_thinking'],
    supportEfforts: ['max'],
    defaultEffort: 'max',
  };

  // Per-session thinking state, wired into the deps the same way the facade
  // wires the daemon status: tests seed thinkingBySession and assert via
  // persistSessionProfileMock.
  const persistSessionProfileMock = vi.fn();

  beforeEach(() => {
    apiMock.updateSession.mockReset();
    apiMock.updateSession.mockResolvedValue({});
    apiMock.listModels.mockReset();
    apiMock.listModels.mockResolvedValue([effortAppModel, booleanAppModel, maxOnlyAppModel]);
    apiMock.setConfig.mockReset();
    apiMock.setConfig.mockResolvedValue({});
    apiMock.activateSkill.mockReset();
    apiMock.activateSkill.mockResolvedValue({});
    persistSessionProfileMock.mockReset();
    persistSessionProfileMock.mockResolvedValue(true);
  });

  function createState(options: {
    activeSession?: Pick<AppSession, 'id' | 'model'>;
    defaultModel: string;
  }): ExtendedState {
    return {
      activeSessionId: options.activeSession?.id ?? null,
      sessions: options.activeSession ? [options.activeSession] : [],
      thinking: 'off',
      thinkingBySession: {},
      defaultModel: options.defaultModel,
      inFlightBySession: {},
      parkedPromptsBySession: {},
    } as ExtendedState;
  }

  function createModelProvider(state: ExtendedState, depOverrides: Partial<UseModelProviderStateDeps> = {}) {
    const deps: UseModelProviderStateDeps = {
      pushOperationFailure: vi.fn(),
      refreshSessionStatus: vi.fn().mockResolvedValue(undefined),
      persistSessionProfile: persistSessionProfileMock,
      activity: computed(() => 'idle'),
      updateSession: (id, update) => {
        state.sessions = state.sessions.map((session) =>
          session.id === id ? update(session) : session,
        );
      },
      updateSessionMessages: vi.fn(),
      bindNextPromptId: vi.fn(),
      ...depOverrides,
    };
    const provider = useModelProviderState(state, deps);
    provider.models.value = [effortAppModel, booleanAppModel, maxOnlyAppModel];
    return provider;
  }

  it('pre-selects the target model default effort on a switch', async () => {
    const state = createState({ defaultModel: booleanAppModel.id });
    const provider = createModelProvider(state);

    await provider.setModel(effortAppModel.id);

    expect(state.thinking).toBe('high');
  });

  it('keeps the current level when re-selecting the same model', async () => {
    const state = createState({ defaultModel: effortAppModel.id });
    const provider = createModelProvider(state);

    await provider.setModel(effortAppModel.id);

    expect(state.thinking).toBe('off');
  });

  it('keeps thinking off when an active session inherits the selected default model', async () => {
    const state = createState({
      activeSession: { id: 'session-1', model: '' },
      defaultModel: effortAppModel.id,
    });
    const provider = createModelProvider(state);

    await provider.setModel(effortAppModel.id);

    expect(state.thinking).toBe('off');
    expect(apiMock.updateSession).toHaveBeenCalledWith('session-1', {
      model: effortAppModel.id,
      thinking: undefined,
    });
  });

  it('pre-selects the default effort when switching from a different model', async () => {
    const state = createState({ defaultModel: booleanAppModel.id });
    const provider = createModelProvider(state);

    await provider.setModel(effortAppModel.id);

    expect(state.thinking).toBe('high');
  });

  it('keeps thinking off when re-selecting an explicit new-session draft model', async () => {
    const state = createState({ defaultModel: booleanAppModel.id });
    const provider = createModelProvider(state);

    // Switch the draft to the effort model (catalog default applies), then
    // explicitly turn thinking off — re-selecting the same model must keep it.
    await provider.setModel(effortAppModel.id);
    provider.setThinking('off');
    await provider.setModel(effortAppModel.id);

    expect(state.thinking).toBe('off');
  });

  it('applies the resolved level to the session profile before activating a skill', async () => {
    // Skill activation carries no thinking — the daemon runs at the session
    // profile effort. The resolved level must be persisted there first, or the
    // skill runs at a stale profile effort the UI no longer shows.
    const state = createState({
      activeSession: { id: 'session-1', model: effortAppModel.id },
      defaultModel: booleanAppModel.id,
    });
    const provider = createModelProvider(state);

    await provider.activateSkill('gen-changesets');

    expect(persistSessionProfileMock).toHaveBeenCalledWith({ thinking: 'high' }, 'session-1');
    expect(apiMock.activateSkill).toHaveBeenCalledWith('session-1', 'gen-changesets', undefined);
    // The profile write precedes the activation, mirroring the new-session path.
    const persistOrder = persistSessionProfileMock.mock.invocationCallOrder[0]!;
    const activateOrder = apiMock.activateSkill.mock.invocationCallOrder[0]!;
    expect(persistOrder).toBeLessThan(activateOrder);
  });

  it('does not activate the skill when the thinking profile update fails', async () => {
    // persistSessionProfile resolves false after surfacing the failure itself:
    // activating would run the skill at the stale profile effort, so it must
    // not happen — and activateSkill must not report a second, synthetic error.
    persistSessionProfileMock.mockResolvedValue(false);
    const state = createState({
      activeSession: { id: 'session-1', model: effortAppModel.id },
      defaultModel: booleanAppModel.id,
    });
    const provider = createModelProvider(state);

    await provider.activateSkill('gen-changesets');

    expect(apiMock.activateSkill).not.toHaveBeenCalled();
    expect(state.inFlightBySession['session-1']).toBe(false);
  });

  it('resolves an empty session model through the default model before activating a skill', async () => {
    // The daemon's profile echo can leave session.model '' — the same fallback
    // the prompt/BTW/steer paths apply must hold here too, or the profile gets
    // the raw active level instead of the target session model's default.
    const state = createState({
      activeSession: { id: 'session-1', model: '' },
      defaultModel: effortAppModel.id,
    });
    const provider = createModelProvider(state);

    await provider.activateSkill('gen-changesets');

    expect(persistSessionProfileMock).toHaveBeenCalledWith({ thinking: 'high' }, 'session-1');
    expect(apiMock.activateSkill).toHaveBeenCalledWith('session-1', 'gen-changesets', undefined);
  });

  it('should show the skill as queued without a failure when the daemon parks the activation while the session is busy', async () => {
    const state = createState({
      activeSession: { id: 'session-1', model: effortAppModel.id },
      defaultModel: booleanAppModel.id,
    });
    const pushOperationFailure = vi.fn();
    const provider = createModelProvider(state, {
      // Busy: a running turn keeps the loop occupied, so the daemon replies
      // with a queued receipt instead of starting the skill turn right away.
      activity: computed(() => 'running'),
      pushOperationFailure,
    });
    apiMock.activateSkill.mockResolvedValue({
      activated: true,
      skillName: 'gen-changesets',
      promptId: 'prompt_queued_1',
      status: 'queued',
    } as never);

    await provider.activateSkill('gen-changesets');

    expect(apiMock.activateSkill).toHaveBeenCalledWith('session-1', 'gen-changesets', undefined);
    // No misleading failure notice — the activation was accepted.
    expect(pushOperationFailure).not.toHaveBeenCalled();
  });

  it('should park a busy skill activation in the queue area instead of putting it into the chat', async () => {
    const state = createState({
      activeSession: { id: 'session-1', model: effortAppModel.id },
      defaultModel: booleanAppModel.id,
    });
    const bindNextPromptId = vi.fn();
    const updateSessionMessages = vi.fn();
    const pushOperationFailure = vi.fn();
    const provider = createModelProvider(state, {
      activity: computed(() => 'running'),
      bindNextPromptId,
      updateSessionMessages,
      pushOperationFailure,
    });
    apiMock.activateSkill.mockResolvedValue({
      activated: true,
      skillName: 'gen-changesets',
      promptId: 'prompt_queued_1',
      status: 'queued',
    } as never);

    await provider.activateSkill('gen-changesets');

    // The daemon parked the activation; it must NOT look like a delivered
    // chat message — it is queued, so the optimistic echo must not run.
    expect(updateSessionMessages).not.toHaveBeenCalled();
    // The parked entry is registered for the queue area, keyed by prompt_id so
    // the later :abort / restore paths can address it.
    expect(state.parkedPromptsBySession['session-1']).toEqual([
      { prompt_id: 'prompt_queued_1', text: '/gen-changesets' },
    ]);
    expect(pushOperationFailure).not.toHaveBeenCalled();
  });

  it('pins the catalog default in memory when no thinking preference exists', async () => {
    const state = createState({ defaultModel: effortAppModel.id });
    state.thinking = undefined;
    const provider = createModelProvider(state);

    await provider.loadModels();

    expect(state.thinking).toBe('high');
    expect(apiMock.setConfig).not.toHaveBeenCalled();
  });

  it('drops a session level the active model does not declare', async () => {
    // A level the session ran with must not leak onto a model that cannot run
    // it — resolution falls back to the model default.
    const state = createState({
      activeSession: { id: 'session-1', model: maxOnlyAppModel.id },
      defaultModel: maxOnlyAppModel.id,
    });
    state.thinkingBySession = { 'session-1': 'low' };
    state.thinking = 'low';
    const provider = createModelProvider(state);

    await provider.loadModels();

    expect(state.thinking).toBe('max');
  });

  it('re-resolves the level when the active session switches to another model', async () => {
    const state = reactive(
      createState({
        activeSession: { id: 'session-1', model: maxOnlyAppModel.id },
        defaultModel: maxOnlyAppModel.id,
      }),
    ) as ExtendedState;
    state.sessions = [
      { id: 'session-1', model: maxOnlyAppModel.id },
      { id: 'session-2', model: effortAppModel.id },
    ] as AppSession[];
    state.thinking = 'max';
    createModelProvider(state);

    state.activeSessionId = 'session-2';
    await nextTick();
    // No session level of its own yet — the catalog default applies.
    expect(state.thinking).toBe('high');

    // Switching back resolves the max-only model's own level again.
    state.activeSessionId = 'session-1';
    await nextTick();
    expect(state.thinking).toBe('max');
  });

  it('restores the session own daemon level when switching sessions on the same model', async () => {
    const state = reactive(
      createState({
        activeSession: { id: 'session-1', model: effortAppModel.id },
        defaultModel: effortAppModel.id,
      }),
    ) as ExtendedState;
    state.sessions = [
      { id: 'session-1', model: effortAppModel.id },
      { id: 'session-2', model: effortAppModel.id },
    ] as AppSession[];
    // session-2 ran at max (folded from /status); the current view shows
    // high — the session's own level must win.
    state.thinkingBySession = { 'session-2': 'max' };
    state.thinking = 'high';
    createModelProvider(state);

    state.activeSessionId = 'session-2';
    await nextTick();
    expect(state.thinking).toBe('max');
  });

  it('re-resolves the active session when its daemon level arrives after the switch', async () => {
    const state = reactive(
      createState({
        activeSession: { id: 'session-1', model: effortAppModel.id },
        defaultModel: effortAppModel.id,
      }),
    ) as ExtendedState;
    state.sessions = [
      { id: 'session-1', model: effortAppModel.id },
      { id: 'session-2', model: effortAppModel.id },
    ] as AppSession[];
    state.thinking = 'high';
    createModelProvider(state);

    // Before the /status fold lands, the catalog default is shown.
    state.activeSessionId = 'session-2';
    await nextTick();
    expect(state.thinking).toBe('high');

    // The /status fold arrives with the session's own level — the displayed
    // level follows it without another session switch.
    state.thinkingBySession = { 'session-2': 'max' };
    await nextTick();
    expect(state.thinking).toBe('max');
  });

  it('waits for the session status fold before resolving the prompt thinking', async () => {
    // Cold window after a session switch: the session's own level has not been
    // folded yet — resolveThinkingForPrompt must refresh status first so the
    // prompt carries the session's real level, not the catalog default.
    const refreshSessionStatus = vi.fn().mockResolvedValue(undefined);
    const state = createState({
      activeSession: { id: 'session-1', model: effortAppModel.id },
      defaultModel: effortAppModel.id,
    });
    const provider = createModelProvider(state, { refreshSessionStatus });

    // No session entry yet → refresh fires before resolution.
    const level = await provider.resolveThinkingForPrompt('session-1', effortAppModel.id);
    expect(refreshSessionStatus).toHaveBeenCalledWith('session-1');
    expect(level).toBe('high');

    // After a session entry is folded, refresh is no longer needed.
    refreshSessionStatus.mockClear();
    state.thinkingBySession = { 'session-1': 'low' };
    const level2 = await provider.resolveThinkingForPrompt('session-1', effortAppModel.id);
    expect(refreshSessionStatus).not.toHaveBeenCalled();
    expect(level2).toBe('low');
  });

  it('does not write the global thinking config for the loadModels default pin', async () => {
    const state = createState({ defaultModel: effortAppModel.id });
    state.thinking = undefined;
    const provider = createModelProvider(state);

    await provider.loadModels();

    expect(apiMock.setConfig).not.toHaveBeenCalled();
  });
});
