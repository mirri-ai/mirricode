// apps/mirri-web/src/composables/useDetailPanel.ts
// Unified right-side detail layer. Only one detail is open at a time.

import { computed, ref, watch, type Ref } from 'vue';
import type { AgentMember, ToolDiffTarget, TurnBlock } from '../types';
import type { DetailTarget } from './useFilePreview';
import type { useMirriWebClient } from './useMirriWebClient';
import type { SwarmMember } from './swarmGroups';
import { buildEditDiffLines, extractEditPath, findToolCallById } from '../lib/toolDiff';
import { toolLabel } from '../lib/toolMeta';
import { toAgentMember } from './messagesToTurns';
import { memberBlocksFromMessages } from '../lib/memberBlocks';
import { getMirriWebApi } from '../api';
import { clampPanelWidth, panelMaxWidth, useViewportWidth } from './useViewportWidth';

type MirriWebClient = ReturnType<typeof useMirriWebClient>;

const PREVIEW_WIDTH_KEY = 'mirri-web.file-preview-width';
export const PREVIEW_MIN = 320;

export interface UseDetailPanelOptions {
  client: MirriWebClient;
  /** Mirrored sidebar width (px) so the preview max-width stays within the viewport. */
  sideWidth: Ref<number>;
  /** Shared owner of the single right-side slot (also written by useFilePreview). */
  detailTarget: Ref<DetailTarget | null>;
  /** Closes the file preview; injected to avoid a composable-to-composable import cycle. */
  closeFilePreview: () => void;
}

export function useDetailPanel({
  client,
  sideWidth,
  detailTarget,
  closeFilePreview,
}: UseDetailPanelOptions) {
  // ---------------------------------------------------------------------------
  // Panel width helpers
  // ---------------------------------------------------------------------------
  const { viewportWidth } = useViewportWidth();

  // Area available to the right of the sidebar (conversation + preview).
  const previewAreaWidth = computed(() =>
    Math.max(0, viewportWidth.value - sideWidth.value),
  );

  // Largest preview width that still leaves the conversation pane usable.
  const previewMax = computed(() =>
    panelMaxWidth(previewAreaWidth.value, PREVIEW_MIN, PREVIEW_MIN),
  );

  function clampPreviewWidth(width: number): number {
    return clampPanelWidth(Math.round(width), PREVIEW_MIN, previewMax.value);
  }

  function defaultPreviewWidth(): number {
    return clampPreviewWidth(previewAreaWidth.value / 2);
  }

  const previewDefaultWidth = computed(() => defaultPreviewWidth());
  const previewWidth = ref(previewDefaultWidth.value);
  // Rendered width, clamped to the current cap so a restored width or a window
  // shrink can never push the resize handle off-screen.
  const previewPanelWidth = computed(() =>
    clampPanelWidth(previewWidth.value, PREVIEW_MIN, previewMax.value),
  );

  // ---------------------------------------------------------------------------
  // Thinking panel
  // ---------------------------------------------------------------------------
  const thinkingTarget = ref<{ turnId: string; blockIndex: number } | null>(null);

  const thinkingPanelText = computed<string | null>(() => {
    const target = thinkingTarget.value;
    if (!target) return null;
    const turn = client.turns.value.find((tn) => tn.id === target.turnId);
    const blk = turn?.blocks?.[target.blockIndex];
    return blk?.kind === 'thinking' ? blk.thinking : null;
  });

  const thinkingVisible = computed(() => thinkingPanelText.value !== null);

  function openThinkingPanel(target: { turnId: string; blockIndex: number }): void {
    const current = thinkingTarget.value;
    if (current && current.turnId === target.turnId && current.blockIndex === target.blockIndex) {
      thinkingTarget.value = null;
      if (detailTarget.value === 'thinking') detailTarget.value = null;
      return;
    }
    detailTarget.value = 'thinking';
    thinkingTarget.value = target;
  }

  function closeThinkingPanel(): void {
    thinkingTarget.value = null;
    if (detailTarget.value === 'thinking') detailTarget.value = null;
  }

  // ---------------------------------------------------------------------------
  // Compaction summary panel
  // ---------------------------------------------------------------------------
  const compactionTarget = ref<{ turnId: string } | null>(null);

  const compactionPanelText = computed<string | null>(() => {
    const target = compactionTarget.value;
    if (!target) return null;
    const turn = client.turns.value.find((tn) => tn.id === target.turnId);
    return turn?.role === 'compaction' && turn.text ? turn.text : null;
  });

  const compactionPanelVisible = computed(() => compactionPanelText.value !== null);

  function openCompactionPanel(target: { turnId: string }): void {
    if (compactionTarget.value?.turnId === target.turnId) {
      compactionTarget.value = null;
      if (detailTarget.value === 'compaction') detailTarget.value = null;
      return;
    }
    detailTarget.value = 'compaction';
    compactionTarget.value = target;
  }

  function closeCompactionPanel(): void {
    compactionTarget.value = null;
    if (detailTarget.value === 'compaction') detailTarget.value = null;
  }

  // ---------------------------------------------------------------------------
  // Subagent detail panel (supervision L1)
  // ---------------------------------------------------------------------------
  // Sourced from the live subagent task (not the message flow), so the panel
  // keeps streaming a still-running subagent's `outputLines`. `agentTarget`
  // holds the delegation (spawning tool-call id) plus the entry member task
  // id; `agentPanelSelectedId` is the member-selector selection (null = the
  // entry member). The open entry points are the delegation card (keyed by its
  // tool-call id) and a background subagent chip in the dock (keyed by the
  // task id) — both resolve to a task id here.
  const agentTarget = ref<{ toolCallId: string; subagentId: string } | null>(null);
  const agentPanelSelectedId = ref<string | null>(null);

  function resolveSubagentId(target: string): string | undefined {
    const tasks = client.activeAppTasks.value;
    const task =
      tasks.find((tk) => tk.id === target) ?? tasks.find((tk) => tk.parentToolCallId === target);
    if (task) return task.id;
    // Same fallback as resolveAgentTaskId: a synthesized subagent task (missed
    // spawn) has no parentToolCallId; if exactly one exists, open it.
    const unmapped = tasks.filter((tk) => tk.kind === 'subagent' && !tk.parentToolCallId);
    if (unmapped.length === 1) return unmapped[0]!.id;
    return undefined;
  }

  /** The delegation's member set behind a spawning tool call (live view). */
  const agentPanelMembers = computed<SwarmMember[]>(() => {
    const target = agentTarget.value;
    if (!target) return [];
    return client.swarmMembersByToolCallId.value.get(target.toolCallId) ?? [];
  });

  const agentPanelMember = computed<AgentMember | null>(() => {
    const target = agentTarget.value;
    if (!target) return null;
    const id = agentPanelSelectedId.value ?? target.subagentId;
    const task = client.activeAppTasks.value.find((tk) => tk.id === id);
    return task ? toAgentMember(task) : null;
  });

  const agentPanelVisible = computed(() => agentPanelMember.value !== null);

  function selectPanelMember(id: string): void {
    agentPanelSelectedId.value = id;
  }

  function stopPanelMember(id: string): void {
    void client.cancelTask(id);
  }

  // ---------------------------------------------------------------------------
  // Member history (supervision A2): the read face for a member that is no
  // longer live. Live members keep streaming from the task store; a settled
  // member's narrative comes from its own journal via the member-scoped
  // `message.list`, normalized to the main transcript's content blocks
  // (ADR-009 — no flattening layer). Honest absence: a fetch failure or an
  // empty journal renders nothing — never a fabricated log.
  // ---------------------------------------------------------------------------
  const agentPanelHistory = ref<TurnBlock[] | null>(null);
  const agentPanelHistoryLoading = ref(false);

  // The journal read is keyed by the member's identity + settledness, NOT the
  // member object: agentPanelMember is re-created on every tasksBySession
  // refresh (each WS event clones the whole tasks map; the 1s poll rebuilds
  // the array), so watching the object would re-clear the history and
  // re-fetch the journal on every unrelated event — the settled member's
  // panel flickered between content, loading and blank (regression guard:
  // see test/use-terminal.test.ts "subagent detail-panel history loading").
  // A live member streams from the task store instead (null key), and the
  // running→settled transition re-keys to the member id and fetches once.
  watch(
    () => {
      const member = agentPanelMember.value;
      if (!member) return null;
      return member.status === 'running' ? null : member.id;
    },
    (key) => {
      void (async () => {
        agentPanelHistory.value = null;
        agentPanelHistoryLoading.value = false;
        if (key === null) return;
        const sessionId = client.activeSessionId.value;
        if (!sessionId) return;
        agentPanelHistoryLoading.value = true;
        try {
          const page = await getMirriWebApi().listMessages(sessionId, { agentId: key });
          // The panel moved on while the fetch was in flight — drop the result.
          if (
            agentPanelMember.value?.id !== key ||
            client.activeSessionId.value !== sessionId
          ) {
            return;
          }
          agentPanelHistory.value = memberBlocksFromMessages(page.items);
        } catch {
          agentPanelHistory.value = null;
        } finally {
          agentPanelHistoryLoading.value = false;
        }
      })();
    },
  );

  function openAgentPanel(target: string, memberId?: string): void {
    const subagentId = resolveSubagentId(target);
    if (!subagentId) return;
    if (agentTarget.value?.toolCallId === target) {
      // SDR-007: clicking a DIFFERENT member row of the same open card
      // switches the focused member — it must not toggle the panel closed.
      // Same-member clicks (and the head's Open, memberId undefined) keep
      // the toggle-close semantics.
      if (memberId !== undefined && memberId !== agentPanelSelectedId.value) {
        agentPanelSelectedId.value = memberId;
        return;
      }
      agentTarget.value = null;
      agentPanelSelectedId.value = null;
      if (detailTarget.value === 'agent') detailTarget.value = null;
      return;
    }
    agentTarget.value = { toolCallId: target, subagentId };
    // SDR-007: a member row click pre-selects that member in the panel; the
    // head's Open button keeps the previous null (selector shows) behavior.
    agentPanelSelectedId.value = memberId ?? null;
    detailTarget.value = 'agent';
  }

  function closeAgentPanel(): void {
    agentTarget.value = null;
    agentPanelSelectedId.value = null;
    if (detailTarget.value === 'agent') detailTarget.value = null;
  }

  // ---------------------------------------------------------------------------
  // Edit/Write tool-call diff preview
  // ---------------------------------------------------------------------------
  // Store only the tool id and re-derive the panel payload from the live tool
  // call in the session turns, so a panel opened while the tool is still
  // running keeps tracking its status / output / diff as they update.
  const toolDiffToolId = ref<string | null>(null);

  const toolDiffTarget = computed<ToolDiffTarget | null>(() => {
    const id = toolDiffToolId.value;
    if (!id) return null;
    const tool = findToolCallById(client.turns.value, id);
    if (!tool) return null;
    return {
      id,
      title: toolLabel(tool.name),
      path: extractEditPath(tool.arg),
      // On error the diff describes what was attempted, not what happened —
      // show the tool output (the failure reason) instead.
      lines: tool.status === 'error' ? null : buildEditDiffLines(tool),
      output: tool.output,
    };
  });

  const toolDiffVisible = computed(() => toolDiffTarget.value !== null);

  function openToolDiff(id: string): void {
    // Guard: the diff panel reads its payload from the MAIN transcript's tool
    // calls (findToolCallById over client.turns). A member's tool cards live
    // in the supervision panel's own block model — never in the transcript —
    // so opening the slot for one would strand a blank, close-less panel
    // (every v-else occupant requires its target). Until the diff data
    // source covers member blocks, an unknown id must not claim the slot.
    if (!findToolCallById(client.turns.value, id)) return;
    if (detailTarget.value === 'toolDiff' && toolDiffToolId.value === id) {
      closeToolDiff();
      return;
    }
    detailTarget.value = 'toolDiff';
    toolDiffToolId.value = id;
  }

  function closeToolDiff(): void {
    toolDiffToolId.value = null;
    if (detailTarget.value === 'toolDiff') detailTarget.value = null;
  }

  // ---------------------------------------------------------------------------
  // Diff detail layer (opened from the chat header git area)
  // ---------------------------------------------------------------------------
  const detailDiffMode = ref<'list' | 'detail'>('list');
  const detailDiffPath = ref<string | null>(null);

  function openDiffDetail(): void {
    if (detailTarget.value === 'diff') {
      closeDiffDetail();
      return;
    }
    detailTarget.value = 'diff';
    detailDiffMode.value = 'list';
    detailDiffPath.value = null;
    void client.loadGitStatus(client.activeSessionId.value);
  }

  function closeDiffDetail(): void {
    if (detailTarget.value === 'diff') detailTarget.value = null;
    detailDiffMode.value = 'list';
    detailDiffPath.value = null;
    client.clearFileDiff();
  }

  async function selectDiffFile(path: string): Promise<void> {
    detailDiffMode.value = 'detail';
    detailDiffPath.value = path;
    await client.loadFileDiff(path);
  }

  // ---------------------------------------------------------------------------
  // Terminal panel (opened from the chat header / mobile top bar)
  // ---------------------------------------------------------------------------
  // The xterm panel owns its own state via useTerminal (list → create/attach);
  // this layer only claims / releases the shared right-side slot. It is
  // excluded from the per-session snapshot restore on purpose: the terminal
  // is a process-bearing occupant (unlike the purely derived thinking /
  // compaction panels), matching the file/diff exclusion precedent — restoring
  // it would re-attach a live shell the user may not want running.
  function openTerminalPanel(): void {
    // No session scope → no terminal. Both entries are session-gated, but a
    // race (the session archived elsewhere between render and click) must not
    // strand an unrenderable target in the shared slot (on mobile that would
    // be a full-screen overlay with no close affordance).
    if (!client.activeSessionId.value) return;
    if (detailTarget.value === 'terminal') {
      closeTerminalPanel();
      return;
    }
    detailTarget.value = 'terminal';
  }

  function closeTerminalPanel(): void {
    if (detailTarget.value === 'terminal') detailTarget.value = null;
  }

  // ---------------------------------------------------------------------------
  // Side chat (BTW) — now rendered in the unified right-side detail layer.
  // ---------------------------------------------------------------------------
  async function openSideChatTab(prompt?: string): Promise<void> {
    // Empty-composer heal: `/btw [<question>]` from the new-session screen needs
    // a parent session before openSideChat can start a BTW sub-agent. Create one
    // in the active workspace (same path as the first prompt / a new-session
    // skill / goal), then open the side chat on it.
    if (!client.activeSessionId.value && client.activeWorkspaceId.value) {
      await client.startSessionAndOpenSideChat(client.activeWorkspaceId.value, prompt);
    } else {
      await client.openSideChat(prompt);
    }
    detailTarget.value = 'btw';
  }

  function closeSideChat(): void {
    client.closeSideChat();
    if (detailTarget.value === 'btw') detailTarget.value = null;
  }

  // Only hides the right-side BTW panel; the side-chat target is per-session and
  // preserved so switching back to a session restores its BTW transcript.
  function hideSideChatPanel(): void {
    if (detailTarget.value === 'btw') detailTarget.value = null;
  }

  const btwVisible = computed(() => client.sideChatVisible.value);

  /** Any occupant of the shared right-side slot. */
  const sidePanelVisible = computed(
    () =>
      detailTarget.value !== null &&
      (detailTarget.value !== 'thinking' || thinkingVisible.value) &&
      (detailTarget.value !== 'compaction' || compactionPanelVisible.value) &&
      (detailTarget.value !== 'agent' || agentPanelVisible.value) &&
      (detailTarget.value !== 'toolDiff' || toolDiffVisible.value) &&
      (detailTarget.value !== 'btw' || btwVisible.value),
  );

  /** True while the panel's resize handle is being dragged — the width
      transition is disabled so the panel follows the pointer 1:1. */
  const panelDragging = ref(false);

  // ---------------------------------------------------------------------------
  // Per-session panel snapshot (in-memory only). Switching sessions still closes
  // the right-side detail layer, but for the transient panels whose content is
  // re-derived from the session's turns (thinking / compaction / agent /
  // toolDiff) or already stored per session (btw), we remember which one was
  // open and restore it when the user switches back.
  //
  // File preview ('file') and git diff ('diff') are intentionally excluded:
  // their content is tied to the active session's cwd / git state and is
  // re-fetched on demand, so restoring them across sessions would be ambiguous.
  // ---------------------------------------------------------------------------
  type PanelSnapshot =
    | { kind: 'thinking'; turnId: string; blockIndex: number }
    | { kind: 'compaction'; turnId: string }
    | { kind: 'agent'; toolCallId: string; subagentId: string }
    | { kind: 'toolDiff'; toolId: string }
    | { kind: 'btw' };

  const snapshotBySession = ref<Record<string, PanelSnapshot>>({});

  function captureSnapshot(): PanelSnapshot | null {
    switch (detailTarget.value) {
      case 'thinking':
        return thinkingTarget.value ? { kind: 'thinking', ...thinkingTarget.value } : null;
      case 'compaction':
        return compactionTarget.value ? { kind: 'compaction', ...compactionTarget.value } : null;
      case 'agent':
        return agentTarget.value ? { kind: 'agent', ...agentTarget.value } : null;
      case 'toolDiff':
        return toolDiffToolId.value ? { kind: 'toolDiff', toolId: toolDiffToolId.value } : null;
      case 'btw':
        return { kind: 'btw' };
      default:
        return null;
    }
  }

  function restoreSnapshot(snap: PanelSnapshot | undefined): void {
    if (!snap) return;
    switch (snap.kind) {
      case 'thinking':
        thinkingTarget.value = { turnId: snap.turnId, blockIndex: snap.blockIndex };
        detailTarget.value = 'thinking';
        break;
      case 'compaction':
        compactionTarget.value = { turnId: snap.turnId };
        detailTarget.value = 'compaction';
        break;
      case 'agent':
        agentTarget.value = { toolCallId: snap.toolCallId, subagentId: snap.subagentId };
        agentPanelSelectedId.value = null;
        detailTarget.value = 'agent';
        break;
      case 'toolDiff':
        toolDiffToolId.value = snap.toolId;
        detailTarget.value = 'toolDiff';
        break;
      case 'btw':
        // Only re-open the BTW panel if this session still has a live side chat;
        // the snapshot can outlive it if the user closed the side chat explicitly.
        if (client.sideChatVisible.value) detailTarget.value = 'btw';
        break;
    }
  }

  // Escape closes whichever transient right-side detail panel is open.
  function closeOpenSidePanel(): boolean {
    if (detailTarget.value === 'thinking' && thinkingVisible.value) { closeThinkingPanel(); return true; }
    if (detailTarget.value === 'compaction' && compactionPanelVisible.value) { closeCompactionPanel(); return true; }
    if (detailTarget.value === 'agent' && agentPanelVisible.value) { closeAgentPanel(); return true; }
    if (detailTarget.value === 'toolDiff' && toolDiffVisible.value) { closeToolDiff(); return true; }
    if (detailTarget.value === 'file') { closeFilePreview(); return true; }
    if (detailTarget.value === 'diff') { closeDiffDetail(); return true; }
    if (detailTarget.value === 'terminal') { closeTerminalPanel(); return true; }
    if (detailTarget.value === 'btw') { closeSideChat(); return true; }
    return false;
  }

  watch(client.activeSessionId, (newId, oldId) => {
    // Remember the leaving session's open panel (restorable kinds only) before
    // the close calls below wipe the target refs.
    if (oldId) {
      const snap = captureSnapshot();
      if (snap) snapshotBySession.value[oldId] = snap;
      else delete snapshotBySession.value[oldId];
    }
    // Close everything for the incoming session (unchanged behavior).
    closeFilePreview();
    closeThinkingPanel();
    closeCompactionPanel();
    closeAgentPanel();
    closeToolDiff();
    closeDiffDetail();
    closeTerminalPanel();
    hideSideChatPanel();
    // Restore the entering session's panel, if it had one.
    if (newId) {
      restoreSnapshot(snapshotBySession.value[newId]);
    }
  });

  return {
    PREVIEW_WIDTH_KEY,
    PREVIEW_MIN,
    previewDefaultWidth,
    previewMax,
    previewWidth,
    previewPanelWidth,
    thinkingPanelText,
    thinkingVisible,
    openThinkingPanel,
    closeThinkingPanel,
    compactionPanelText,
    compactionPanelVisible,
    openCompactionPanel,
    closeCompactionPanel,
    agentPanelMember,
    agentPanelMembers,
    agentPanelSelectedId,
    agentPanelHistory,
    agentPanelHistoryLoading,
    selectPanelMember,
    stopPanelMember,
    agentPanelVisible,
    openAgentPanel,
    closeAgentPanel,
    toolDiffTarget,
    toolDiffVisible,
    openToolDiff,
    closeToolDiff,
    detailDiffMode,
    detailDiffPath,
    openDiffDetail,
    closeDiffDetail,
    selectDiffFile,
    openTerminalPanel,
    closeTerminalPanel,
    btwVisible,
    openSideChatTab,
    closeSideChat,
    hideSideChatPanel,
    sidePanelVisible,
    panelDragging,
    closeOpenSidePanel,
  };
}
