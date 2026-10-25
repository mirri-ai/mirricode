# TUI Trust Surface — Architecture

> Domain architecture of `tui-architecture.md` (cross-domain). Requirement source: `docs/specs/tui/trust-requirements.md`. Parity row: C in `docs/specs/tui-context.md`.

## 1. State slice

```rust
// app.rs slice
struct TrustState {
    permission_mode: PermissionMode,   // Manual | Auto | Yolo
    plan_mode: bool,
    modal: ModalCoordinator,          // single active modal + FIFO queue (see §3)
    session_approvals: SessionApprovals, // action-key -> approved-for-session set
}
enum Modal {
    Approval { request: ApprovalRequest, choice: ApprovalChoice, feedback: FeedbackInput },
    Question { form: QuestionForm, answers: Vec<Answer>, page: QuestionPage },
}
```

- Permission/plan mode changes are ordinary msgs; the reducer applies them symmetrically (tighten/loosen both take effect immediately for new requests).

## 2. Reverse-RPC and the modal queue

- The engine's approval/question requests arrive as engine events; `msg.rs` translates them into `ApprovalRequested`/`QuestionAsked` msgs.
- `ModalCoordinator` (mirroring the TS TUI's reverse-rpc design): at most one modal is active; further requests wait in a FIFO; **approval and question modals are mutually exclusive** — a question never replaces an approval and vice versa.
- `SessionApprovals`: when the user approves-for-session, the action key is recorded; subsequent requests with the same key are answered automatically **without entering the modal queue** (the reducer short-circuits them into an auto-approve command plus a notice marker).

## 3. Approval panel model

- `ApprovalPanelModel` (pure derivation from the engine request): headline (tool-family word map), evidence blocks (shell/diff/file/url/search/invocation/…; deduped, capped at 5, order preserved), danger flag, choices.
- Danger detection: a pure pattern table (the TS TUI's 8 classes), unit-tested with true/false fixture pairs — same judgment rules as the TS TUI (the quoted-`rm -rf` case is a fixture).
- Feedback input is an inline sub-editor state; `requires_feedback` choices activate it.
- Full-screen preview (Ctrl-E) and expand (Ctrl-O) are view modes of the same modal state; preview opens as a nested render layer over the modal (dialog stack depth ≤ 2 here).

## 4. Question dialog model

- `QuestionForm`: ordered questions, each single/multi/other; `page` walks questions then a review page; submit emits the answers payload; unanswered questions block submit with a flag (R-level requirement).
- Esc dismisses with empty answers (the engine contract's cancellation response).

## 5. Plan mode

- Plan approval rides the same `Modal::Approval` machinery with the plan-exit headline and engine-provided choices; the plan body renders through the conversation domain's markdown pipeline (reuse, not duplication).

## 6. Notifications

- Panel appearance emits a notify command (once per request id) through the shared `NotificationSink` port (interaction domain §5).

## 7. Test anchors

| Requirement | Layer | Anchor |
|---|---|---|
| Headline/block-set/choice rendering per tool family | L2 | per-family snapshots incl. danger flag forms |
| Choice semantics (once/session/reject/feedback) | L1 | reducer over choice msgs |
| Session-approval auto-apply | L1 + L3 | reducer auto-answer; loop-level: second identical request never opens a panel |
| Modal mutual exclusion + FIFO order | L1 + L3 | coordinator unit tests; scripted burst through the loop |
| Question review-page flow, unanswered flag | L1 + L2 | reducer + snapshots |
| Danger detection table | L1 | true/false fixture pairs |
| Esc cancels leave editor untouched | L3 | event-driven round-trip, screen assertion |
| Notification per request | L1 | sink recording |
