// apps/mirri-web/src/lib/agentProfiles.ts
// Pure form/panel logic for `AgentProfilesPanel.vue`, extracted so it can be
// unit-tested without a component harness: create/update payload assembly,
// "start from" prefill, subagent candidate options, the external-change
// three-way conflict detection (US-A4-4), and the D16 edit-face gating
// (essential / builtin / custom). All functions are DOM-free and take
// explicit data (profiles, form values) — none depend on component instance.

import type {
  AppAgentProfile,
  AppAgentProfileInput,
  AppAgentProfileUpdate,
} from '../api/types';

/** The editable form fields as held by the panel (tools as one comma-joined string). */
export interface ProfileFormValues {
  name: string;
  description: string;
  defaultModel: string;
  tools: string;
  whenToUse: string;
  systemPromptTemplate: string;
  subagents: string[];
  disallowedTools: string[];
}

/** A blank form — used when starting a create with no "start from" base. */
export function emptyFormValues(): ProfileFormValues {
  return {
    name: '',
    description: '',
    defaultModel: '',
    tools: '',
    whenToUse: '',
    systemPromptTemplate: '',
    subagents: [],
    disallowedTools: [],
  };
}

/** Fields prefilled from an existing profile ("start from" / edit populate).
 * The name is passed explicitly: the "start from" affordance leaves it blank. */
export function formValuesFromProfile(profile: AppAgentProfile, name = ''): ProfileFormValues {
  return {
    name,
    description: profile.description ?? '',
    defaultModel: profile.defaultModel ?? '',
    tools: (profile.tools ?? []).join(', '),
    whenToUse: profile.whenToUse ?? '',
    systemPromptTemplate: profile.systemPromptTemplate ?? '',
    subagents: [...(profile.subagents ?? [])],
    disallowedTools: [...(profile.disallowedTools ?? [])],
  };
}

/** Parse the comma-separated tools field. An empty/blank string means "no
 * tools" and must serialize as `[]` (never `undefined`), otherwise the
 * server-side PUT merge would silently preserve the old list. */
export function parseToolList(formTools: string): string[] {
  return formTools.trim()
    ? formTools.split(',').map((s) => s.trim()).filter(Boolean)
    : [];
}

/** The editable fields shared by the create and update payloads. */
function payloadFields(form: ProfileFormValues): Omit<AppAgentProfileInput, 'name'> {
  return {
    description: form.description.trim() || undefined,
    defaultModel: form.defaultModel.trim() || undefined,
    tools: parseToolList(form.tools),
    whenToUse: form.whenToUse.trim() || undefined,
    systemPromptTemplate: form.systemPromptTemplate.trim() || undefined,
    // Always send the arrays — `undefined` would make the kap-server PUT
    // merge preserve the existing lists, so clearing every entry would
    // silently no-op. `[]` clears the persisted frontmatter key.
    subagents: [...form.subagents],
    disallowedTools: [...form.disallowedTools],
  };
}

/** Self-contained create payload — the "start from" selector prefills the
 * form but never becomes an `extends` reference. */
export function buildCreateProfilePayload(form: ProfileFormValues): AppAgentProfileInput {
  return {
    name: form.name.trim(),
    ...payloadFields(form),
  };
}

/** Update payload for an existing profile (no name — it is the route key). */
export function buildUpdateProfilePayload(form: ProfileFormValues): AppAgentProfileUpdate {
  return { ...payloadFields(form) };
}

/** Subagent candidates — currently enabled profiles, excluding the one being edited. */
export function enabledSubagentOptions(
  profiles: AppAgentProfile[],
  selectedName: string | null,
): AppAgentProfile[] {
  return profiles.filter((p) => p.enabled && p.name !== selectedName);
}

// ---------------------------------------------------------------------------
// External-change conflict detection (US-A4-4)
// ---------------------------------------------------------------------------

/** Fields compared during the three-way conflict resolution, in display order. */
export const CONFLICT_FIELDS = [
  'description',
  'defaultModel',
  'tools',
  'whenToUse',
  'systemPromptTemplate',
  'subagents',
  'disallowedTools',
] as const;

export type ConflictField = (typeof CONFLICT_FIELDS)[number];

/** One per-field diff row shown in the conflict picker. */
export interface ConflictRow {
  field: ConflictField;
  fileValue: string;
  mineValue: string;
  useFile: boolean;
}

/** The form's textual rendering of a field (arrays joined for display). */
export function formFieldText(form: ProfileFormValues, field: ConflictField): string {
  switch (field) {
    case 'description': return form.description;
    case 'defaultModel': return form.defaultModel;
    case 'tools': return form.tools;
    case 'whenToUse': return form.whenToUse;
    case 'systemPromptTemplate': return form.systemPromptTemplate;
    case 'subagents': return form.subagents.join(', ');
    case 'disallowedTools': return form.disallowedTools.join(', ');
  }
}

/** The on-disk profile's textual rendering of a field. */
export function externalFieldText(profile: AppAgentProfile, field: ConflictField): string {
  switch (field) {
    case 'description': return profile.description ?? '';
    case 'defaultModel': return profile.defaultModel ?? '';
    case 'tools': return (profile.tools ?? []).join(', ');
    case 'whenToUse': return profile.whenToUse ?? '';
    case 'systemPromptTemplate': return profile.systemPromptTemplate ?? '';
    case 'subagents': return (profile.subagents ?? []).join(', ');
    case 'disallowedTools': return (profile.disallowedTools ?? []).join(', ');
  }
}

/** Per-field diff between the on-disk profile and the form; empty when the
 * local values still match the file (nothing to resolve). */
export function buildConflictRows(profile: AppAgentProfile, form: ProfileFormValues): ConflictRow[] {
  const rows: ConflictRow[] = [];
  for (const field of CONFLICT_FIELDS) {
    const fileValue = externalFieldText(profile, field);
    const mineValue = formFieldText(form, field);
    if (fileValue !== mineValue) {
      rows.push({ field, fileValue, mineValue, useFile: false });
    }
  }
  return rows;
}

/** Overwrite one form field from the on-disk profile. Mutates the passed
 * form values — used while applying the per-field "use file" picks. */
export function applyFileValue(form: ProfileFormValues, profile: AppAgentProfile, field: ConflictField): void {
  switch (field) {
    case 'description': form.description = profile.description ?? ''; break;
    case 'defaultModel': form.defaultModel = profile.defaultModel ?? ''; break;
    case 'tools': form.tools = (profile.tools ?? []).join(', '); break;
    case 'whenToUse': form.whenToUse = profile.whenToUse ?? ''; break;
    case 'systemPromptTemplate': form.systemPromptTemplate = profile.systemPromptTemplate ?? ''; break;
    case 'subagents': form.subagents = [...(profile.subagents ?? [])]; break;
    case 'disallowedTools': form.disallowedTools = [...(profile.disallowedTools ?? [])]; break;
  }
}

/** Whether a save must be held back for three-way conflict resolution: an
 * existing profile with unsaved edits whose on-disk version changed. */
export function shouldInterceptSave(
  creating: boolean,
  externallyUpdated: boolean,
  externalProfile: AppAgentProfile | undefined,
): boolean {
  return !creating && externallyUpdated && externalProfile !== undefined;
}

// ---------------------------------------------------------------------------
// Edit-face gating (D16): which actions render for which profile kind
// ---------------------------------------------------------------------------

/** Which edit face the detail pane shows: nothing (list/create), a custom
 * profile, a built-in profile, or the essential built-in (`agent`). The
 * essential face keeps the form editable — only the destructive operations
 * (delete/disable) stay locked. */
export type EditingKind = 'none' | 'custom' | 'builtin' | 'essential';

export function editingKind(creating: boolean, profile: AppAgentProfile | undefined): EditingKind {
  if (creating || profile === undefined) return 'none';
  if (profile.essential) return 'essential';
  if (profile.builtin) return 'builtin';
  return 'custom';
}

/** Whether the enable/disable toggle is usable — essential profiles stay locked (D16). */
export function canToggleProfile(profile: AppAgentProfile): boolean {
  return !profile.essential;
}

/** Whether "reset to default" is available — built-in profiles with a user override. */
export function canResetToDefault(profile: AppAgentProfile | undefined): boolean {
  return profile !== undefined && profile.builtin && profile.hasOverride;
}

// ---------------------------------------------------------------------------
// Create/save form validation (US-A3-2)
// ---------------------------------------------------------------------------

/** Backend cap for agent names — TS `AGENT_NAME_MAX_LENGTH`, mirrored by the
 * rust `.md` parser. Names become file names on write, so the form rejects
 * over-long names before the request. */
export const AGENT_NAME_MAX_LENGTH = 64;

/** The kebab-case grammar both engines enforce (TS `AGENT_NAME_PATTERN`,
 * rust `is_kebab_case`): lowercase alphanumeric segments separated by single
 * hyphens (spec US-A3: 小写字母或数字与连字符). */
const KEBAB_CASE_PATTERN = /^[a-z0-9]+(?:-[a-z0-9]+)*$/;

export type AgentProfileFormErrorCode =
  | 'nameRequired'
  | 'namePattern'
  | 'nameTooLong'
  | 'descriptionRequired';

/** Structured validation failure — the UI translates `code` per field. */
export interface AgentProfileFormError {
  field: 'name' | 'description';
  code: AgentProfileFormErrorCode;
}

/** Front-end form validation (spec US-A3-2): an invalid name or an empty
 * description is intercepted before any request, naming the offending field.
 * Name rules apply only while creating — the name is the route key on update
 * and is not editable. The description requirement applies to both faces:
 * saving an empty description would write a file the engine can no longer
 * parse. Returns null when the form is valid. */
export function validateAgentProfileForm(
  form: ProfileFormValues,
  options: { creating: boolean },
): AgentProfileFormError | null {
  if (options.creating) {
    const name = form.name.trim();
    if (!name) return { field: 'name', code: 'nameRequired' };
    if (!KEBAB_CASE_PATTERN.test(name)) return { field: 'name', code: 'namePattern' };
    if (name.length > AGENT_NAME_MAX_LENGTH) return { field: 'name', code: 'nameTooLong' };
  }
  if (!form.description.trim()) return { field: 'description', code: 'descriptionRequired' };
  return null;
}

// ---------------------------------------------------------------------------
// List-row override badge (US-A6-2)
// ---------------------------------------------------------------------------

/** Which "custom/override" badge a list row shows: a built-in profile covered
 * by a user file is labeled as overridden (US-A6-2), every non-built-in
 * profile is labeled custom, and a vanilla built-in profile gets no badge. */
export type AgentProfileOverrideBadge = 'overridden' | 'custom';

export function overrideBadgeKind(profile: AppAgentProfile): AgentProfileOverrideBadge | null {
  if (profile.builtin && profile.hasOverride) return 'overridden';
  if (!profile.builtin) return 'custom';
  return null;
}
