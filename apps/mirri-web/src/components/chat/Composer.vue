<!-- apps/mirri-web/src/components/chat/Composer.vue -->
<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import { useEditor, EditorContent } from '@tiptap/vue-3';
import StarterKit from '@tiptap/starter-kit';
import SlashMenu from './SlashMenu.vue';
import MentionMenu from './MentionMenu.vue';
import Banner from '../ui/Banner.vue';
import Button from '../ui/Button.vue';
import { buildSlashItems, parseSlash, SKILL_COMMAND_PREFIX } from '../../lib/slashCommands';
import {
  findInvalidSkillMentionAttrs,
  hasInlineReferences,
  normalizeInlineReferencesForSubmit,
} from '../../lib/inlineReferences';
import {
  backfillSkillMentionTargets,
  buildSkillMentionDocJson,
  collectSkillMentionAttrs,
  textOffsetToPos,
  type SkillMentionAttrs,
} from '../../lib/skillMention';
import { SkillMention } from '../../lib/skillMentionNode';
import { serializeDocJsonWithFileMentions, collectFileMentionAttrs, toPromptReferences, type PromptFileReference } from '../../lib/fileMention';
import { FileMention } from '../../lib/fileMentionNode';
import type { SearchFilesResult } from '../../types';
import type { ActivationBadges, ConversationStatus, PermissionMode, QueuedPromptView } from '../../types';
import type { AppModel, AppSkill, ThinkingLevel } from '../../api/types';
import {
  commitLevel,
  effectiveThinkingLevel,
  effortLabel,
  isThinkingOn,
  modelThinkingAvailability,
  segmentsFor,
} from '../../lib/modelThinking';
import { useInputHistory } from '../../composables/useInputHistory';
import { useSlashMenu } from '../../composables/useSlashMenu';
import { useMentionMenu } from '../../composables/useMentionMenu';
import { useSkillTriggerMenu } from '../../composables/useSkillTriggerMenu';
import { useComposerDraft } from '../../composables/useComposerDraft';
import type { TextSource } from '../../composables/useTextSource';
import { useAttachmentUpload, type Attachment } from '../../composables/useAttachmentUpload';
import { openFileAttachment } from '../../lib/openFileAttachment';
import { formatTokens } from '../../lib/formatTokens';
import type { PromptAttachment } from '../../composables/useMirriWebClient';
import Spinner from '../ui/Spinner.vue';
import IconButton from '../ui/IconButton.vue';
import Icon from '../ui/Icon.vue';
import ContextRing from '../ui/ContextRing.vue';
import Tooltip from '../ui/Tooltip.vue';
import AttachmentChip from './AttachmentChip.vue';

// ---------------------------------------------------------------------------
// Props & emits
// ---------------------------------------------------------------------------

const props = withDefaults(defineProps<{
  running?: boolean;
  /** True while the empty-composer first prompt is being created + submitted.
   *  Disables the textarea and swaps the send button for a spinner. */
  starting?: boolean;
  /** Active session id — scopes the persisted unsent draft (per session). */
  sessionId?: string;
  queued?: QueuedPromptView[];
  searchFiles?: (q: string) => Promise<SearchFilesResult>;
  /** E9: whether the backend understands structured file references (v3).
   *  False = the @-menu degrades to plain-text path insertion. */
  supportsFileReferences?: boolean;
  /** If undefined, attach button is hidden and paste/drag are no-ops. */
  uploadImage?: (file: Blob, name?: string) => Promise<{ fileId: string; name: string; mediaType: string } | null>;
  /** Status data (model, context, permission) — drives the bottom toolbar. */
  status?: ConversationStatus;
  thinking?: ThinkingLevel;
  planMode?: boolean;
  swarmMode?: boolean;
  collaborationMode?: boolean;
  /** Whether the current backend has a collaboration engine effect (B10
   *  D-24) — v1/v2 hide the collaboration pill + menu row entirely. */
  collaborationSupported: boolean;
  goalMode?: boolean;
  activationBadges?: ActivationBadges;
  /** Available models for the quick-switch dropdown. */
  models?: AppModel[];
  /** Starred model ids shown at the top of the quick-switch dropdown. */
  starredIds?: string[];
  /** Session skills shown in the `/` menu (after the built-in commands). */
  skills?: AppSkill[];
  /** Hide the context-usage indicator (used on the empty-session landing page). */
  hideContext?: boolean;
}>(), {
  running: false,
  starting: false,
  queued: () => [],
  searchFiles: undefined,
  supportsFileReferences: false,
  uploadImage: undefined,
  models: () => [],
  starredIds: () => [],
  skills: () => [],
});

const placeholder = computed(() =>
  props.starting
    ? t('composer.starting')
    : props.running
      ? t('composer.placeholderRunning')
      : props.goalMode
        ? t('status.goalPlaceholder')
        : t('composer.placeholder')
);

const emit = defineEmits<{
  submit: [payload: { text: string; attachments: PromptAttachment[] }];
  /** Steer the composer text (+ any queued prompts, merged by the parent)
      into the RUNNING turn — TUI ctrl+s. */
  steer: [payload: { text: string; attachments: PromptAttachment[] }];
  command: [cmd: string];
  interrupt: [];
  setPermission: [mode: PermissionMode];
  setThinking: [level: ThinkingLevel];
  togglePlan: [];
  toggleSwarm: [];
  toggleCollaboration: [];
  toggleGoal: [];
  openBtw: [];
  createGoal: [objective: string];
  controlGoal: [action: 'pause' | 'resume' | 'cancel'];
  focusGoal: [];
  focusSwarm: [];
  compact: [];
  pickModel: [];
  selectModel: [modelId: string];
  /** E9 uiux §4: a file chip was clicked — open the preview (folder chips
   *  have no click behavior; the composer filters before emitting). */
  openFile: [target: { path: string }];
}>();

const { t } = useI18n();

// ---------------------------------------------------------------------------
// TipTap editor + per-session draft persistence — see useComposerDraft.
// The editor hosts the contenteditable; the draft string is persisted (the
// serialized wire form), parsed back into the doc on load/session switch.
// ---------------------------------------------------------------------------
const { text, loadForEdit, clearDraft } = useComposerDraft({
  sessionId: () => props.sessionId,
});

/** The serialized wire text (skill mentions as full `[$name](target)` links,
 *  file mentions as `@path` with folders trailing `/`). */
function serializeDoc(): string {
  const e = editor.value;
  if (!e) return '';
  return serializeDocJsonWithFileMentions(e.getJSON());
}
/** The structural chips in the doc (ADR-009): the warning path validates
 *  ONLY these — pasted text never enters the pre-submit warning. */
function collectDocChipAttrs(): SkillMentionAttrs[] {
  const e = editor.value;
  if (!e) return [];
  return collectSkillMentionAttrs(e.getJSON());
}
/** Apply a plain string to the editor (parse full links + compact tokens into
 *  mention nodes), caret at the end, focus kept. Targetless legacy chips get
 *  their targets backfilled by name against the live skill list (ADR-009 D2). */
function applyTextToEditor(value: string): void {
  const e = editor.value;
  if (!e) return;
  e.commands.setContent(backfillSkillMentionTargets(buildSkillMentionDocJson(value), props.skills));
  e.commands.focus('end');
}

/** Apply a string to the editor WITHOUT persisting (submit-clear path). */
function clearEditor(): void {
  const e = editor.value;
  if (!e) return;
  e.commands.clearContent(true);
  text.value = '';
}

// The TextSource adapter the trigger menus and history share. Mentions read
// as `〇` placeholders in the token-scan text so caret offsets stay contiguous
// (a chip is one atomic inline unit — one placeholder char per node).
// OFFSET MODEL: getText() returns plain-text offsets, but ProseMirror
// selection.from is a DOC position (block boundaries add one hidden slot:
// the first text char of a doc sits at from=1, not 0). The token scanners
// index into the returned string, so the caret must be converted:
// text offset = doc pos − (number of block-open slots before it), which for
// this composer (single paragraph flow) equals the position of the caret
// within the textBetween projection — computed by measuring the projection
// up to the selection.
const textSource: TextSource = {
  getText: () => {
    const e = editor.value;
    if (!e) return '';
    const { from } = e.state.selection;
    return e.state.doc.textBetween(0, from, '\n', '〇');
  },
  getCaret: () => {
    const e = editor.value;
    if (!e) return 0;
    // Project the doc up to the caret; the projected string's length IS the
    // text offset of the caret (handles chips as single 〇 chars and block
    // boundaries as \n) — e.g. doc pos 5 for "帮我 $" yields offset 4.
    return e.state.doc.textBetween(0, e.state.selection.from, '\n', '〇').length;
  },
  replaceRange: (start, end, insert) => {
    const e = editor.value;
    if (!e) return;
    const text = e.state.doc.textBetween(0, e.state.doc.content.size, '\n', '〇');
    e.chain()
      .focus()
      .insertContentAt({ from: textOffsetToPos(text, start), to: textOffsetToPos(text, end) }, insert)
      .run();
  },
  insertSkillMention: (options) => {
    editor.value?.chain().focus().insertSkillMention(options).run();
  },
  insertFileMention: (options) => {
    editor.value?.chain().focus().insertFileMention(options).run();
  },
  hasFileMention: (path) => {
    const e = editor.value;
    if (!e) return false;
    return collectFileMentionAttrs(e.getJSON()).some((attrs) => attrs.path === path);
  },
  focus: () => {
    editor.value?.commands.focus();
  },
};

const editor = useEditor({
  content: buildSkillMentionDocJson(text.value),
  // Editor-ready leg of the one-shot chip backfill (see backfillChipsOnce):
  // when skills were already cached before mount, the watch's immediate run
  // saw no editor — this callback is the only remaining arrival signal.
  onCreate: () => {
    backfillChipsOnce(props.skills);
  },
  extensions: [
    StarterKit.configure({
      // Plain-input composer: no rich blocks, marks or links — only
      // paragraph/text/hardBreak + undo/redo history are kept.
      blockquote: false,
      bold: false,
      bulletList: false,
      code: false,
      codeBlock: false,
      dropcursor: false,
      gapcursor: false,
      heading: false,
      horizontalRule: false,
      italic: false,
      link: false,
      listItem: false,
      listKeymap: false,
      orderedList: false,
      strike: false,
      underline: false,
      trailingNode: false,
    }),
    SkillMention,
    FileMention,
  ],
  // The "starting" state locks the editor (the old textarea's :disabled).
  editable: !props.starting,
  editorProps: {
    // Class hook for the empty-state placeholder (CSS ::before shows the
    // placeholder text while the doc is empty).
    attributes: { class: 'ph' },
    handleKeyDown: (_view, event) => handleEditorKeydown(event),
    // E9 uiux §4 — clicking a file chip inside the input opens its preview.
    // The chip's renderHTML carries data-file-mention + data-kind + data-path,
    // so a DOM closest() lookup recovers the path without a node view.
    handleClick: (_view, _pos, event) => {
      const el = (event.target instanceof Element ? event.target : null)?.closest?.('[data-file-mention]');
      if (!el) return false;
      const path = el.getAttribute('data-path');
      const kind = el.getAttribute('data-kind');
      if (path && kind === 'file') {
        emit('openFile', { path });
        return true;
      }
      return false; // folder chips have no click behavior
    },
  },
});

// One-shot backfill when the skill list first arrives (ADR-009 D2): the
// initial mount cannot backfill (skills not ready); without this, a legacy
// compact-token chip survives only until the first keystroke rewrites the
// draft as plain text. BOTH arrival paths feed the same one-shot: the skills
// watch covers skills arriving after mount, and the editor's onCreate
// callback covers the reverse order (skills already cached when the editor
// mounts — the watch's immediate run happened during setup, when editor.value
// was still undefined). The editor-readiness check runs BEFORE the flag is
// set, so whichever path runs first with both ends ready performs the
// backfill exactly once.
const skillsArrived = ref(false);
function backfillChipsOnce(skills: ReadonlyArray<{ name: string; path?: string }>): void {
  if (skillsArrived.value || skills.length === 0) return;
  const e = editor.value;
  if (!e) return;
  skillsArrived.value = true;
  e.commands.setContent(backfillSkillMentionTargets(e.getJSON(), skills));
}
watch(
  () => props.skills,
  (skills) => {
    backfillChipsOnce(skills);
  },
  { immediate: true },
);

// Mirror the starting state into the editor's editability.
watch(
  () => props.starting,
  (starting) => {
    editor.value?.setEditable(!starting);
  },
);

// Keep the persisted draft in lockstep with the doc (serialized wire form).
function syncDraftFromEditor(): void {
  const value = serializeDoc();
  if (value === text.value) return;
  text.value = value;
}
// Empty-state class on the editor element (the CSS placeholder shows while
// .is-editor-empty is present). Runs on every transaction (update) — cheap.
watch(
  () => editor.value,
  (e, _old, onCleanup) => {
    if (!e) return;
    const onUpdate = (): void => {
      docVersion.value++;
      syncDraftFromEditor();
      history.resetBrowsing();
      updateSlashMenu();
      updateMentionMenu();
      updateSkillTriggerMenu();
    };
    const syncEmptyClass = (): void => {
      e.view.dom.classList.toggle('is-editor-empty', e.isEmpty);
    };
    e.on('update', onUpdate);
    e.on('transaction', syncEmptyClass);
    syncEmptyClass();
    onCleanup(() => {
      e.off('update', onUpdate);
      e.off('transaction', syncEmptyClass);
    });
  },
  { immediate: true },
);

// Session switch: the draft composable swaps `text`; apply it to the editor
// (and skip the update-triggered draft save while loading).
watch(text, (value) => {
  const e = editor.value;
  if (!e) return;
  const current = serializeDoc();
  if (current !== value) applyTextToEditor(value);
});

// ---------------------------------------------------------------------------
// Expanded editor — a taller, multi-line composing mode. While expanded, Enter
// inserts a newline instead of sending (send via the button or Cmd/Ctrl+Enter);
// it auto-collapses after a successful send. See handleEditorKeydown /
// handleSubmit.
// ---------------------------------------------------------------------------
const expanded = ref(false);
function toggleExpand(): void {
  expanded.value = !expanded.value;
  // Return focus to the editor so the user can keep typing right away;
  // otherwise focus stays on the toggle button and the next Enter would
  // activate it again instead of inserting a newline.
  void nextTick(() => {
    editor.value?.commands.focus();
  });
}

// Collapse the expanded editor after a successful send/steer. With the
// contenteditable the box height is pure CSS (min/max-height), so no re-fit
// pass is needed — the rules restyle instantly.
function collapseAndRefit(): void {
  if (!expanded.value) return;
  expanded.value = false;
}

// The expand toggle is hidden at the resting height and only appears once the
// box has grown past it (multi-line content) — keeps the empty composer
// uncluttered. While expanded it always shows so the user can collapse back.
// The docVersion ref re-evaluates this on every editor transaction; the test
// mirrors the old textarea heuristic (scrollHeight past the 36px resting
// height), so a long auto-wrapped single line counts too.
const docVersion = ref(0);
const isGrown = computed(() => {
  void docVersion.value;
  if (!editor.value || expanded.value) return false;
  const el = editor.value.view.dom;
  if (typeof getComputedStyle === 'undefined') return el.scrollHeight > 36;
  const min = Number.parseFloat(getComputedStyle(el).minHeight);
  return el.scrollHeight > (Number.isFinite(min) && min > 0 ? min : 36);
});

// The component instance is reused across session switches (it is not keyed by
// session), so reset the per-session expanded preference when the active
// session changes. Without this, expanding in one chat would leave the next
// session's draft stuck in the tall editor with Enter inserting newlines.
watch(() => props.sessionId, () => {
  expanded.value = false;
});

// ---------------------------------------------------------------------------
// Sent-message history recall (shell-style ↑/↓). See useInputHistory for the
// implementation; the composer keeps the keydown orchestration (which also
// juggles the slash and mention menus).
// ---------------------------------------------------------------------------
const history = useInputHistory({
  textSource,
  applyText: applyTextToEditor,
  getText: serializeDoc,
  sessionId: () => props.sessionId,
});

// ---------------------------------------------------------------------------
// Slash-command menu — see useSlashMenu for the implementation. The composer
// keeps the keydown orchestration (arrow keys / Enter / Escape) because it also
// juggles the mention menu and history recall.
// ---------------------------------------------------------------------------
const {
  open: slashOpen,
  items: slashItems,
  active: slashActive,
  update: updateSlashMenu,
  select: selectSlashCommand,
} = useSlashMenu({
  textSource,
  skills: () => props.skills,
  emitCommand: (cmd) => emit('command', cmd),
  historyPush: (entry) => history.push(entry),
  clearDraft,
});

// ---------------------------------------------------------------------------
// @-mention menu — see useMentionMenu for the implementation. The composer
// keeps the keydown orchestration because it also juggles the slash menu and
// history recall.
// ---------------------------------------------------------------------------
const {
  open: mentionOpen,
  items: mentionItems,
  active: mentionActive,
  loading: mentionLoading,
  indexState: mentionIndexState,
  drilled: mentionDrilled,
  crumbs: mentionCrumbs,
  dismiss: dismissMentionMenu,
  goCrumb: goMentionCrumb,
  update: updateMentionMenu,
  select: selectMentionItem,
} = useMentionMenu({
  textSource,
  searchFiles: () => props.searchFiles,
  supportsFileReferences: () => props.supportsFileReferences,
});

/** Menu → item action (E9 dual action): Enter/row-click picks, the folder
 *  arrow drills. Kept as a named handler so the template stays declarative. */
function onMentionSelect(item: SearchFilesResult['items'][number], action: 'pick' | 'drill'): void {
  selectMentionItem(item, action);
}
/** Folder row-tail arrow → drill (same select call, drill action). */
function onMentionDrill(item: SearchFilesResult['items'][number]): void {
  selectMentionItem(item, 'drill');
}
/** The current @query for the menu header (🔍 line / breadcrumb context). */
const mentionQuery = computed(() => {
  const val = textSource.getText();
  const pos = textSource.getCaret();
  const at = val.lastIndexOf('@', pos - 1);
  if (at === -1) return '';
  return val.slice(at + 1, pos);
});

// ---------------------------------------------------------------------------
// Skill trigger menu ($/¥/￥) — see useSkillTriggerMenu for the implementation.
// The composer keeps the keydown orchestration because it also juggles the
// slash menu, the mention menu and history recall. The trigger symbol is only
// an invocation aid: selecting an item inserts the skillMention chip node and
// submission goes through the ordinary mixed-message path.
// ---------------------------------------------------------------------------
const {
  open: skillTriggerOpen,
  items: skillTriggerItems,
  active: skillTriggerActive,
  update: updateSkillTriggerMenu,
  select: selectSkillTriggerItem,
} = useSkillTriggerMenu({
  textSource,
  skills: () => props.skills,
});

// ---------------------------------------------------------------------------
// Pre-submit validation (ADR-009): chips the user INSERTED are validated
// against the live session skill list — pasted text never enters the warning
// path (a log line shaped like `[$envs[$this->x]]` is plain text, not a chip).
// Invalid chips surface a warning bar under the input with "remove invalid" /
// "send anyway" actions. Sending anyway is never blocked. Command references
// never block (execution dispatch is a later batch — TODO US3.6 command
// blocks).
// ---------------------------------------------------------------------------
const invalidRefWarning = ref(false);
const invalidRefs = computed(() =>
  invalidRefWarning.value ? findInvalidSkillMentionAttrs(collectDocChipAttrs(), props.skills) : [],
);
/** Set by "send anyway": skip the inline-reference gate for one submission. */
let sendDespiteInvalidRefs = false;

function checkInlineReferencesBeforeSubmit(): boolean {
  if (sendDespiteInvalidRefs) {
    sendDespiteInvalidRefs = false;
    invalidRefWarning.value = false;
    return true;
  }
  const issues = findInvalidSkillMentionAttrs(collectDocChipAttrs(), props.skills);
  if (issues.length === 0) {
    invalidRefWarning.value = false;
    return true;
  }
  // Show the warning bar; the caller must NOT submit yet.
  invalidRefWarning.value = true;
  return false;
}

function removeInvalidRefs(): void {
  const e = editor.value;
  if (!e) return;
  // Delete every skillMention node whose attrs failed validation (ADR-009:
  // removal can only touch real chips — pasted text has no deletion path).
  // Two-phase: collect ranges first, then delete in DESCENDING position order
  // so earlier deletions cannot shift later nodes' positions.
  const issues = new Set(
    findInvalidSkillMentionAttrs(collectDocChipAttrs(), props.skills).map(
      (i) => `${i.attrs.display}\u0000${i.attrs.target}`,
    ),
  );
  const ranges: Array<{ from: number; to: number }> = [];
  e.state.doc.descendants((node, pos) => {
    if (node.type.name !== 'skillMention') return;
    const { display, target } = node.attrs as { display: string; target: string };
    if (issues.has(`${display}\u0000${target}`)) {
      ranges.push({ from: pos, to: pos + node.nodeSize });
    }
  });
  ranges.sort((a, b) => b.from - a.from);
  // Single chained transaction: one undo step and one update event for the
  // whole removal (descending order keeps positions stable across deletes).
  const chain = e.chain().focus();
  for (const range of ranges) {
    chain.deleteRange(range);
  }
  chain.run();
  invalidRefWarning.value = false;
  syncDraftFromEditor();
  void nextTick(() => {
    editor.value?.commands.focus();
  });
}

function sendAnyway(): void {
  sendDespiteInvalidRefs = true;
  handleSubmit();
}

// ---------------------------------------------------------------------------
// E9 stale file references — the send-time `file.stat` gate outcome. The
// composer was already cleared when the submit left; the App refills the doc
// and routes the gate outcome here. "Remove and send" re-submits the SAME
// text with the filtered references (the stale `@path` spellings stay in the
// text as plain text — the reference channel simply stops carrying them);
// "cancel" leaves the refilled doc editable.
// ---------------------------------------------------------------------------
/** The failed references of the last gated submit (drives the dialog). */
const staleFileRefs = ref<Array<{ path: string; reason: string }> | null>(null);
/** The submit that was gated (text + attachments + references) — the
 *  re-submit material for "remove and send". */
let pendingGatedSubmit: { text: string; attachments: PromptAttachment[]; references: PromptFileReference[] } | null = null;

/** The doc's fileMention chips as the submit's references array. v2 carries
 *  none — the plain-text `@path` spellings in `text` go alone there. */
function collectFileMentionReferences(): PromptFileReference[] {
  const e = editor.value;
  if (!e || !props.supportsFileReferences) return [];
  return toPromptReferences(collectFileMentionAttrs(e.getJSON()));
}

/** Gate outcome arriving from the App layer (invalid_file_references): show
 *  the dialog and refill the composer so the user sees what was blocked. */
function handleStaleFileReferences(
  invalid: Array<{ path: string; reason: string }>,
  payload: { text: string; attachments: PromptAttachment[]; references?: PromptFileReference[] },
): void {
  pendingGatedSubmit = {
    text: payload.text,
    attachments: payload.attachments,
    references: payload.references ?? [],
  };
  staleFileRefs.value = invalid;
  // Refill the (already-cleared) editor so the blocked content is visible
  // and editable — the plain-text `@path` spellings of the stale chips ride
  // along (they never constituted the reference channel).
  loadForEdit(payload.text);
  loadAttachments(payload.attachments.map((a) => ({ fileId: a.fileId, kind: a.kind, url: '', name: a.name })));
}

/** "Remove stale references and send": re-submit the SAME text/attachments
 *  with the surviving references — per the E9 degrade contract the removed
 *  `@path` spellings stay in the text as plain text and simply stop being
 *  structured references. */
function removeStaleFileRefsAndSend(): void {
  const invalid = staleFileRefs.value;
  const gated = pendingGatedSubmit;
  if (!invalid || !gated) return;
  const stale = new Set(invalid.map((i) => i.path));
  const references = gated.references.filter((r) => !stale.has(r.path));
  staleFileRefs.value = null;
  pendingGatedSubmit = null;

  history.push(gated.text);

  // Revoke object URLs and drop the re-submitted attachments.
  previewAttachment.value = null;
  clearAfterSubmit();
  clearEditor();
  clearDraft();
  slashOpen.value = false;
  mentionOpen.value = false;
  skillTriggerOpen.value = false;
  invalidRefWarning.value = false;
  collapseAndRefit();
  emit('submit', {
    text: gated.text,
    attachments: gated.attachments,
    ...(references.length > 0 ? { references } : undefined),
  });
}

/** "Cancel": keep the refilled doc untouched so the user can edit it further. */
function cancelStaleFileRefs(): void {
  staleFileRefs.value = null;
  pendingGatedSubmit = null;
  void nextTick(() => {
    editor.value?.commands.focus();
  });
}

// ---------------------------------------------------------------------------
// Attachments — see useAttachmentUpload. The composer keeps handleSubmit /
// handleSteer (which read the attachments to build the payload) and the
// `hasUpload` toolbar flag.
// ---------------------------------------------------------------------------
const {
  attachments,
  previewAttachment,
  fileInputRef,
  isDragOver,
  removeAttachment,
  openAttachmentPreview,
  closeAttachmentPreview,
  openFilePicker,
  handleFileInputChange,
  handleDragOver,
  handleDragLeave,
  handleDrop,
  clearAfterSubmit,
  loadAttachments,
} = useAttachmentUpload({ uploadImage: () => props.uploadImage, sessionId: () => props.sessionId });

// Silence noUnusedLocals: fileInputRef is used as a template ref (ref="fileInputRef").
void fileInputRef;

onMounted(() => {
  // Reflect the restored draft's grown state (the expand toggle shows for an
  // already-long draft) — no fit pass needed: the editor's box height is pure
  // CSS and isGrown derives from the doc.
});

onUnmounted(() => {
  document.removeEventListener('mousedown', onModesDocClick);
});

// ---------------------------------------------------------------------------
// Submit / keydown
// ---------------------------------------------------------------------------

// loadForEdit comes from useComposerDraft (it lives next to the text state).
function focus(): void {
  // preventScroll keeps the pane from jumping if the composer is already in view
  // or if focus is triggered during an animation/transition.
  editor.value?.commands.focus();
}
function loadAttachmentsForEdit(atts: { fileId?: string; kind: 'image' | 'video' | 'file'; url: string; name?: string }[]): void {
  loadAttachments(atts);
}
defineExpose({ loadForEdit, loadAttachmentsForEdit, focus, handleStaleFileReferences });

// Build the wire-bound attachment payload: images/videos only need the fileId,
// while file parts also carry name/mediaType/size for the daemon's file shape.
function toPromptAttachment(a: Attachment): PromptAttachment {
  return { fileId: a.fileId!, kind: a.kind, name: a.name, mediaType: a.mediaType, size: a.size };
}

// Chip primary action: media opens the lightbox preview; a generic file opens
// in a new tab (browser-renderable types) or downloads, once its upload has
// completed and produced a daemon file id.
function onAttachmentActivate(att: Attachment): void {
  if (att.kind === 'file') {
    if (att.fileId !== undefined) void openFileAttachment(att.fileId, att.name, att.mediaType);
    return;
  }
  openAttachmentPreview(att);
}

function handleSubmit(): void {
  // Pre-submit inline-reference gate (M4, US3.6): invalid skill references
  // surface the warning bar and pause the send until the user picks "remove"
  // or "send anyway" (send anyway re-enters with the flag cleared).
  if (!checkInlineReferencesBeforeSubmit()) return;

  // Normalize the whitespace between adjacent references to one space
  // (cosmetic collapse only, US3.6) before the payload is built.
  const trimmed = normalizeInlineReferencesForSubmit(serializeDoc(), props.skills).trim();

  // An upload is still in flight — submitting now would silently send the
  // message WITHOUT the image. Keep the text + chips (the chip shows its
  // uploading spinner); the user submits again in a moment.
  if (attachments.value.some((a) => a.uploading)) return;

  // Allow submission with images even when text is empty
  const readyAttachments = attachments.value.filter((a) => !a.uploading && !a.error && a.fileId);

  if (!trimmed && readyAttachments.length === 0) return;

  // Record for ↑/↓ recall before the slash branch so commands (with or without
  // args) are recallable too, not just plain messages. `push` ignores empty /
  // whitespace, so an image-only send adds nothing.
  history.push(trimmed);

  // If it's a known slash command, keep the optional tail as command input
  // instead of submitting it as normal chat text. This covers `/goal <task>`,
  // `/swarm <task>`, `/btw <question>`, slash skills with args, and bare
  // commands such as `/model`.
  // A MIXED message (carries at least one inline reference) never takes this
  // branch (US3.6): inside a mixed message only the reference blocks are
  // structured execution units — even when the text happens to START with a
  // slash command, the whole message submits through the ordinary path.
  // TODO(US3.6 command blocks): the engine response will carry the commands to
  // execute; web-side dispatch of inline command references lands in a later
  // batch — for now command references are inserted and rendered only.
  if (trimmed && !hasInlineReferences(trimmed)) {
    const parsed = parseSlash(trimmed);
    const known = parsed
      ? buildSlashItems(props.skills, { includeDisabled: true }).some(
          (item) => item.name === parsed.cmd || item.name === `/${SKILL_COMMAND_PREFIX}${parsed.cmd.slice(1)}`,
        )
      : false;
    if (parsed && known) {
      clearEditor();
      clearDraft();
      slashOpen.value = false;
      collapseAndRefit();
      emit('command', parsed.arg ? `${parsed.cmd} ${parsed.arg}` : parsed.cmd);
      return;
    }
  }

  submitPayloadWithReferences({
    text: trimmed,
    attachments: readyAttachments.map((a) => toPromptAttachment(a)),
  });
}

/**
 * The submit tail shared by handleSubmit and the stale-reference dialog's
 * "remove and send" re-entry: attaches the CURRENT doc's file references to
 * the payload, clears the composer, and emits the submit. The references are
 * recomputed from the live editor so the dialog's removal path (which deletes
 * the stale chips first) automatically sends the filtered set.
 */
function submitPayloadWithReferences(payload: { text: string; attachments: PromptAttachment[] }): void {
  const refs = collectFileMentionReferences();
  const full = {
    text: payload.text,
    attachments: payload.attachments,
    ...(refs.length > 0 ? { references: refs } : undefined),
  };

  // Revoke object URLs and drop the submitted attachments.
  previewAttachment.value = null;
  clearAfterSubmit();

  clearEditor();
  clearDraft();
  slashOpen.value = false;
  mentionOpen.value = false;
  skillTriggerOpen.value = false;
  invalidRefWarning.value = false;
  staleFileRefs.value = null;
  collapseAndRefit();
  emit('submit', full);
}

/**
 * Steer (TUI ctrl+s): push the current text — and the parent merges any queued
 * prompts — straight into the running turn. With an empty composer it still
 * fires when something is queued, so "queue a few thoughts, then ctrl+s" works.
 */
function handleSteer(): void {
  if (!props.running) return;
  if (attachments.value.some((a) => a.uploading)) return;
  // Same pre-submit gate as handleSubmit (M4): a steer with broken references
  // warns instead of silently sending.
  if (!checkInlineReferencesBeforeSubmit()) return;

  const trimmed = normalizeInlineReferencesForSubmit(serializeDoc(), props.skills).trim();
  const readyAttachments = attachments.value.filter((a) => !a.uploading && !a.error && a.fileId);
  if (!trimmed && readyAttachments.length === 0 && props.queued.length === 0) return;

  const payload = {
    text: trimmed,
    attachments: readyAttachments.map((a) => toPromptAttachment(a)),
  };
  clearAfterSubmit();
  history.push(trimmed);
  clearEditor();
  clearDraft();
  slashOpen.value = false;
  mentionOpen.value = false;
  skillTriggerOpen.value = false;
  invalidRefWarning.value = false;
  collapseAndRefit();
  emit('steer', payload);
}

// The editor's keydown orchestration. IME composition is NOT handled here —
// ProseMirror's own composition machinery owns it (the old manual
// compositionStart/End guards existed only for the textarea).
function handleEditorKeydown(e: KeyboardEvent): boolean {
  // Close dropdowns on Escape
  if (e.key === 'Escape') {
    if (dropdownOpen.value) {
      e.preventDefault();
      closeDropdown();
      return true;
    }
    if (permDropdownOpen.value) {
      e.preventDefault();
      closePermDropdown();
      return true;
    }
  }

  // Slash menu navigation
  if (slashOpen.value) {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      slashActive.value = (slashActive.value + 1) % slashItems.value.length;
      return true;
    }
    if (e.key === 'ArrowUp') {
      e.preventDefault();
      slashActive.value = (slashActive.value - 1 + slashItems.value.length) % slashItems.value.length;
      return true;
    }
    if (e.key === 'Enter' || e.key === 'Tab') {
      e.preventDefault();
      const item = slashItems.value[slashActive.value];
      if (item) selectSlashCommand(item);
      return true;
    }
    if (e.key === 'Escape') {
      e.preventDefault();
      slashOpen.value = false;
      return true;
    }
  }

  // Skill trigger menu navigation
  if (skillTriggerOpen.value) {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      skillTriggerActive.value = (skillTriggerActive.value + 1) % skillTriggerItems.value.length;
      return true;
    }
    if (e.key === 'ArrowUp') {
      e.preventDefault();
      skillTriggerActive.value = (skillTriggerActive.value - 1 + skillTriggerItems.value.length) % skillTriggerItems.value.length;
      return true;
    }
    if (e.key === 'Enter' || e.key === 'Tab') {
      e.preventDefault();
      const item = skillTriggerItems.value[skillTriggerActive.value];
      if (item) selectSkillTriggerItem(item);
      return true;
    }
    if (e.key === 'Escape') {
      e.preventDefault();
      skillTriggerOpen.value = false;
      return true;
    }
  }

  // Mention menu navigation (E9 uiux §3): Enter always picks, Tab drills a
  // directory (and picks a file — same as Enter); Shift+Tab is unbound. Esc
  // closes + remembers. While loading, Enter/Tab are consumed with no
  // action so a stale item can never be committed.
  if (mentionOpen.value) {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      mentionActive.value = (mentionActive.value + 1) % Math.max(1, mentionItems.value.length);
      return true;
    }
    if (e.key === 'ArrowUp') {
      e.preventDefault();
      mentionActive.value = (mentionActive.value - 1 + Math.max(1, mentionItems.value.length)) % Math.max(1, mentionItems.value.length);
      return true;
    }
    if (e.key === 'Enter') {
      e.preventDefault();
      if (!mentionLoading.value) {
        const item = mentionItems.value[mentionActive.value];
        if (item) selectMentionItem(item, 'pick');
      }
      return true;
    }
    if (e.key === 'Tab' && !e.shiftKey) {
      e.preventDefault();
      if (!mentionLoading.value) {
        const item = mentionItems.value[mentionActive.value];
        if (!item) return true;
        // Tab drills a directory; on a file it picks (Enter parity).
        selectMentionItem(item, item.kind === 'directory' ? 'drill' : 'pick');
      }
      return true;
    }
    if (e.key === 'Escape') {
      e.preventDefault();
      dismissMentionMenu();
      return true;
    }
  }

  // Ctrl+S / Cmd+S — steer into the running turn (TUI parity)
  if (e.key === 's' && (e.ctrlKey || e.metaKey) && !e.shiftKey && !e.altKey) {
    if (props.running) {
      e.preventDefault();
      handleSteer();
    }
    return true;
  }

  // History recall (shell-style ↑/↓) — see useInputHistory for the machinery.
  //
  // Disabled entirely in the expanded editor: that mode is for composing long
  // multi-line text, so the arrows always move the caret within the draft and
  // never jump to a previous message.
  //
  // ENTERING history: a plain ArrowUp only recalls when the caret is at the
  // very start of the doc (selection.from === 0), so editing a multi-line
  // draft with the arrows still works — ArrowUp moves the caret within the
  // draft until it reaches the top, instead of jumping to a previous message
  // mid-navigation. ONCE BROWSING, the arrows walk history directly, regardless
  // of where the caret landed — a recalled multi-line entry leaves the caret
  // at its end, and the old "must be at the start" gate then trapped it
  // there, so further ArrowUp did nothing ("only one step back"). Walking
  // freely while browsing fixes that; typing exits history (the update
  // handler resets browsing), after which the arrows move the caret normally
  // again.
  if (!expanded.value && !slashOpen.value && !mentionOpen.value && !skillTriggerOpen.value && !e.shiftKey && !e.altKey && !e.metaKey && !e.ctrlKey) {
    const browsing = history.isBrowsing();
    if (e.key === 'ArrowUp' && history.hasHistory() && (browsing || history.caretAtTextStart())) {
      e.preventDefault();
      history.recallOlder();
      return true;
    }
    if (e.key === 'ArrowDown' && browsing) {
      e.preventDefault();
      history.recallNewer();
      return true;
    }
  }

  // Normal Enter / Shift+Enter
  if (e.key === 'Enter' && !e.shiftKey) {
    // Expanded editor: Enter inserts a newline; Cmd/Ctrl+Enter sends.
    // (Clicking the send button always sends.) Shift+Enter falls through to
    // the default hard-break insert, so behavior matches either way.
    if (expanded.value && !(e.metaKey || e.ctrlKey)) {
      return false;
    }
    e.preventDefault();
    handleSubmit();
    return true;
  }

  return false;
}

// ---------------------------------------------------------------------------
// Computed
// ---------------------------------------------------------------------------

// Send is always "send" — while running it enqueues (handled upstream by
// sendPrompt). Interrupt lives on a separate Stop button so the two can never
// be confused.
const sendLabel = computed(() => t('composer.send'));
const hasUpload = computed(() => !!props.uploadImage);

// ---------------------------------------------------------------------------
// Bottom toolbar — split into individual controls
// ---------------------------------------------------------------------------

const dropdownOpen = ref(false);
const permDropdownOpen = ref(false);
const toolbarRef = ref<HTMLElement | null>(null);

function toggleDropdown(): void {
  dropdownOpen.value = !dropdownOpen.value;
  if (dropdownOpen.value) {
    permDropdownOpen.value = false;
    closeModes();
    document.addEventListener('click', onDocClick, true);
  } else {
    document.removeEventListener('click', onDocClick, true);
  }
}

function closeDropdown(): void {
  dropdownOpen.value = false;
  if (!permDropdownOpen.value) {
    document.removeEventListener('click', onDocClick, true);
  }
}

function togglePermDropdown(): void {
  permDropdownOpen.value = !permDropdownOpen.value;
  if (permDropdownOpen.value) {
    dropdownOpen.value = false;
    closeModes();
    document.addEventListener('click', onDocClick, true);
  } else {
    document.removeEventListener('click', onDocClick, true);
  }
}

function closePermDropdown(): void {
  permDropdownOpen.value = false;
  if (!dropdownOpen.value) {
    document.removeEventListener('click', onDocClick, true);
  }
}

function onDocClick(e: MouseEvent): void {
  if (toolbarRef.value && !toolbarRef.value.contains(e.target as Node)) {
    closeDropdown();
    closePermDropdown();
  }
}

onUnmounted(() => {
  document.removeEventListener('click', onDocClick, true);
});

// Clamped to 0–100: ctxUsed can momentarily exceed ctxMax (estimates), and
// ctxMax can be 0 before the first status fetch — both broke the ring. ceil
// (not round) so a session under 0.5% usage still shows a sliver of arc —
// Math.round floored it to an empty, "no data"-looking ring.
const pct = computed(() => {
  const max = props.status?.ctxMax ?? 0;
  if (max <= 0) return 0;
  return Math.min(100, Math.max(0, Math.ceil(((props.status?.ctxUsed ?? 0) / max) * 100)));
});

const ctxTooltip = computed(() => {
  const used = formatTokens(props.status?.ctxUsed ?? 0);
  const max = formatTokens(props.status?.ctxMax ?? 0);
  return t('status.ctxTooltip', { used, max, pct: pct.value });
});

const showCompact = computed(() => pct.value >= 80);

// Identity is the model id — display/model names can collide across providers.
const currentModel = computed(() =>
  props.models?.find((m) => m.id === props.status?.modelId),
);
const thinkingAvailability = computed(() => modelThinkingAvailability(currentModel.value));
const thinkingSegments = computed(() => segmentsFor(currentModel.value));
// The client resolves the level per model (the model's stored pick when still
// declared, else the catalog default), so what arrives here is valid for the
// active model and highlights its segment. An undeclared level can only appear
// transiently, before the catalog loads, and simply highlights no segment.
const thinkingLevel = computed(() => effectiveThinkingLevel(currentModel.value, props.thinking));
const activeThinkingSegment = computed(() => {
  const segs = thinkingSegments.value;
  return segs.includes(thinkingLevel.value) ? thinkingLevel.value : '';
});
const thinkingOn = computed(() => isThinkingOn(thinkingLevel.value));
// Single-segment (always-on boolean) or unsupported models can't be changed.
const thinkingReadonly = computed(
  () => thinkingAvailability.value === 'unsupported' || thinkingSegments.value.length <= 1,
);
// Footer-style suffix: effort models show the concrete level; boolean models
// keep the plain "thinking" tag; off shows nothing.
const thinkingSuffix = computed(() => {
  if (!thinkingOn.value) return '';
  const hasEfforts = (currentModel.value?.supportEfforts?.length ?? 0) > 0;
  const level = thinkingLevel.value;
  if (hasEfforts && level !== 'on') return t('composer.thinkingSuffixEffort', { level });
  return t('composer.thinkingSuffix');
});
function setThinkingSegment(draft: string): void {
  if (thinkingReadonly.value) return;
  emit('setThinking', commitLevel(currentModel.value, draft));
}

// Plan toggle
const planOn = computed(() => props.planMode === true);
const swarmOn = computed(() => props.swarmMode === true);
const collaborationOn = computed(() => props.collaborationMode === true);
const goalActive = computed(() => props.activationBadges?.goal !== null);
const goalArmed = computed(() => goalActive.value || props.goalMode === true);

// Modes selector (plan / goal / swarm / collaboration) — the popover that
// replaces the bare "plan" pill. Plan/Swarm/Collaboration are real client
// toggles; goal reflects agent-driven state and focuses its card when active.
const modesOpen = ref(false);
const modesRef = ref<HTMLElement | null>(null);
const modesMenuRef = ref<HTMLElement | null>(null);
// The menu is position:fixed (so no composer stacking context can paint over
// it); these coords anchor it just above the pill, computed on open.
const modesMenuStyle = ref<Record<string, string>>({});
const anyModeActive = computed(
  () => planOn.value || swarmOn.value || collaborationOn.value || goalArmed.value,
);
function closeModes(): void {
  modesOpen.value = false;
  document.removeEventListener('mousedown', onModesDocClick);
}
function onModesDocClick(e: MouseEvent): void {
  const t = e.target as Node;
  if (modesRef.value?.contains(t) || modesMenuRef.value?.contains(t)) return;
  closeModes();
}
function toggleModes(): void {
  if (modesOpen.value) {
    closeModes();
    return;
  }
  // Keep the toolbar menus mutually exclusive so they never overlap.
  closeDropdown();
  closePermDropdown();
  const r = modesRef.value?.getBoundingClientRect();
  if (r) {
    modesMenuStyle.value = {
      left: `${Math.round(r.left)}px`,
      bottom: `${Math.round(window.innerHeight - r.top + 8)}px`,
    };
  }
  modesOpen.value = true;
  setTimeout(() => document.addEventListener('mousedown', onModesDocClick), 0);
}
// Permission modes
const PERM_MODES: { mode: PermissionMode; color: string; labelKey: string; descKey: string }[] = [
  { mode: 'manual', color: 'var(--dim)', labelKey: 'status.permissionManual', descKey: 'status.permissionManualDesc' },
  { mode: 'yolo', color: 'var(--color-warning)', labelKey: 'status.permissionYolo', descKey: 'status.permissionYoloDesc' },
  { mode: 'auto', color: 'var(--color-danger)', labelKey: 'status.permissionAuto', descKey: 'status.permissionAutoDesc' },
];

function choosePermission(mode: PermissionMode): void {
  emit('setPermission', mode);
  closePermDropdown();
}

const permInfo = computed(() => PERM_MODES.find((p) => p.mode === props.status?.permission));
const permLabel = computed(() => (permInfo.value ? t(permInfo.value.labelKey) : ''));

// ---------------------------------------------------------------------------
// Model dropdown — current provider models + thinking + more
// ---------------------------------------------------------------------------

const currentProvider = computed(() => {
  return currentModel.value?.provider ?? '';
});

const providerModels = computed(() => {
  if (!currentProvider.value || !props.models?.length) return [];
  return props.models.filter((m) => m.provider === currentProvider.value);
});

const starredSet = computed(() => new Set(props.starredIds ?? []));
function isStarred(modelId: string): boolean {
  return starredSet.value.has(modelId);
}
const starredOtherModels = computed(() => {
  if (!props.models?.length) return [];
  return props.models.filter(
    (m) => isStarred(m.id) && m.provider !== currentProvider.value,
  );
});

function selectModel(modelId: string): void {
  emit('selectModel', modelId);
  closeDropdown();
}
</script>

<template>
  <div
    class="composer"
    :class="{ 'drag-over': isDragOver, expanded }"
    @dragover="handleDragOver"
    @dragleave="handleDragLeave"
    @drop="handleDrop"
  >
    <div v-if="previewAttachment" class="att-lightbox" @click.self="closeAttachmentPreview">
      <div class="att-lightbox-card">
        <Tooltip :text="t('model.close')">
          <button type="button" class="att-lightbox-close" @click="closeAttachmentPreview">✕</button>
        </Tooltip>
        <video
          v-if="previewAttachment.kind === 'video'"
          class="att-lightbox-media"
          :src="previewAttachment.previewUrl"
          controls
          playsinline
        />
        <img v-else class="att-lightbox-media" :src="previewAttachment.previewUrl" :alt="previewAttachment.name" />
        <div class="att-lightbox-name">{{ previewAttachment.name }}</div>
      </div>
    </div>

    <!-- Main composer card -->
    <div class="composer-card">
      <!-- Input row with popup menus -->
      <div class="cin-wrap">
        <!-- Attachment strip — inside the card, above the editor, so pasted
             media reads as part of the input itself. Media renders as bare
             thumbnail tiles (AttachmentChip tile mode); generic files keep
             the named pill. -->
        <div v-if="attachments.length > 0" class="att-strip">
          <AttachmentChip
            v-for="att in attachments"
            :key="att.localId"
            :kind="att.kind"
            :name="att.name"
            :url="att.previewUrl"
            :file-id="att.fileId"
            :media-type="att.mediaType"
            :size="att.size"
            :uploading="att.uploading"
            :error="att.error"
            tile
            removable
            :remove-label="t('composer.removeNamed', { name: att.name })"
            @activate="onAttachmentActivate(att)"
            @remove="removeAttachment(att.localId)"
          />
        </div>

        <!-- Slash menu (above textarea) -->
        <SlashMenu
          v-if="slashOpen"
          :items="slashItems"
          :active-index="slashActive"
          @select="selectSlashCommand"
          @hover="slashActive = $event"
        />

        <!-- Skill trigger menu (above textarea) — reuses SlashMenu, skill items only -->
        <SlashMenu
          v-if="skillTriggerOpen"
          :items="skillTriggerItems"
          :active-index="skillTriggerActive"
          :grouped="false"
          @select="selectSkillTriggerItem"
          @hover="skillTriggerActive = $event"
        />

        <!-- Mention menu (above textarea) -->
        <MentionMenu
          v-if="mentionOpen"
          :items="mentionItems"
          :active-index="mentionActive"
          :loading="mentionLoading"
          :index-state="mentionIndexState"
          :drilled="mentionDrilled"
          :crumbs="mentionCrumbs"
          :query="mentionQuery"
          @select="onMentionSelect"
          @drill="onMentionDrill"
          @crumb="goMentionCrumb"
          @hover="mentionActive = $event"
        />

        <div class="input-row">
          <!-- Contenteditable host (TipTap). The .ProseMirror element carries
               the .ph class (see editorProps.attributes) so the existing
               input-box styles apply; skill/command references are real
               skillMention DOM nodes (chip pills) inside the text flow — the
               old transparent-textarea + mirror overlay is gone. The
               placeholder rides a CSS variable (the ::before rule reads it
               while the first paragraph is empty). -->
          <EditorContent
            v-if="editor"
            :editor="editor"
            class="ph-box"
            :style="{ '--placeholder': `'${placeholder}'` }"
          />
          <div v-else class="ph-box ph-loading" aria-hidden="true"></div>
          <button
            v-if="expanded || isGrown"
            class="expand-btn"
            type="button"
            :aria-label="expanded ? t('composer.collapseTitle') : t('composer.expandTitle')"
            @click="toggleExpand"
          >
            <Icon v-if="expanded" name="collapse" size="sm" />
            <Icon v-else name="expand" size="sm" />
          </button>
        </div>

        <!-- Invalid inline-reference warning (M4, US3.6): lists the broken
             chips with the reason, offers remove / send-anyway. -->
        <div v-if="invalidRefWarning && invalidRefs.length > 0" class="ref-warning">
          <Banner variant="warning">
            {{ t('composer.invalidRefsTitle') }}
            <ul class="ref-warning-list">
              <li v-for="(issue, i) in invalidRefs" :key="i">
                {{ issue.attrs.display }} —
                {{ issue.reason === 'disabled'
                  ? t('composer.invalidRefDisabled')
                  : t('composer.invalidRefMissing') }}
              </li>
            </ul>
            <div class="ref-warning-actions">
              <Button size="sm" variant="danger-soft" @click="removeInvalidRefs">{{ t('composer.invalidRefsRemove') }}</Button>
              <Button size="sm" variant="ghost" @click="sendAnyway">{{ t('composer.invalidRefsSendAnyway') }}</Button>
            </div>
          </Banner>
        </div>
        <!-- E9 stale file references: the send-time file.stat gate rejected —
             offer remove-and-send (the failed chips are deleted, the surviving
             references go through) or cancel (doc stays editable). -->
        <div v-if="staleFileRefs && staleFileRefs.length > 0" class="ref-warning">
          <Banner variant="warning">
            {{ t('composer.invalidFileRefsTitle') }}
            <ul class="ref-warning-list">
              <li v-for="(issue, i) in staleFileRefs" :key="i">
                {{ issue.path }} — {{ t(`composer.invalidFileRefsReason.${issue.reason}`) }}
              </li>
            </ul>
            <div class="ref-warning-actions">
              <Button size="sm" variant="danger-soft" @click="removeStaleFileRefsAndSend">{{ t('composer.invalidFileRefsRemove') }}</Button>
              <Button size="sm" variant="ghost" @click="cancelStaleFileRefs">{{ t('composer.invalidFileRefsCancel') }}</Button>
            </div>
          </Banner>
        </div>
      </div>
      <input
        v-if="hasUpload"
        ref="fileInputRef"
        type="file"
        multiple
        class="file-input-hidden"
        @change="handleFileInputChange"
      />

      <!-- Bottom toolbar — split into individual controls -->
      <div ref="toolbarRef" class="toolbar">
        <!-- Left: attach + permission + plan -->
        <div class="toolbar-left">
          <IconButton
            v-if="hasUpload"
            size="md"
            :label="t('composer.attachFile')"
            @click="openFilePicker"
          >
            <Icon name="attachment" />
          </IconButton>

          <!-- Permission pill — click to open dropdown -->
          <span
            v-if="status"
            class="perm-pill"
            :class="['perm-' + status.permission, { open: permDropdownOpen }]"
            role="button"
            tabindex="0"
            @click.stop="togglePermDropdown"
            @keydown.enter="togglePermDropdown"
            @keydown.space.prevent="togglePermDropdown"
          >{{ permLabel }}</span>

          <!-- Permission dropdown — anchored to the toolbar left side -->
          <div v-if="permDropdownOpen && status" class="perm-dropdown" role="menu" @click.stop>
            <button
              v-for="opt in PERM_MODES"
              :key="opt.mode"
              class="pd-row"
              :class="{ 'is-current': opt.mode === status.permission }"
              role="menuitem"
              @click="choosePermission(opt.mode)"
            >
              <span class="pd-check"><Icon v-if="opt.mode === status.permission" name="check" size="sm" /></span>
              <span class="pd-info">
                <span class="pd-name" :style="{ color: opt.color }">{{ t(opt.labelKey) }}</span>
                <span class="pd-desc">{{ t(opt.descKey) }}</span>
              </span>
            </button>
          </div>

          <!-- Modes selector (plan / goal / swarm / collaboration) — replaces the plan pill. -->
          <div v-if="status" ref="modesRef" class="modes">
            <button
              type="button"
              class="mode-pill"
              :class="{ on: anyModeActive, open: modesOpen }"
              @click.stop="toggleModes"
            >
              <span class="mode-label">{{ t('status.modesLabel') }}</span>
              <span v-if="planOn" class="mode-tag">{{ t('status.planLabel') }}</span>
              <span v-if="swarmOn" class="mode-tag">{{ t('status.swarmLabel') }}</span>
              <span v-if="collaborationOn && collaborationSupported" class="mode-tag">{{ t('status.collaborationLabel') }}</span>
              <span v-if="goalArmed" class="mode-tag">{{ t('status.goalLabel') }}</span>
            </button>

            <div v-if="modesOpen" ref="modesMenuRef" class="modes-menu" :style="modesMenuStyle">
              <!-- Plan — functional client toggle -->
              <button type="button" class="mode-row" :class="{ on: planOn }" @click="emit('togglePlan')">
                <span class="mode-row-name">{{ t('status.planLabel') }}</span>
                <span class="mode-switch" :class="{ on: planOn }"><span class="mode-knob" /></span>
              </button>
              <!-- Swarm — functional client toggle -->
              <button type="button" class="mode-row" :class="{ on: swarmOn }" @click="emit('toggleSwarm')">
                <span class="mode-row-name">{{ t('status.swarmLabel') }}</span>
                <span class="mode-switch" :class="{ on: swarmOn }"><span class="mode-knob" /></span>
              </button>
              <!-- Collaboration — functional client toggle (opt-in exposure of
                   the Collaboration* tools); only on backends with a
                   collaboration engine effect (B10 D-24). -->
              <button v-if="collaborationSupported" type="button" class="mode-row" :class="{ on: collaborationOn }" @click="emit('toggleCollaboration')">
                <span class="mode-row-name">{{ t('status.collaborationLabel') }}</span>
                <span class="mode-switch" :class="{ on: collaborationOn }"><span class="mode-knob" /></span>
              </button>
              <!-- Goal — lifecycle controls when active; switch is on when active or armed. -->
              <div class="mode-row mode-row-goal" :class="{ on: goalActive || props.goalMode }">
                <button
                  type="button"
                  class="mode-row-main"
                  @click="goalActive ? emit('controlGoal', 'cancel') : emit('toggleGoal')"
                >
                  <span class="mode-row-name">{{ t('status.goalLabel') }}</span>
                  <span v-if="!goalActive" class="mode-switch" :class="{ on: props.goalMode }"><span class="mode-knob" /></span>
                </button>
                <div v-if="goalActive" class="mode-row-actions">
                  <button
                    type="button"
                    class="mode-row-action"
                    @click="emit('controlGoal', 'pause')"
                  >{{ t('status.goalPause') }}</button>
                  <button
                    type="button"
                    class="mode-row-action"
                    @click="emit('controlGoal', 'resume')"
                  >{{ t('status.goalResume') }}</button>
                </div>
              </div>
            </div>
          </div>

        </div>

        <!-- Right: ctx + model -->
        <div class="toolbar-right">
          <!-- Compact chip when context is high -->
          <button v-if="showCompact" class="compact-chip" @click.stop="emit('compact')">/compact</button>

          <!-- Context meter — circular ring only; the full usage (used/max/pct)
               lives in the tooltip. The ring is aria-hidden, so the trigger
               exposes those numbers via aria-label; focusable so keyboard and
               switch-control users reach the same tooltip hover users see. -->
          <Tooltip :text="ctxTooltip">
            <span
              v-if="status && !hideContext"
              class="ctx-group"
              role="img"
              tabindex="0"
              :aria-label="ctxTooltip"
            >
              <ContextRing :pct="pct" />
            </span>
          </Tooltip>

          <!-- Model pill — click to open quick-switch dropdown -->
          <span
            v-if="status"
            class="model-pill"
            :class="{ open: dropdownOpen }"
            role="button"
            tabindex="0"
            @click.stop="toggleDropdown"
            @keydown.enter="toggleDropdown"
            @keydown.space.prevent="toggleDropdown"
          >
            <b>{{ currentModel?.displayName ?? status.model }}</b>
            <span v-if="thinkingSuffix" class="think-suffix">{{ thinkingSuffix }}</span>
            <Icon class="cv" name="chevron-down" size="sm" />
          </span>
          <Tooltip v-if="running" :text="t('composer.interruptTitle')">
            <button
              class="stop"
              :aria-label="t('composer.interrupt')"
              @click="emit('interrupt')"
            >
              <Icon name="stop" size="sm" />
            </button>
          </Tooltip>
          <button
            class="send"
            :class="{ 'is-starting': starting }"
            :aria-label="sendLabel"
            :disabled="starting"
            @click="handleSubmit()"
          >
            <Spinner v-if="starting" size="sm" />
            <Icon v-else name="send" size="sm" />
          </button>
        </div>

        <!-- Model dropdown — current provider models + controls + more -->
        <div v-if="dropdownOpen && status" class="model-dropdown" role="menu" @click.stop>
          <!-- Starred models from other providers -->
          <div v-if="starredOtherModels.length > 0" class="md-section">{{ t('status.starredModels') }}</div>
          <button
            v-for="m in starredOtherModels"
            :key="m.id"
            class="md-row"
            :class="{ 'is-current': m.id === status.modelId }"
            role="menuitem"
            @click="selectModel(m.id)"
          >
            <span class="md-check"><Icon v-if="m.id === status.modelId" name="check" size="sm" /></span>
            <span class="md-name">{{ m.displayName ?? m.model }}</span>
            <span class="md-provider">{{ m.provider }}</span>
            <Icon class="md-star" name="star" size="sm" />
          </button>

          <div v-if="starredOtherModels.length > 0" class="md-divider" />

          <!-- Current provider models -->
          <div v-if="providerModels.length > 0" class="md-section">{{ currentProvider }}</div>
          <button
            v-for="m in providerModels"
            :key="m.id"
            class="md-row"
            :class="{ 'is-current': m.id === status.modelId }"
            role="menuitem"
            @click="selectModel(m.id)"
          >
            <span class="md-check"><Icon v-if="m.id === status.modelId" name="check" size="sm" /></span>
            <span class="md-name">{{ m.displayName ?? m.model }}</span>
            <Icon v-if="isStarred(m.id)" class="md-star" name="star" size="sm" />
          </button>

          <div v-if="providerModels.length > 0" class="md-divider" />

          <!-- Thinking level — segmented control. Effort models show every
               declared level; boolean models show On/Off; unsupported shows a note. -->
          <div class="md-thinking" :class="{ 'is-readonly': thinkingReadonly }">
            <span class="md-name">{{ t('status.thinkingLabel') }}</span>
            <span
              v-if="thinkingAvailability === 'unsupported'"
              class="md-note"
            >{{ t('status.modeNotSupported') }}</span>
            <div
              v-else
              class="effort-segments"
              role="group"
              :aria-label="t('status.thinkingLabel')"
            >
              <button
                v-for="seg in thinkingSegments"
                :key="seg"
                type="button"
                class="effort-seg"
                :class="{ 'is-active': seg === activeThinkingSegment }"
                :disabled="thinkingReadonly"
                @click="setThinkingSegment(seg)"
              >{{ effortLabel(seg) }}</button>
            </div>
          </div>

          <div class="md-divider" />
          <div class="md-cache-note">{{ t('status.cacheNote') }}</div>

          <div class="md-divider" />

          <!-- More models → open full picker -->
          <button class="md-row md-row-more" role="menuitem" @click="closeDropdown(); emit('pickModel');">
            <span class="md-name">{{ t('status.moreModels') }}</span>
          </button>
        </div>
      </div>
  </div>

  <!-- Full-window drop target affordance: shown while files are dragged anywhere
       over the app (document-level listeners in useAttachmentUpload). Pure CSS
       show/hide — a Vue <Transition> can strand an invisible node when the drag
       ends before the enter transition starts. -->
  <div class="drop-overlay" :class="{ show: isDragOver }" aria-hidden="true">
    <div class="drop-card">
      <Icon name="file-plus" size="lg" />
      <span>{{ t('composer.dropToAttach') }}</span>
    </div>
  </div>
</div>
</template>

<style scoped>
.composer {
  padding: 7px var(--dock-inline-right, 16px) 12px var(--dock-inline-left, 16px);
  background: transparent;
  transition: background 0.12s;
}

.composer.drag-over {
  background: var(--color-accent-soft);
}

/* Full-window drop overlay: pointer-events none — the document-level handlers
   in useAttachmentUpload receive the drop, the overlay is purely visual. */
.drop-overlay {
  position: fixed;
  inset: 0;
  z-index: var(--z-modal);
  display: flex;
  align-items: center;
  justify-content: center;
  background: color-mix(in srgb, var(--color-bg) 72%, transparent);
  pointer-events: none;
  opacity: 0;
  visibility: hidden;
  transition:
    opacity var(--duration-base) ease,
    visibility var(--duration-base);
}
.drop-overlay.show {
  opacity: 1;
  visibility: visible;
}
.drop-card {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-4) var(--space-6);
  border-radius: var(--radius-lg);
  border: 1.5px dashed var(--color-accent);
  background: var(--color-bg);
  color: var(--color-accent);
  font-size: var(--ui-font-size-lg);
  font-weight: var(--weight-medium);
  box-shadow: var(--shadow-md);
}

/* Main composer card */
.composer-card {
  --composer-send-size: 32px;
  --composer-send-inset: var(--space-2);
  position: relative;
  border: 1px solid var(--line);
  border-radius: calc((var(--composer-send-size) / 2) + var(--composer-send-inset) + var(--space-3));
  corner-shape: superellipse(1.5);
  background: var(--bg);
  box-shadow: var(--shadow-md);
  transition: border-color 0.15s, box-shadow 0.15s;
}
.composer-card:focus-within {
  border-color: var(--color-accent);
  box-shadow: var(--shadow-md), 0 0 0 3px var(--color-accent-soft);
}



/* Attachment strip — inside the composer card, above the editor. The chip
   itself is the shared AttachmentChip; this is only the row layout. The
   bottom margin separates it from the text input; the top row of .cin-wrap
   padding collapses so the strip sits flush under the card's top edge. */
.att-strip {
  display: flex;
  flex-wrap: wrap;
  align-items: flex-start;
  gap: 6px;
  margin: -6px 0 6px;
}

.att-lightbox {
  position: fixed;
  inset: 0;
  z-index: var(--z-overlay);
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 24px;
  background: rgba(20, 23, 28, 0.62);
}
.att-lightbox-card {
  position: relative;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 10px;
  max-width: min(960px, calc(100vw - 48px));
  max-height: calc(100vh - 48px);
}
.att-lightbox-media {
  max-width: 100%;
  max-height: calc(100vh - 96px);
  border-radius: 6px;
  background: var(--bg);
  box-shadow: var(--shadow-xl);
  object-fit: contain;
}
.att-lightbox-name {
  max-width: 100%;
  color: var(--color-text-on-accent);
  font-family: var(--mono);
  font-size: calc(var(--ui-font-size) - 2px);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.att-lightbox-close {
  position: absolute;
  top: -14px;
  right: -14px;
  width: 28px;
  height: 28px;
  border: 1px solid rgba(255,255,255,0.45);
  border-radius: 50%;
  background: rgba(20,23,28,0.82);
  color: var(--color-text-on-accent);
  cursor: pointer;
}

/* Hidden file input */
.file-input-hidden {
  display: none;
}

/* Wrapper that establishes a positioning context for the popup menus */
.cin-wrap {
  position: relative;
  padding: 14px 16px 8px;
}

/* Input row */
.input-row {
  display: flex;
  align-items: flex-start;
  gap: var(--space-2);
}

/* Expand toggle — top-right of the textarea */
.expand-btn {
  width: 22px;
  height: 22px;
  display: flex;
  align-items: center;
  justify-content: center;
  border: none;
  border-radius: 6px;
  background: transparent;
  color: var(--dim);
  cursor: pointer;
  padding: 0;
  transition: background 0.12s, color 0.12s;
}

.expand-btn:hover {
  background: var(--panel2);
  color: var(--color-text);
}

.expand-btn:focus-visible {
  outline: 2px solid var(--color-accent);
  outline-offset: 2px;
}

/* Textarea + chip overlay box (M3). The box establishes the positioning
   context for the editor host; the editor keeps its own layout metrics
   (flex:1 within .input-row). */
.ph-box {
  position: relative;
  flex: 1;
  min-width: 0;
  display: block;
}

/* The contenteditable (.ProseMirror carries the .ph class via
   editorProps.attributes). Same box metrics as the old textarea: rest at
   36px, cap at 1/4 viewport, internal scroll beyond. */
.ph-box :deep(.ProseMirror) {
  color: var(--color-text);
  caret-color: var(--color-text);
  border: none;
  outline: none;
  resize: none;
  font-family: var(--font-ui);
  font-size: var(--content-font-size);
  background: transparent;
  min-height: 36px;
  max-height: calc(100vh / 4);
  overflow-y: auto;
  line-height: 1.5;
  margin-bottom: 6px;
  white-space: pre-wrap;
  word-break: break-word;
}

/* Placeholder via CSS ::before while the editor is empty — the is-editor-
   empty class is toggled on the ProseMirror element on every transaction
   (see the watcher); the text rides the --placeholder variable set on .ph-box
   from the live computed (no placeholder extension dependency). */
.ph-box :deep(.ProseMirror.is-editor-empty > p:first-child::before) {
  content: var(--placeholder, '');
  color: var(--muted);
  pointer-events: none;
  float: left;
  height: 0;
}

/* The skillMention chip node inside the editor — same visual language as the
   shared ReferenceChip (bubble side): small pill, mono font, soft accent fill;
   commands swap to the neutral identity. */
.ph-box :deep(.ref-chip) {
  display: inline-flex;
  align-items: center;
  gap: 3px;
  padding: 0 5px;
  border-radius: var(--radius-xs);
  border: 1px solid var(--color-accent-bd);
  background: var(--color-accent-soft);
  color: var(--color-text);
  font-family: var(--font-mono);
  /* Fixed pill height that fits INSIDE the 1.5em line box (1.42em at a 14px
     content font ≈ 20px vs the 21px line). line-height 1 stops the chip from
     inheriting 1.5 and inflating to ~23px — the old setup pushed the pill
     below the text baseline. vertical-align middle centers the chip on the
     text's x-height midline (the competitor's align-top + fixed leading is
     the same goal, expressed relative to the baseline instead of the line
     box, so it holds for wrapped lines too). */
  height: 1.42em;
  line-height: 1;
  max-width: 220px;
  vertical-align: middle;
}
.ph-box :deep(.ref-chip .ref-chip-icon) {
  flex: none;
  color: var(--color-accent);
}
.ph-box :deep(.ref-chip[data-kind='command']) {
  background: var(--color-surface-sunken);
  border-color: var(--color-line);
}
.ph-box :deep(.ref-chip[data-kind='command'] .ref-chip-icon) {
  color: var(--color-text-muted);
}
.ph-box :deep(.ref-chip .ref-chip-name) {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* Expanded editor: a tall composing area at ~70% of the viewport — clearly
   larger than the auto-grow cap, while leaving room for the chat header, the
   bottom toolbar row, and padding so nothing gets clipped. Content beyond it
   scrolls internally. */
.composer.expanded .ph-box :deep(.ProseMirror) {
  min-height: 70vh;
  max-height: 70vh;
}

/* Invalid inline-reference warning bar (M4) — below the input row. */
.ref-warning {
  padding: 6px 16px 8px;
}

.ref-warning-list {
  margin: 4px 0 0;
  padding-left: 18px;
  font-family: var(--font-mono);
  font-size: var(--text-sm);
}

.ref-warning-list li {
  list-style: disc;
}

.ref-warning-actions {
  display: flex;
  gap: var(--space-2);
  margin-top: var(--space-2);
}

/* The old .ph textarea rules were replaced by the .ph-box :deep(.ProseMirror)
   block above (the editor carries the .ph class, but its styles live under
   the .ph-box scope there). Mobile keeps the 16px anti-zoom pin. */

/* /compact chip */
.compact-chip {
  background: none;
  border: 1px solid var(--line);
  border-radius: var(--radius-xs);
  color: var(--color-warning);
  font-family: var(--mono);
  font-size: var(--ui-font-size);
  padding: 0 4px;
  cursor: pointer;
  height: 19px;
  line-height: 17px;
  flex: none;
}
.compact-chip:hover { background: var(--panel2); }

/* Send button — circular accent icon. Always "send"; while running it enqueues
   (handled upstream). Interrupt is a separate Stop button so the two are never
   confused. */
.send {
  width: 32px;
  height: 32px;
  border-radius: 50%;
  background: var(--color-accent);
  color: var(--color-text-on-accent); /* white on accent — readable in light and dark */
  border: none;
  box-shadow: var(--shadow-xs);
  padding: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  cursor: pointer;
  flex-shrink: 0;
  margin-left: var(--space-2);
  transition: background 0.25s ease, transform 0.12s ease;
  position: relative;
}

.send:hover {
  background: var(--color-accent-hover);
}

.send:active {
  transform: scale(0.92);
}

.send:disabled {
  cursor: not-allowed;
  opacity: 0.88;
}

.send:disabled:active {
  transform: none;
}

/* Spinner-on-accent: recolor the ring so the arc reads on the accent fill.
   Spinner.vue styles are scoped, so pierce them with :deep(). */
.send.is-starting :deep(.ui-spinner) {
  color: var(--color-text-on-accent);
}

.send.is-starting :deep(.ui-spinner__track) {
  stroke: rgba(255, 255, 255, 0.32);
}

.send svg {
  flex: none;
}

/* Stop button — sibling of Send, shown only while running. Red at rest so the
   destructive action is easy to spot; fills solid danger on hover. Kept softer
   than the accent Send so Send stays the primary action. */
.stop {
  width: 32px;
  height: 32px;
  border-radius: 50%;
  background: var(--color-danger-soft);
  color: var(--color-danger);
  border: 1px solid var(--color-danger-bd);
  box-shadow: var(--shadow-xs);
  padding: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  cursor: pointer;
  flex-shrink: 0;
  margin-left: var(--space-2);
  transition: background 0.16s ease, color 0.16s ease, border-color 0.16s ease, transform 0.12s ease;
}
.stop:hover {
  background: var(--color-danger);
  color: var(--color-text-on-accent);
  border-color: var(--color-danger);
}
.stop:active {
  transform: scale(0.92);
}
.stop svg {
  flex: none;
}

/* Bottom toolbar */
.toolbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 6px 8px 8px;
  position: relative;
}

.toolbar-left,
.toolbar-right {
  display: flex;
  align-items: center;
  gap: 2px;
  min-width: 0;
  overflow: hidden;
}

/* Permission pill */
.perm-pill {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 2px 7px;
  border-radius: 6px;
  font-size: var(--ui-font-size);
  color: var(--color-text);
  cursor: pointer;
  user-select: none;
  transition: background 0.1s, color 0.15s;
  font-family: var(--sans);
}
.perm-pill:hover {
  background: var(--color-surface-sunken);
}
.perm-pill.open {
  background: var(--color-accent-soft);
}
.perm-pill.perm-manual {
  color: var(--dim);
}
.perm-pill.perm-yolo {
  color: var(--color-warning);
}
.perm-pill.perm-auto {
  color: var(--color-danger);
}

/* Context group — circular ring. Focusable for keyboard / switch access to its
   aria-label and tooltip (see template), so it needs a focus ring. */
.ctx-group {
  display: flex;
  align-items: center;
  gap: 4px;
  flex-shrink: 0;
  padding: 2px 4px;
  border-radius: var(--radius-xs);
}
.ctx-group:focus-visible {
  outline: 2px solid var(--color-accent);
  outline-offset: 2px;
}

/* Model pill */
.model-pill {
  display: inline-flex;
  align-items: center;
  gap: 3px;
  padding: 2px 7px;
  border-radius: 6px;
  font-size: var(--ui-font-size);
  line-height: 16px;
  color: var(--dim);
  cursor: pointer;
  user-select: none;
  transition: background 0.1s;
  position: relative;
  overflow: hidden;
}
.model-pill:hover {
  background: var(--color-surface-sunken);
  color: var(--color-text);
}
.model-pill.open {
  background: var(--color-accent-soft);
}
.model-pill b {
  font-weight: 500;
  color: var(--color-text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  min-width: 0;
  max-width: 280px;
}
.model-pill .think-suffix {
  color: var(--color-accent);
  font-weight: 500;
  flex-shrink: 0;
}
.model-pill .cv {
  color: var(--faint);
  flex: none;
}
.model-pill:hover .cv,
.model-pill.open .cv {
  color: var(--color-accent-hover);
}

/* Model dropdown — anchored to the toolbar right edge */
.model-dropdown {
  position: absolute;
  bottom: calc(100% + 4px);
  right: 10px;
  z-index: var(--z-dropdown);
  min-width: 200px;
  background: var(--color-surface-raised);
  border: 1px solid var(--color-line);
  border-radius: var(--radius-lg);
  box-shadow: var(--shadow-sm);
  padding: 5px;
  display: flex;
  flex-direction: column;
  gap: 1px;
}

.md-section {
  padding: 4px 7px 2px;
  font-size: var(--ui-font-size);
  color: var(--muted);
  text-transform: uppercase;
  letter-spacing: 0.04em;
  font-weight: 500;
}

.md-row {
  display: flex;
  align-items: center;
  gap: 7px;
  width: 100%;
  background: none;
  border: none;
  cursor: pointer;
  font-family: var(--mono);
  font-size: var(--ui-font-size);
  color: var(--color-text);
  padding: 5px 7px;
  border-radius: 6px;
  text-align: left;
}
.md-row:hover { background: var(--color-surface-sunken); }
.md-row:disabled {
  cursor: default;
  opacity: 0.58;
}
.md-row:disabled:hover { background: none; }
.md-row.is-current { color: var(--color-text); background: var(--color-accent-soft); }
.md-row.is-on { color: var(--color-accent); }
.md-note {
  margin-left: auto;
  color: var(--muted);
  font-size: var(--ui-font-size-xs);
}

.md-row-more {
  color: var(--color-accent);
  font-weight: 500;
}
.md-row-more:hover {
  background: var(--color-accent-soft);
}

.md-check {
  width: 14px;
  flex: none;
  color: var(--color-accent);
  font-weight: 500;
  display: flex;
  justify-content: center;
}

.md-name {
  flex: 1;
}
.md-provider {
  color: var(--muted);
  font-size: var(--ui-font-size-xs);
  flex: none;
}
.md-star {
  color: var(--star);
  flex: none;
  margin-left: auto;
}

.md-divider {
  height: 1px;
  background: var(--line);
  margin: 3px 0;
}

/* Thinking level segmented control — sits inside the model dropdown. */
.md-thinking {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 7px;
  border-radius: var(--radius-sm);
}
.md-thinking .md-name {
  font-family: var(--mono);
  font-size: var(--ui-font-size);
  color: var(--color-text);
  flex: none;
}
.md-thinking .md-note {
  margin-left: auto;
}
.effort-segments {
  margin-left: auto;
  display: inline-flex;
  align-items: center;
  gap: 1px;
  padding: 2px;
  background: var(--color-surface-sunken);
  border: 1px solid var(--color-line);
  border-radius: var(--radius-md);
}
.effort-seg {
  appearance: none;
  border: none;
  background: none;
  cursor: pointer;
  font-family: var(--mono);
  font-size: var(--ui-font-size-xs);
  line-height: 1;
  color: var(--color-text-muted);
  padding: 4px 9px;
  border-radius: var(--radius-sm);
  white-space: nowrap;
  transition: background 0.12s, color 0.12s, box-shadow 0.12s;
}
.effort-seg:hover:not(:disabled):not(.is-active) {
  background: var(--color-surface-raised);
  color: var(--color-text);
}
.effort-seg:focus-visible {
  outline: 2px solid var(--color-accent);
  outline-offset: -2px;
}
.effort-seg.is-active {
  background: var(--color-accent);
  color: var(--color-text-on-accent);
  box-shadow: var(--shadow-xs);
  font-weight: 500;
}
.effort-seg:disabled {
  cursor: default;
}
.md-thinking.is-readonly .effort-segments {
  opacity: 0.62;
}
.md-cache-note {
  /* width:0 + min-width:100% — the note never widens the shrink-to-fit
     dropdown, but always fills its width and wraps there naturally. */
  width: 0;
  min-width: 100%;
  padding: 2px 7px 4px;
  color: var(--muted);
  font-size: var(--ui-font-size-xs);
  line-height: 1.4;
}
.md-thinking.is-readonly .effort-seg.is-active {
  background: var(--color-surface-raised);
  color: var(--color-text-muted);
  box-shadow: none;
}

/* Permission dropdown — anchored to the toolbar left side */
.perm-dropdown {
  position: absolute;
  bottom: calc(100% + 4px);
  left: 10px;
  z-index: var(--z-dropdown);
  min-width: 220px;
  max-width: 280px;
  background: var(--color-surface-raised);
  border: 1px solid var(--color-line);
  border-radius: var(--radius-lg);
  box-shadow: var(--shadow-sm);
  padding: 5px;
  display: flex;
  flex-direction: column;
  gap: 1px;
}

.pd-row {
  display: flex;
  align-items: flex-start;
  gap: 7px;
  width: 100%;
  background: none;
  border: none;
  cursor: pointer;
  padding: 6px 7px;
  border-radius: 6px;
  text-align: left;
}
.pd-row:hover { background: var(--color-surface-sunken); }
.pd-row.is-current { background: var(--color-accent-soft); }

.pd-check {
  width: 14px;
  flex: none;
  color: var(--color-accent);
  font-weight: 500;
  display: flex;
  justify-content: center;
  margin-top: 1px;
}

.pd-info {
  display: flex;
  flex-direction: column;
  gap: 2px;
  flex: 1;
  min-width: 0;
}

.pd-name {
  font-family: var(--sans);
  font-size: var(--ui-font-size);
  font-weight: 500;
}

.pd-desc {
  font-family: var(--sans);
  font-size: var(--ui-font-size);
  color: var(--muted);
  line-height: 1.4;
}

/* Toggle pills (Thinking / Plan) */
/* Modes selector (plan / goal / swarm) — replaces the old plan pill + badges.
   z-index lifts the whole control (incl. its upward-opening menu) above the
   composer input row, which otherwise paints over the menu. */
.modes { position: relative; display: inline-flex; z-index: var(--z-sticky); }
.mode-pill {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 2px 9px;
  border: none;
  background: none;
  border-radius: 6px;
  font-size: var(--ui-font-size);
  font-family: var(--sans);
  color: var(--color-text);
  cursor: pointer;
  user-select: none;
  transition: background 0.1s, color 0.15s;
}
.mode-pill:hover { background: var(--color-surface-sunken); }
.mode-pill.on { background: var(--color-accent-soft); color: var(--color-accent-hover); }
.mode-pill.open { background: var(--color-accent-soft); }
.mode-label { flex: none; }
.mode-tag {
  flex: none;
  font-family: var(--mono);
  font-size: calc(var(--ui-font-size) - 3px);
  color: var(--color-accent-hover);
  background: var(--bg);
  border: 1px solid var(--color-accent-bd);
  border-radius: 999px;
  padding: 0 6px;
  line-height: 16px;
}
.mode-dot { width: 6px; height: 6px; border-radius: 50%; background: var(--color-accent); flex: none; }

.modes-menu {
  position: fixed;
  z-index: var(--z-dropdown);
  min-width: 220px;
  background: var(--color-surface-raised);
  border: 1px solid var(--color-line);
  border-radius: var(--radius-lg);
  box-shadow: var(--shadow-sm);
  padding: 4px;
}
.mode-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
  width: 100%;
  padding: 7px 10px;
  border: none;
  background: none;
  border-radius: 6px;
  cursor: pointer;
  font-family: var(--sans);
  text-align: left;
}
.mode-row:hover:not(:disabled) { background: var(--panel2); }
.mode-row:disabled { cursor: not-allowed; opacity: 0.45; }
.mode-row-name { font-size: var(--ui-font-size-sm); color: var(--color-text); }
.mode-row-not-supported {
  margin-left: auto;
  font-size: var(--ui-font-size-xs);
  color: var(--muted);
}
.mode-row.on .mode-row-name { color: var(--color-accent-hover); font-weight: 500; }
.mode-row-meta { font-family: var(--mono); font-size: calc(var(--ui-font-size) - 3px); color: var(--muted); }
.mode-row:disabled .mode-row-meta { color: var(--faint); }
.mode-switch {
  flex: none;
  width: 34px;
  height: 19px;
  border-radius: 999px;
  background: var(--panel2);
  border: 1px solid var(--line);
  position: relative;
  transition: background 0.15s;
}
.mode-switch.on { background: var(--color-accent); border-color: var(--color-accent); }
.mode-knob {
  position: absolute;
  top: 1px;
  left: 1px;
  width: 15px;
  height: 15px;
  border-radius: 50%;
  background: var(--bg);
  box-shadow: var(--shadow-xs);
  transition: transform 0.15s;
}
.mode-switch.on .mode-knob { transform: translateX(15px); }

.mode-row-goal {
  flex-wrap: wrap;
  cursor: default;
  padding: 0;
  gap: 0;
}
.mode-row-goal:hover { background: transparent; }
.mode-row-main {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
  width: 100%;
  padding: 7px 10px;
  border: none;
  background: none;
  border-radius: 6px;
  cursor: pointer;
  font-family: var(--sans);
  text-align: left;
}
.mode-row-main:hover { background: var(--panel2); }
.mode-row-goal.on .mode-row-main .mode-row-name { color: var(--color-accent-hover); font-weight: 500; }
.mode-row-actions {
  display: flex;
  gap: 6px;
  flex: 1 1 100%;
  justify-content: flex-end;
}
.mode-row-action {
  padding: 3px 8px;
  border-radius: var(--radius-sm);
  border: 1px solid var(--line);
  background: var(--panel);
  color: var(--color-text);
  font-size: calc(var(--ui-font-size) - 3px);
  cursor: pointer;
}
.mode-row-action:hover:not(:disabled) { background: var(--panel2); }
.mode-row-action:disabled { opacity: 0.5; cursor: default; }
.mode-row-input {
  flex: 1;
  min-width: 0;
  padding: 4px 8px;
  border-radius: var(--radius-sm);
  border: 1px solid var(--line);
  background: var(--bg);
  color: var(--color-text);
  font-size: var(--ui-font-size-xs);
}

/* ---- Narrow composer toolbar ----------------------------------------------
   Below a wide desktop the chat column can be narrower than the full toolbar
   needs — with the sidebar open on a small window, and on phones. The desktop
   toolbar shows every control on one row and toolbar-left / toolbar-right are
   overflow:hidden, so without shedding ink the row clips its own content. The
   context ring stays visible at every width (it is the live context-pressure
   signal; the exact numbers live in its tooltip), the model name truncates
   earlier, and the permission label is capped so the ring and the send button
   are never squeezed out. Mobile (≤640px) additionally hides perm / modes via
   the rules below (those live in MobileSettingsSheet there). */
@media (max-width: 980px) {
  /* Model name was budgeted for a wide card (280px); trim it so the ring and
     send button are not squeezed out on a narrow column. */
  .model-pill b {
    max-width: 130px;
  }
  /* Permission label is short (manual/yolo/auto); cap it defensively so a
     longer label can never push the toolbar past its container. */
  .perm-pill {
    max-width: 104px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
}

/* ---- Mobile composer (prototype): round attach + rounded panel input +
       round blue send with a soft shadow. The .cin container loses its border
       and acts as a flex row; the textarea itself becomes the pill input. ---- */
@media (max-width: 640px) {
  .composer {
    padding:
      9px
      var(--dock-inline-right, max(12px, var(--safe-right)))
      max(24px, var(--safe-bottom))
      var(--dock-inline-left, max(12px, var(--safe-left)));
  }
  .composer-card {
    border-radius: var(--radius-xl);
    max-width: 100%;
  }
  .input-row {
    gap: 6px;
    min-width: 0;
  }
  /* Send → 36px round (hide the SVG arrow, show only the ::after glyph) */
  .send {
    width: 36px;
    height: 36px;
    min-width: 36px;
    padding: 0;
    border-radius: 50%;
    font-size: 0;
    align-self: flex-end;
    position: relative;
  }
  .send svg {
    display: none;
  }
  .send::after {
    content: "↑";
    /* Fixed icon glyph size — not part of the UI font scale. */
    font-size: 17px;
    line-height: 1;
    color: var(--bg);
  }
  /* Stop → 36px round "■" glyph to match the mobile Send sizing. */
  .stop {
    width: 36px;
    height: 36px;
    min-width: 36px;
    padding: 0;
    border-radius: 50%;
    font-size: 0;
    align-self: flex-end;
    position: relative;
  }
  .stop svg {
    display: none;
  }
  .stop::after {
    content: "■";
    /* Fixed icon glyph size — not part of the UI font scale. */
    font-size: 14px;
    line-height: 1;
  }

  /* Mobile toolbar: hide secondary controls; attach / context ring / model /
     send stay visible. Permission + plan move into the MobileSettingsSheet.
     The context ring stays at every width by design — it is the live
     context-pressure signal on a phone (the exact numbers live in the ring's
     tooltip). The /compact chip also stays so compaction is one tap away at
     ≥80% usage. */
  .perm-pill,
  .modes {
    display: none;
  }

  /* Model dropdown on mobile → anchored right with padding */
  .model-dropdown {
    right: 10px;
    left: auto;
    min-width: 180px;
    max-width: calc(100vw - 24px);
  }

  /* Bump mobile font sizes +2px and pin input at 16px to prevent iOS zoom.
     Height (min 36px / max one quarter of the viewport) is inherited from the
     base .ProseMirror rule so the box auto-grows the same way on touch and
     desktop. */
  .ph-box :deep(.ProseMirror) {
    /* Pinned at 16px to prevent iOS auto-zoom on focus (not part of UI font scale). */
    font-size: 16px;
  }
  .model-pill,
  .attach-btn {
    font-size: var(--ui-font-size);
  }
  .toolbar {
    gap: 6px;
    min-width: 0;
  }
  .toolbar-left,
  .toolbar-right {
    min-width: 0;
  }
  .model-pill {
    max-width: min(52vw, 220px);
  }
  .model-pill b {
    max-width: min(40vw, 170px);
  }
  .md-row {
    font-size: var(--ui-font-size);
  }
  .md-section {
    font-size: var(--ui-font-size);
  }
  .md-thinking {
    flex-wrap: wrap;
    row-gap: 6px;
  }
  .md-thinking .effort-segments {
    margin-left: 0;
    width: 100%;
    justify-content: space-between;
  }
  .md-thinking .effort-seg {
    flex: 1;
    padding: 5px 6px;
  }
  .pd-name {
    font-size: var(--ui-font-size);
  }
  .pd-desc {
    font-size: var(--ui-font-size);
  }
}

/* NOTE: Composer overrides live in src/style.css (global), NOT here. Scoped
   `.cin` rules did NOT reliably win the cascade against the base `.cin` (the
   input stayed square + mono), so they were moved to the global sheet where they
   apply. */
</style>
