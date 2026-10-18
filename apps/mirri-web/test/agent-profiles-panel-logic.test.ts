// apps/mirri-web/test/agent-profiles-panel-logic.test.ts
// Pure-logic tests for the AgentProfilesPanel edit flows, extracted from the
// component into `src/lib/agentProfiles.ts` (no component test harness in
// this repo): create-from-template prefill and self-contained payload
// assembly, tools/denylist clear semantics, subagent candidate options, the
// external-change three-way conflict resolution (US-A4-4), and the D16
// edit-face gating (essential / builtin / custom).

import { describe, expect, it } from 'vitest';

import type { AppAgentProfile } from '../src/api/types';
import {
  AGENT_NAME_MAX_LENGTH,
  CONFLICT_FIELDS,
  applyFileValue,
  buildConflictRows,
  buildCreateProfilePayload,
  buildUpdateProfilePayload,
  canResetToDefault,
  canToggleProfile,
  editingKind,
  emptyFormValues,
  enabledSubagentOptions,
  externalFieldText,
  formFieldText,
  formValuesFromProfile,
  overrideBadgeKind,
  parseToolList,
  shouldInterceptSave,
  validateAgentProfileForm,
  type ConflictField,
  type EditingKind,
  type ProfileFormValues,
} from '../src/lib/agentProfiles';

function profile(partial: Partial<AppAgentProfile> & { name: string }): AppAgentProfile {
  return {
    source: 'user',
    builtin: false,
    essential: false,
    enabled: true,
    hasOverride: false,
    ...partial,
  };
}

const FULL_BASE: AppAgentProfile = profile({
  name: 'coder',
  source: 'builtin',
  builtin: true,
  description: 'General coding agent',
  defaultModel: 'claude-sonnet-4',
  tools: ['Read', 'Write', 'Edit', 'Bash', 'Grep', 'Glob'],
  whenToUse: 'Use for general coding tasks',
  systemPromptTemplate: 'You are a coding assistant.',
  subagents: ['explore'],
  disallowedTools: ['WebFetch'],
});

// ---------------------------------------------------------------------------
// "Create from X" — self-contained profile data transformation
// ---------------------------------------------------------------------------

describe('create from an existing profile (self-contained, no extends)', () => {
  it('should prefill every editable field from the chosen base profile while keeping the name blank', () => {
    const values = formValuesFromProfile(FULL_BASE);
    expect(values.name).toBe('');
    expect(values.description).toBe('General coding agent');
    expect(values.defaultModel).toBe('claude-sonnet-4');
    expect(values.tools).toBe('Read, Write, Edit, Bash, Grep, Glob');
    expect(values.whenToUse).toBe('Use for general coding tasks');
    expect(values.systemPromptTemplate).toBe('You are a coding assistant.');
    expect(values.subagents).toEqual(['explore']);
    expect(values.disallowedTools).toEqual(['WebFetch']);
  });

  it('should build a self-contained create payload without any extends reference', () => {
    const payload = buildCreateProfilePayload({ ...formValuesFromProfile(FULL_BASE), name: 'my-coder' });
    expect(payload).toEqual({
      name: 'my-coder',
      description: 'General coding agent',
      defaultModel: 'claude-sonnet-4',
      tools: ['Read', 'Write', 'Edit', 'Bash', 'Grep', 'Glob'],
      whenToUse: 'Use for general coding tasks',
      systemPromptTemplate: 'You are a coding assistant.',
      subagents: ['explore'],
      disallowedTools: ['WebFetch'],
    });
    expect(payload).not.toHaveProperty('extends');
  });

  it('should stay self-contained when the base profile itself carries a legacy extends field', () => {
    // Legacy wire payloads may still carry `extends`; the create payload
    // built from them must stay self-contained.
    const legacyBase = Object.assign({}, FULL_BASE, { extends: 'coder' }) as AppAgentProfile;
    const payload = buildCreateProfilePayload({ ...formValuesFromProfile(legacyBase), name: 'my-specialist' });
    expect(payload).not.toHaveProperty('extends');
    // Fields are still prefilled from the base
    expect(payload.tools).toEqual(['Read', 'Write', 'Edit', 'Bash', 'Grep', 'Glob']);
  });

  it('should omit empty optional fields, keeping the list fields as [], when the base has no values', () => {
    const minimal = profile({ name: 'agent', source: 'builtin', builtin: true, essential: true });
    const values = formValuesFromProfile(minimal);
    expect(values.description).toBe('');
    expect(values.defaultModel).toBe('');
    expect(values.tools).toBe('');
    expect(values.whenToUse).toBe('');
    expect(values.systemPromptTemplate).toBe('');
    expect(values.subagents).toEqual([]);
    expect(values.disallowedTools).toEqual([]);

    const payload = buildCreateProfilePayload({ ...values, name: 'my-agent' });
    expect(payload.name).toBe('my-agent');
    expect(payload.description).toBeUndefined();
    expect(payload.defaultModel).toBeUndefined();
    expect(payload.tools).toEqual([]);
    expect(payload.whenToUse).toBeUndefined();
    expect(payload.systemPromptTemplate).toBeUndefined();
    expect(payload.subagents).toEqual([]);
    expect(payload.disallowedTools).toEqual([]);
  });

  it('should not copy the prefilled arrays into the payload by reference', () => {
    const values = formValuesFromProfile(FULL_BASE);
    const payload = buildCreateProfilePayload({ ...values, name: 'copy' });
    expect(payload.subagents).not.toBe(values.subagents);
    expect(payload.disallowedTools).not.toBe(values.disallowedTools);
  });
});

// ---------------------------------------------------------------------------
// Profile save — tools / denylist / subagents clear semantics
// ---------------------------------------------------------------------------

describe('profile save payload (empty lists persist, text fields trim)', () => {
  it('should send the parsed tool list when tools are set', () => {
    expect(parseToolList('Read, Write , Bash')).toEqual(['Read', 'Write', 'Bash']);
  });

  it('should send an empty tools array when the user cleared every tool tag', () => {
    // Clearing the ToolTagEditor leaves the form tools field empty — the
    // payload must carry [] so the server-side merge clears the persisted
    // list instead of treating the field as omitted.
    expect(parseToolList('')).toEqual([]);
    expect(parseToolList('   ')).toEqual([]);
    expect(buildUpdateProfilePayload({ ...emptyFormValues(), tools: '   ' }).tools).toEqual([]);
  });

  it('should always carry subagents and the denylist as arrays in the update payload', () => {
    const payload = buildUpdateProfilePayload({
      name: 'ignored',
      description: 'd',
      defaultModel: '',
      tools: 'Read, Grep',
      whenToUse: '',
      systemPromptTemplate: '',
      subagents: ['coder'],
      disallowedTools: ['Bash', 'Write'],
    });
    expect(payload.tools).toEqual(['Read', 'Grep']);
    expect(payload.subagents).toEqual(['coder']);
    expect(payload.disallowedTools).toEqual(['Bash', 'Write']);
    expect(payload).not.toHaveProperty('name');
  });

  it('should keep an empty denylist and empty subagents as [] in the update payload', () => {
    // The form is populated from the existing profile on every edit, so an
    // empty denylist means "no disallowed tools" — the payload must carry []
    // so the server-side merge clears the persisted frontmatter key instead
    // of preserving the old list (same defect class as the tools/subagents
    // fix rounds).
    const payload = buildUpdateProfilePayload({ ...emptyFormValues(), tools: 'Read' });
    expect(payload.disallowedTools).toEqual([]);
    expect('disallowedTools' in payload).toBe(true);
    expect(payload.subagents).toEqual([]);
    expect(payload.tools).toEqual(['Read']);
  });

  it('should trim whitespace on optional text fields when assembling the payload', () => {
    const payload = buildCreateProfilePayload({
      name: '  my-agent  ',
      description: '  desc  ',
      defaultModel: ' model ',
      tools: ' Read, Write , ',
      whenToUse: ' when ',
      systemPromptTemplate: ' prompt ',
      subagents: [],
      disallowedTools: [],
    });
    expect(payload.name).toBe('my-agent');
    expect(payload.description).toBe('desc');
    expect(payload.defaultModel).toBe('model');
    expect(payload.tools).toEqual(['Read', 'Write']);
    expect(payload.whenToUse).toBe('when');
    expect(payload.systemPromptTemplate).toBe('prompt');
  });
});

// ---------------------------------------------------------------------------
// Subagent candidate options (US-A4-3)
// ---------------------------------------------------------------------------

describe('subagent candidate options (enabled profiles only)', () => {
  const enabledCoder = profile({ name: 'coder' });
  const enabledReviewer = profile({ name: 'reviewer' });
  const disabledExplore = profile({ name: 'explore', enabled: false });

  it('should offer only enabled profiles as subagent candidates', () => {
    const names = enabledSubagentOptions([enabledCoder, enabledReviewer, disabledExplore], null).map((p) => p.name);
    expect(names).toEqual(['coder', 'reviewer']);
  });

  it('should exclude the profile currently being edited from the candidates', () => {
    const names = enabledSubagentOptions([enabledCoder, enabledReviewer, disabledExplore], 'reviewer').map((p) => p.name);
    expect(names).toEqual(['coder']);
  });

  it('should show an empty candidate list when every other profile is disabled', () => {
    expect(enabledSubagentOptions([disabledExplore], null)).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// External-change conflict resolution (US-A4-4)
// ---------------------------------------------------------------------------

describe('external-change conflict resolution (three-way picker)', () => {
  const onDisk: AppAgentProfile = profile({
    name: 'coder',
    source: 'builtin',
    builtin: true,
    description: 'General coding agent',
    defaultModel: 'claude-sonnet-4',
    tools: ['Read', 'Write'],
    whenToUse: 'Use for coding',
    systemPromptTemplate: 'You are a coding assistant.',
    subagents: ['explore'],
    disallowedTools: ['WebFetch'],
  });

  const onDiskChanged: AppAgentProfile = profile({
    name: 'coder',
    source: 'builtin',
    builtin: true,
    description: 'Edited by another process',
    defaultModel: 'claude-sonnet-4',
    tools: ['Read', 'Write', 'Grep'],
    whenToUse: 'Use for coding',
    systemPromptTemplate: 'You are a coding assistant.',
    subagents: ['explore'],
    disallowedTools: ['WebFetch', 'Bash'],
  });

  /** One divergent field at a time, file side. */
  function withExternalChange(field: ConflictField): AppAgentProfile {
    const changed: AppAgentProfile = { ...onDisk };
    switch (field) {
      case 'description': changed.description = `file ${field}`; break;
      case 'defaultModel': changed.defaultModel = `file ${field}`; break;
      case 'tools': changed.tools = [...(changed.tools ?? []), 'FileTool']; break;
      case 'whenToUse': changed.whenToUse = `file ${field}`; break;
      case 'systemPromptTemplate': changed.systemPromptTemplate = `file ${field}`; break;
      case 'subagents': changed.subagents = [...(changed.subagents ?? []), 'file-agent']; break;
      case 'disallowedTools': changed.disallowedTools = [...(changed.disallowedTools ?? []), 'FileTool']; break;
    }
    return changed;
  }

  /** One divergent field at a time, local side. */
  function withLocalChange(values: ProfileFormValues, field: ConflictField): ProfileFormValues {
    const next = {
      ...values,
      subagents: [...values.subagents],
      disallowedTools: [...values.disallowedTools],
    };
    switch (field) {
      case 'description': next.description = `local ${field}`; break;
      case 'defaultModel': next.defaultModel = `local ${field}`; break;
      case 'tools': next.tools = `local ${field}`; break;
      case 'whenToUse': next.whenToUse = `local ${field}`; break;
      case 'systemPromptTemplate': next.systemPromptTemplate = `local ${field}`; break;
      case 'subagents': next.subagents = [...next.subagents, 'extra-agent']; break;
      case 'disallowedTools': next.disallowedTools = [...next.disallowedTools, 'ExtraTool']; break;
    }
    return next;
  }

  /** The changed file-side textual value for `field` (see the helpers above). */
  function fileText(field: ConflictField): string {
    if (field === 'tools') return 'Read, Write, FileTool';
    if (field === 'subagents') return 'explore, file-agent';
    if (field === 'disallowedTools') return 'WebFetch, FileTool';
    return `file ${field}`;
  }

  /** The changed local textual value for `field` (see the helpers above). */
  function mineText(field: ConflictField): string {
    if (field === 'subagents') return 'explore, extra-agent';
    if (field === 'disallowedTools') return 'WebFetch, ExtraTool';
    return `local ${field}`;
  }

  it('should refresh silently and not intercept the save when the form has no unsaved edits', () => {
    // With no unsaved edits the poll applies the refresh without holding the
    // save, even when the on-disk profile differs from what the form shows.
    expect(buildConflictRows(onDiskChanged, formValuesFromProfile(onDiskChanged))).toEqual([]);
    expect(shouldInterceptSave(false, false, onDiskChanged)).toBe(false);
    expect(shouldInterceptSave(false, false, undefined)).toBe(false);
  });

  it('should hold back the save when the on-disk profile changed while the user has unsaved edits', () => {
    const edited = withLocalChange(formValuesFromProfile(onDisk), 'tools');
    expect(shouldInterceptSave(false, true, onDiskChanged)).toBe(true);
    expect(buildConflictRows(onDiskChanged, edited).length).toBeGreaterThan(0);
  });

  it('should never intercept the save while creating a new profile', () => {
    expect(shouldInterceptSave(true, true, onDiskChanged)).toBe(false);
  });

  it('should not intercept the save when there is no external profile to compare against', () => {
    expect(shouldInterceptSave(false, true, undefined)).toBe(false);
  });

  it('should flag only the fields whose file and local values diverged', () => {
    for (const field of CONFLICT_FIELDS) {
      // Neither side changed — not flagged.
      expect(buildConflictRows(onDisk, formValuesFromProfile(onDisk)).find((r) => r.field === field)).toBeUndefined();

      // Only the local side changed — flagged with the file value preserved.
      const localRow = buildConflictRows(onDisk, withLocalChange(formValuesFromProfile(onDisk), field))
        .find((r) => r.field === field);
      expect(localRow?.fileValue).toBe(externalFieldText(onDisk, field));
      expect(localRow?.mineValue).toBe(mineText(field));

      // Only the file side changed — flagged with the local value preserved.
      const fileRow = buildConflictRows(withExternalChange(field), formValuesFromProfile(onDisk))
        .find((r) => r.field === field);
      expect(fileRow?.fileValue).toBe(fileText(field));
      expect(fileRow?.mineValue).toBe(formFieldText(formValuesFromProfile(onDisk), field));

      // Both sides changed — still flagged.
      const bothRow = buildConflictRows(
        withExternalChange(field),
        withLocalChange(formValuesFromProfile(onDisk), field),
      ).find((r) => r.field === field);
      expect(bothRow).toBeDefined();
      expect(bothRow?.useFile).toBe(false);
    }
  });

  it('should adopt the on-disk version wholesale when choosing use-file', () => {
    // "Use file" repopulates the form from the external profile.
    const adopted = formValuesFromProfile(onDiskChanged);
    expect(adopted.description).toBe('Edited by another process');
    expect(adopted.tools).toBe('Read, Write, Grep');
    expect(adopted.subagents).toEqual(['explore']);
    expect(adopted.disallowedTools).toEqual(['WebFetch', 'Bash']);
  });

  it('should keep the local values when saving over the file version', () => {
    const local = withLocalChange(formValuesFromProfile(onDisk), 'description');
    const payload = buildUpdateProfilePayload(local);
    expect(payload.description).toBe('local description');
    expect(payload.tools).toEqual(['Read', 'Write']);
  });

  it('should apply only the fields picked per row when merging choices', () => {
    const local = withLocalChange(formValuesFromProfile(onDisk), 'description');
    const rows = buildConflictRows(onDiskChanged, local);
    const merged = {
      ...local,
      subagents: [...local.subagents],
      disallowedTools: [...local.disallowedTools],
    };
    for (const row of rows) {
      if (row.field === 'description' || row.field === 'disallowedTools') {
        applyFileValue(merged, onDiskChanged, row.field);
      }
    }
    expect(merged.description).toBe('Edited by another process');
    expect(merged.disallowedTools).toEqual(['WebFetch', 'Bash']);
    // Untouched rows keep the local values
    expect(merged.tools).toBe('Read, Write');
    const payload = buildUpdateProfilePayload(merged);
    expect(payload.description).toBe('Edited by another process');
    expect(payload.disallowedTools).toEqual(['WebFetch', 'Bash']);
    expect(payload.tools).toEqual(['Read', 'Write']);
  });
});

// ---------------------------------------------------------------------------
// D16 edit-face gating — essential / builtin / custom
// ---------------------------------------------------------------------------

describe('edit-face gating (essential / builtin / custom)', () => {
  const essential = profile({ name: 'agent', source: 'builtin', builtin: true, essential: true });
  const essentialOverridden = profile({
    name: 'agent', source: 'builtin', builtin: true, essential: true, hasOverride: true,
  });
  const builtinOverridden = profile({ name: 'coder', source: 'builtin', builtin: true, hasOverride: true });
  const builtinVanilla = profile({ name: 'explore', source: 'builtin', builtin: true });
  const custom = profile({ name: 'reviewer' });

  const MATRIX: Array<{
    label: string;
    creating: boolean;
    profile: AppAgentProfile | undefined;
    face: EditingKind;
    deleteEnabled: boolean;
    deleteLocked: boolean;
    deleteHidden: boolean;
    toggleAllowed?: boolean;
    resetShown: boolean;
  }> = [
    {
      label: 'custom profile',
      creating: false, profile: custom, face: 'custom',
      deleteEnabled: true, deleteLocked: false, deleteHidden: false,
      toggleAllowed: true, resetShown: false,
    },
    {
      label: 'builtin profile with user override',
      creating: false, profile: builtinOverridden, face: 'builtin',
      deleteEnabled: false, deleteLocked: false, deleteHidden: true,
      toggleAllowed: true, resetShown: true,
    },
    {
      label: 'builtin profile without override',
      creating: false, profile: builtinVanilla, face: 'builtin',
      deleteEnabled: false, deleteLocked: false, deleteHidden: true,
      toggleAllowed: true, resetShown: false,
    },
    {
      label: 'essential profile',
      creating: false, profile: essential, face: 'essential',
      deleteEnabled: false, deleteLocked: true, deleteHidden: false,
      toggleAllowed: false, resetShown: false,
    },
    {
      label: 'essential profile with user override',
      creating: false, profile: essentialOverridden, face: 'essential',
      deleteEnabled: false, deleteLocked: true, deleteHidden: false,
      toggleAllowed: false, resetShown: true,
    },
    {
      label: 'no profile selected',
      creating: false, profile: undefined, face: 'none',
      deleteEnabled: false, deleteLocked: false, deleteHidden: true,
      resetShown: false,
    },
    {
      label: 'create mode',
      creating: true, profile: custom, face: 'none',
      deleteEnabled: false, deleteLocked: false, deleteHidden: true,
      toggleAllowed: true, resetShown: false,
    },
  ];

  it('should derive delete, disable, and reset availability from the selected profile kind', () => {
    for (const row of MATRIX) {
      const face = editingKind(row.creating, row.profile);
      expect(face).toBe(row.face);
      // The delete face: enabled for custom, locked (disabled button + hint)
      // for essential, absent for builtin/none.
      expect(face === 'custom').toBe(row.deleteEnabled);
      expect(face === 'essential').toBe(row.deleteLocked);
      expect(face === 'builtin' || face === 'none').toBe(row.deleteHidden);
      if (row.toggleAllowed !== undefined) {
        expect(canToggleProfile(row.profile!)).toBe(row.toggleAllowed);
      }
      expect(canResetToDefault(row.profile)).toBe(row.resetShown);
    }
  });

  it('should keep the essential profile editable while locking only the destructive actions', () => {
    // D16: the essential content stays editable (every field, saved as a user
    // override while the built-in registration stays the fallback identity) —
    // only delete and disable remain locked.
    expect(editingKind(false, essential)).not.toBe('none');
    expect(editingKind(false, essential)).toBe('essential');
    expect(canToggleProfile(essential)).toBe(false);
    // Plus override: reset-to-default becomes available, delete stays locked.
    expect(canResetToDefault(essentialOverridden)).toBe(true);
    expect(editingKind(false, essentialOverridden)).toBe('essential');
    expect(canToggleProfile(essentialOverridden)).toBe(false);
    expect(editingKind(false, essentialOverridden) === 'custom').toBe(false);
  });
});

// ---------------------------------------------------------------------------
// Create/save form validation (US-A3-2)
// ---------------------------------------------------------------------------

describe('form validation (US-A3-2)', () => {
  const validForm = (overrides: Partial<ProfileFormValues> = {}): ProfileFormValues => ({
    ...emptyFormValues(),
    name: 'code-reviewer',
    description: 'Code review specialist',
    ...overrides,
  });

  it('given a blank name, when creating, then it flags the name field as required', () => {
    expect(validateAgentProfileForm(validForm({ name: '   ' }), { creating: true })).toEqual({
      field: 'name',
      code: 'nameRequired',
    });
  });

  it('given a non-kebab-case name, when creating, then it flags the name pattern', () => {
    for (const name of ['Code-Reviewer', 'code_reviewer', '-code', 'code-', 'code--reviewer', '代码审查']) {
      expect(validateAgentProfileForm(validForm({ name }), { creating: true })).toEqual({
        field: 'name',
        code: 'namePattern',
      });
    }
  });

  it('given a name longer than the engine cap, when creating, then it flags the name length', () => {
    const tooLong = 'a'.repeat(AGENT_NAME_MAX_LENGTH + 1);
    expect(tooLong.length).toBeGreaterThan(AGENT_NAME_MAX_LENGTH);
    expect(validateAgentProfileForm(validForm({ name: tooLong }), { creating: true })).toEqual({
      field: 'name',
      code: 'nameTooLong',
    });
    // The cap itself stays valid.
    expect(validateAgentProfileForm(validForm({ name: 'a'.repeat(AGENT_NAME_MAX_LENGTH) }), { creating: true })).toBeNull();
  });

  it('given a valid kebab-case name but an empty description, when creating, then it flags the description field', () => {
    expect(validateAgentProfileForm(validForm({ description: '  ' }), { creating: true })).toEqual({
      field: 'description',
      code: 'descriptionRequired',
    });
  });

  it('given a valid name and non-empty description, when creating, then the form passes validation', () => {
    // Names are trimmed before validation, so surrounding whitespace is fine.
    expect(validateAgentProfileForm(validForm({ name: '  code-reviewer  ' }), { creating: true })).toBeNull();
  });

  it('given an empty description on an existing profile, when saving an edit, then it flags the description field', () => {
    // Saving an empty description would write a file the engine can no
    // longer parse — the guard applies to updates too, not just create.
    expect(validateAgentProfileForm(validForm({ description: '' }), { creating: false })).toEqual({
      field: 'description',
      code: 'descriptionRequired',
    });
  });

  it('given an invalid name on an existing profile, when saving an edit, then the name is not re-validated', () => {
    // The name is the route key on update and is not editable — only the
    // description guard applies.
    expect(validateAgentProfileForm(validForm({ name: 'Not Kebab' }), { creating: false })).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// Override badge (US-A6-2) — built-in covered by a user file reads "overridden"
// ---------------------------------------------------------------------------

describe('override badge (US-A6-2)', () => {
  it('given a built-in profile with a user override, when the row renders, then it is labeled overridden', () => {
    expect(overrideBadgeKind(profile({ name: 'explore', source: 'builtin', builtin: true, hasOverride: true }))).toBe('overridden');
  });

  it('given a built-in profile without an override, when the row renders, then it shows no custom badge', () => {
    expect(overrideBadgeKind(profile({ name: 'explore', source: 'builtin', builtin: true }))).toBeNull();
  });

  it('given a custom profile, when the row renders, then it is labeled custom either way', () => {
    expect(overrideBadgeKind(profile({ name: 'reviewer' }))).toBe('custom');
    // A non-built-in profile keeps the custom label even when it carries a
    // same-name override marker (parity with the previous two-badge logic).
    expect(overrideBadgeKind(profile({ name: 'reviewer', hasOverride: true }))).toBe('custom');
  });

  it('given an essential built-in with a user override, when the row renders, then it is labeled overridden', () => {
    expect(overrideBadgeKind(profile({ name: 'agent', source: 'builtin', builtin: true, essential: true, hasOverride: true }))).toBe('overridden');
  });
});