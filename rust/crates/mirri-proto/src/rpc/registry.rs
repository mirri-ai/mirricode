//! Static RPC method registry — the method-surface fact source (M1.1 + M1.2-a).
//!
//! Every method name either backend dispatches is registered here with its
//! availability, domain, and M1.2-a disposition:
//!
//! - **Embedded** — dispatched by `mirri-embedded`'s `call_method` match
//!   (95 methods as of M1.1; the registry records the dotted *primary*
//!   name — the camelCase wire aliases the dispatch also accepts
//!   (`createSession` for `session.create`, …) are a dispatch concern and
//!   join the registry with the M3 embedded migration).
//! - **Server** — the serve wire of `mirri-server` (transport registry in
//!   `mirri-server::rpc::build_transport_registry` behind the
//!   [`EngineAdapter`](mirri_transport::adapter::EngineAdapter) seam;
//!   58 methods).
//! - **Both** — the intersection (35 as of M5-3b: the M1.2-a 34 = the
//!   M1.1 31 + the three rows that gained the server side via rename
//!   folding `session.beginCompaction`, `agent.activateSkill`, `btw.start`
//!   — plus the M5-3b `core.echo` drill row).
//! - **Unimplemented** — in the v2 contract (per the D4 appendix) but
//!   dispatched by neither backend yet; gap-filling is out of program
//!   scope, so they are registered as facts only.
//!
//! M1.2-a executed the D4 adjudications (`plan/rust-rpc-contract-unification-methods.md`,
//! decided 2026-09-02) into the registry as **data**: 6 rename folds
//! (v2 name adopted, old name recorded per backend), 25 retirements,
//! 14 rust-first keeps, and 60 unimplemented v2 rows (incl.
//! `session.registerAgent`). The rename folds have since all been executed
//! on the wire — five server-side folds in batch ⑥ and the embedded
//! `plan.get` → `plan.status` rename in M3-c — so no `RenameInto` rows
//! remain and [`current_names_for`] now reports each backend's canonical
//! entry names directly.
//!
//! The registry rows mirror — row for row — the authoritative method table
//! `authoritative-methods.tsv` next to this file; the TSV parity test below
//! fails if the two drift apart.
//!
//! Per decision D3 the registry grows incrementally: M1.1 registers **names,
//! availability, and error-semantics position only**. Typed params/result
//! structs are not transplanted wholesale — each domain migration (M2 for
//! the server handlers, M3 for the embedded arms) fills in its methods'
//! [`TypedMethod`] impls and per-method [`MethodErrorSemantics`] as it moves.
//! Per review decision R5 the registry models facts only: no behavior, no
//! over-generalization. (The D4 adjudication details — v2 membership, batch,
//! params/result refs, notes — live in the TSV, not in Rust code.)

use MethodDomain as D;

/// Which backend(s) serve a method — a recorded fact, not a routing rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendAvailability {
    /// Dispatched by the embedded engine only.
    Embedded,
    /// Registered by the mirri-server RPC registry only.
    Server,
    /// Served by both backends.
    Both,
    /// In the v2 contract but dispatched by neither backend yet (M1.2-a:
    /// the 60 unimplemented rows; gap-filling is out of program scope).
    Unimplemented,
}

impl BackendAvailability {
    /// Whether the embedded engine serves this method.
    pub fn serves_embedded(&self) -> bool {
        matches!(self, Self::Embedded | Self::Both)
    }

    /// Whether the mirri-server registry serves this method.
    pub fn serves_server(&self) -> bool {
        matches!(self, Self::Server | Self::Both)
    }
}

/// Backend selector for [`methods_for`] / [`current_names_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcBackend {
    /// The embedded engine (`mirri-embedded`).
    Embedded,
    /// The RPC server (`mirri-server`).
    Server,
}

/// The dotted-prefix domain a method belongs to — the §3 domain families of
/// the D4 appendix (`plan/rust-rpc-contract-unification-methods.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodDomain {
    Session,
    Message,
    Tool,
    File,
    Fs,
    Mcp,
    Plugin,
    Agent,
    Skill,
    Goal,
    Plan,
    Cron,
    Task,
    Config,
    Core,
    Model,
    Provider,
    Workspace,
    Shell,
    Swarm,
    Collaboration,
    Approval,
    Question,
    Interaction,
    Env,
    Flag,
    ProviderDiscovery,
    Btw,
    Retention,
}

impl MethodDomain {
    /// The dotted prefix as it appears on the wire, e.g. `"session"`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Message => "message",
            Self::Tool => "tool",
            Self::File => "file",
            Self::Fs => "fs",
            Self::Mcp => "mcp",
            Self::Plugin => "plugin",
            Self::Agent => "agent",
            Self::Skill => "skill",
            Self::Goal => "goal",
            Self::Plan => "plan",
            Self::Cron => "cron",
            Self::Task => "task",
            Self::Config => "config",
            Self::Core => "core",
            Self::Model => "model",
            Self::Provider => "provider",
            Self::Workspace => "workspace",
            Self::Shell => "shell",
            Self::Swarm => "swarm",
            Self::Collaboration => "collaboration",
            Self::Approval => "approval",
            Self::Question => "question",
            Self::Interaction => "interaction",
            Self::Env => "env",
            Self::Flag => "flag",
            Self::ProviderDiscovery => "providerDiscovery",
            Self::Btw => "btw",
            Self::Retention => "retention",
        }
    }

    /// The §4 migration batch this domain migrates in (M2/M3 planning aid;
    /// not a stored field — see also `authoritative-methods.tsv`'s `batch`
    /// column). §4 lists: ① session; ② message/tool/task/shell/plan/goal/
    /// cron/swarm/btw; ③ mcp/plugin; ④ workspace/approval/question/
    /// interaction; ⑤ auth/env/flag/provider/fs/providerDiscovery/model/
    /// config; ⑥ RUST-ONLY cleanup. Domains §4 leaves unlisted are mapped
    /// by best fit: `agent`/`skill`/`core` sit with ② (EMB complete, SRV
    /// catch-up), `file` with ⑥ (its RUST-ONLY adjudication executes there).
    pub const fn batch(self) -> MethodBatch {
        match self {
            Self::Session => MethodBatch::Batch1,
            Self::Message
            | Self::Tool
            | Self::Task
            | Self::Shell
            | Self::Plan
            | Self::Goal
            | Self::Cron
            | Self::Swarm
            | Self::Collaboration
            | Self::Btw
            | Self::Agent
            | Self::Skill
            | Self::Core
            | Self::Retention => MethodBatch::Batch2,
            Self::Mcp | Self::Plugin => MethodBatch::Batch3,
            Self::Workspace | Self::Approval | Self::Question | Self::Interaction => {
                MethodBatch::Batch4
            }
            Self::Env
            | Self::Flag
            | Self::Provider
            | Self::Fs
            | Self::ProviderDiscovery
            | Self::Model
            | Self::Config => MethodBatch::Batch5,
            Self::File => MethodBatch::Batch6,
        }
    }
}

/// The §4 migration batch a domain belongs to (M2/M3 planning aid only —
/// not a stored field on the entry; see [`MethodDomain::batch`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodBatch {
    Batch1,
    Batch2,
    Batch3,
    Batch4,
    Batch5,
    Batch6,
}

impl MethodBatch {
    /// The batch number as a `u8` (`1`–`6`).
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Batch1 => 1,
            Self::Batch2 => 2,
            Self::Batch3 => 3,
            Self::Batch4 => 4,
            Self::Batch5 => 5,
            Self::Batch6 => 6,
        }
    }
}

/// A method's error semantics, as declared on the registry entry.
///
/// M1.1 registers every method as [`MethodErrorSemantics::SharedMapping`]:
/// its failures follow the backend-wide D5 mapping (see [`crate::rpc::error`])
/// with no method-specific code coverage declared yet. The M2/M3 domain
/// migrations replace this with the method's concrete error-code coverage as
/// each domain's handlers move onto the registry (plan decision D3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodErrorSemantics {
    /// No per-method declaration yet (M1.1): failures use the shared D5
    /// mapping — `mirri-proto::rpc::json_rpc_code_for_domain_code`.
    SharedMapping,
    /// Declared per-method JSON-RPC code coverage (filled by M2/M3 domain
    /// migrations; empty nowhere yet).
    Codes(&'static [i64]),
}

/// What M1.2-a's D4 adjudication decided for a method row (the disposition
/// column of the authoritative table). Facts — execution (renames, deletions)
/// happens in migration batch ⑥ (M2/M3), not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodDisposition {
    /// Adjudicated keep — stays on the wire as-is (appendix DRIFT and
    /// MISSING-ONE rows; drift shapes are aligned per-domain in M2/M3).
    Settled,
    /// RUST-ONLY row retained as a rust pioneering capability (the 14 keeps:
    /// `config.watch`, `session.status`/`children`/`snapshot`, `message.list`,
    /// `file.*` ×6, `agent.list`/`get`, `tool.catalog`).
    RustFirst,
    /// RUST-ONLY row scheduled for deletion (the 25 retirements, appendix §3).
    /// Still dispatched today — the row leaves the registry only when batch ⑥
    /// executes.
    Retired,
    /// In the v2 contract but implemented by neither backend (the 60
    /// gap rows, incl. `session.registerAgent`). Gap-filling is out of
    /// program scope.
    Unimplemented,
}

/// One registered RPC method: name, domain, disposition, availability, error
/// semantics. Facts only — the entry carries no behavior (R5).
pub struct MethodEntry {
    /// The dotted primary wire name (e.g. `"session.create"`). Alias names
    /// the dispatches also accept are not registered (M1.1 scope).
    pub name: &'static str,
    /// The dotted-prefix domain of the method (appendix §3).
    pub domain: MethodDomain,
    /// The M1.2-a adjudication of this method row.
    pub disposition: MethodDisposition,
    /// Which backend(s) serve the method today.
    pub availability: BackendAvailability,
    /// The method's error-code semantics declaration.
    pub error_semantics: MethodErrorSemantics,
}

/// Typed params/result contract for a registered method — the M2/M3
/// interface placeholder (plan decision D3).
///
/// M1.1 deliberately does **not** transplant the typed wire structs: each
/// domain migration binds its methods to this trait as it moves, so the
/// registry's typed surface grows domain by domain alongside the handler
/// migrations instead of in one big-bang rewrite. Until a domain migrates,
/// its methods are present in [`METHODS`] by name only and have no
/// `TypedMethod` impl.
///
/// Implementations pin the method to its [`MethodEntry`] (`NAME` must match
/// a registered entry) and bind the request/response types the domain
/// migration settles on; serde/schemars bounds are added with the first
/// real impls rather than frozen now.
pub trait TypedMethod {
    /// The dotted primary wire name; must equal a [`MethodEntry::name`].
    const NAME: &'static str;
    /// Typed request params for the method.
    type Params;
    /// Typed response result for the method.
    type Result;
}

// ---------------------------------------------------------------------------
// Constructors keeping the static table compact. Each pins the M1.1
// error-semantics default (shared D5 mapping, decision D3). The compact
// names encode availability × disposition so every row reads in one line.
// ---------------------------------------------------------------------------

const fn emb(name: &'static str, domain: MethodDomain) -> MethodEntry {
    MethodEntry {
        name,
        domain,
        disposition: MethodDisposition::Settled,
        availability: BackendAvailability::Embedded,
        error_semantics: MethodErrorSemantics::SharedMapping,
    }
}

const fn both(name: &'static str, domain: MethodDomain) -> MethodEntry {
    MethodEntry {
        name,
        domain,
        disposition: MethodDisposition::Settled,
        availability: BackendAvailability::Both,
        error_semantics: MethodErrorSemantics::SharedMapping,
    }
}

/// Both-faces rust-first keep (an embedded route the serve wire reaches
/// through the engine fall-through). The TS v2 surface stays on its REST
/// routes — no klient RPC — so the row is outside the v2 contract.
const fn both_kept(name: &'static str, domain: MethodDomain) -> MethodEntry {
    MethodEntry {
        name,
        domain,
        disposition: MethodDisposition::RustFirst,
        availability: BackendAvailability::Both,
        error_semantics: MethodErrorSemantics::SharedMapping,
    }
}

/// Embedded-served, retired (still dispatched; removed in batch ⑥).
const fn emb_retired(name: &'static str, domain: MethodDomain) -> MethodEntry {
    MethodEntry {
        name,
        domain,
        disposition: MethodDisposition::Retired,
        availability: BackendAvailability::Embedded,
        error_semantics: MethodErrorSemantics::SharedMapping,
    }
}

/// Server-served rust-first keep (retained as a rust pioneering capability).
const fn srv_kept(name: &'static str, domain: MethodDomain) -> MethodEntry {
    MethodEntry {
        name,
        domain,
        disposition: MethodDisposition::RustFirst,
        availability: BackendAvailability::Server,
        error_semantics: MethodErrorSemantics::SharedMapping,
    }
}

/// The full method registry: 215 rows (P2 MCP pool surface + the P5d
/// session.getHistory read face + the P6 agent profile CRUD vertical +
/// the R4 Task 25 agent profile enable/disable/reset lifecycle rows +
/// the P10 session.search read face + the T2 user-tool management
/// vertical + the P6 B4a providerDiscovery.listCatalog browse + the M5
/// mcp.config.preview + the P7 B4b providerDiscovery.importCatalog
/// import + the M6 mcp.sync + the M7 mcp.subscribe + the W8
/// workspace.setTrust row + the 2 collaboration mode-gate rows + the 2 B8
/// skills management rows + the 3 B9 approval rule CRUD rows + the 2 B6
/// cron management rows, the 6 session-retention rows joined with the
/// retention domain, and the ADR-010 skill.reload and skill.subscribe
/// rows joined with the skill hot-reload domain; the 8 B5-d `auth.*`
/// rows were removed with the managed-login domain) = 199
/// embedded-current ∪ 190 server-current (174 both) + 0 unimplemented
/// v2 gap rows; 23 of the
/// current rows are retired-but-still-dispatched. All six D4 §2 rename
/// folds have been executed (five server-side in batch ⑥, the embedded
/// `plan.status` ← `plan.get` in M3-c), so every row carries its canonical
/// wire name. Embedded methods appear in the embedded dispatch's domain
/// order; server-only methods follow, sorted; unimplemented rows close the
/// table, grouped by domain and sorted. (B4-a moved the 3 approval/
/// question HITL rows to both via EMB backfills and implemented the 5
/// interaction gap rows; B4-b did the same for the 6 workspace rows; B5-a
/// implemented the 21 shallow batch-A gap rows — env×13, flag×5, fs×2,
/// providerDiscovery×1 — plus the P6 `providerDiscovery.listCatalog`
/// browse row and the P7 `providerDiscovery.importCatalog` import row
/// (×3 on the domain now); the counts live in the pinned tests
/// below; and the SDR-014 `session.touch` recency row joined the
/// both-current settled surface.)
pub const METHODS: &[MethodEntry] = &[
    // --- embedded dispatch: session domain --------------------------------
    both("session.create", D::Session),
    both("session.list", D::Session),
    both("session.resume", D::Session),
    both("session.reload", D::Session),
    both("session.close", D::Session),
    both("session.archive", D::Session),
    both("session.fork", D::Session),
    both("session.rename", D::Session),
    // SDR-014 recency touch — user-interaction-only, bumps lastUsedAt
    // (the sidebar sort key) without touching updatedAt.
    both("session.touch", D::Session),
    both("session.updateMeta", D::Session),
    // B1 serve flip: the embedded route also rides the serve wire through
    // the engine fall-through (SRV has update, no read-back).
    both("session.getMeta", D::Session),
    both("session.export", D::Session),
    both("session.warnings", D::Session),
    both("session.clearContext", D::Session),
    both("session.undo", D::Session),
    // P5d: the ACP session/load history-replay read face (embedded-only —
    // the serialized ContextMessage history of the addressed agent).
    emb("session.getHistory", D::Session),
    // P10: the global literal search read face (title + user/assistant
    // message text, NFKC + case folded, cursor paginated). Both surfaces:
    // the serve wire rides the engine fall-through, so the web's global
    // search reaches it.
    both("session.search", D::Session),
    // --- embedded dispatch: agent/turn domain ------------------------------
    both("message.send", D::Message),
    both("message.abort", D::Message),
    both("message.steer", D::Message),
    both("agent.getContext", D::Agent),
    both("agent.getUsage", D::Agent),
    both("agent.getModel", D::Agent),
    both("agent.setModel", D::Agent),
    both("agent.setThinking", D::Agent),
    both("agent.setPermission", D::Agent),
    // retired: v1-era agent surface, removed in batch ⑥ (v2 routes via
    // session metadata / prompt.disabledTools / config.get).
    emb_retired("agent.getPermission", D::Agent),
    emb_retired("agent.getConfig", D::Agent),
    emb_retired("agent.setActiveTools", D::Agent),
    emb_retired("agent.registerTool", D::Agent),
    emb_retired("agent.unregisterTool", D::Agent),
    // --- embedded dispatch: tool / task / cron domain ----------------------
    both("tool.list", D::Tool),
    // T2 user-tool management vertical (plans/07): new wires, both faces via
    // the engine fall-through (no klient counterpart — the management face
    // is rust-first; the read side lists registered user tools with their
    // current visibility, the write side toggles per-name enablement).
    both("tool.listUserTools", D::Tool),
    both("tool.setUserToolEnabled", D::Tool),
    both("task.list", D::Task),
    both("task.cancel", D::Task),
    both("task.output", D::Task),
    both("task.detach", D::Task),
    // B7 workspace task overview (P-023 / D-1): new wire, both faces via
    // the engine fall-through — the live-session task aggregation over
    // every built workspace runtime (the same AgentTask element shape
    // task.list serves, `session_id` carrying the attribution). A
    // rust-first keep: no TS v2 counterpart, so the row stays outside the
    // v2 contract (the both_kept spelling, cron.delete parity).
    both_kept("task.listWorkspace", D::Task),
    both("cron.list", D::Cron),
    // B6 cron management write face: new wire, both faces via the engine
    // fall-through (a thin wrapper over the shared core's durable
    // `CronManager::remove` — the same cron.delete wire op the stale
    // auto-delete path records, write-before-mutate; unknown ids answer
    // not_found, task.cancel parity).
    both_kept("cron.delete", D::Cron),
    // B6 cron management run-now face: fire one task immediately through
    // the same cron_job injection path the driver uses, leaving the
    // schedule untouched (D-2 — no cursor advance, no reschedule, no
    // one-shot consumption; the natural fire still happens).
    both_kept("cron.trigger", D::Cron),
    // --- embedded dispatch: compaction domain ------------------------------
    // (renamed from the server's `session.compact` stub in batch ⑥, D4 §2).
    both("session.beginCompaction", D::Session),
    both("session.cancelCompaction", D::Session),
    // --- embedded dispatch: plan domain ------------------------------------
    both("plan.enter", D::Plan),
    both("plan.cancel", D::Plan),
    both("plan.clear", D::Plan),
    // (renamed from the embedded `plan.get` in M3-c, D4 §2; the old wire
    // name is gone).
    both("plan.status", D::Plan),
    // --- embedded dispatch: goal domain ------------------------------------
    both("goal.create", D::Goal),
    both("goal.get", D::Goal),
    both("goal.pause", D::Goal),
    both("goal.resume", D::Goal),
    both("goal.cancel", D::Goal),
    // --- embedded dispatch: swarm domain -----------------------------------
    both("swarm.enter", D::Swarm),
    both("swarm.exit", D::Swarm),
    both("swarm.get", D::Swarm),
    // --- embedded dispatch: collaboration domain --------------------------
    // Mode gate for the Collaboration* tools: `collaboration.enter` /
    // `collaboration.exit` toggle the session's collaboration-mode flag
    // and expose/hide the 7 builtin collaboration tools to the LLM (the
    // payload mirrors swarm's `{ "session_id": string }`).
    both("collaboration.enter", D::Collaboration),
    both("collaboration.exit", D::Collaboration),
    // --- embedded dispatch: skill domain -----------------------------------
    both("skill.list", D::Skill),
    // B8 skills management read face: the skill's SKILL.md content.
    both_kept("skill.getContent", D::Skill),
    // B8 skills management write face: user-level per-skill enablement.
    both_kept("skill.setEnabled", D::Skill),
    // ADR-010 Task 6: the manual skill-catalog reload face — rescan every
    // built workspace through the serialized orchestrator and report the
    // per-workspace diff counts plus the watcher status (R4).
    both_kept("skill.reload", D::Skill),
    // (renamed from the server's `skill.activate` in batch ⑥, D4 §2).
    both("agent.activateSkill", D::Agent),
    // --- embedded dispatch: core info / config -----------------------------
    both("core.info", D::Core),
    both("core.experimentalFeatures", D::Core),
    // M5 drill (§7 criterion 7): a both-method walking the full
    // registry → handler → wire flow with one embedded route; the serve
    // wire gets it through the engine fall-through, zero transport code.
    both("core.echo", D::Core),
    both("config.get", D::Config),
    emb("config.diagnostics", D::Config),
    both("config.set", D::Config),
    // Both surfaces — the serve wire rides the engine fall-through, so the
    // web's provider-delete flow reaches it.
    both("config.removeProvider", D::Config),
    // retired: v1-era model removal, removed in batch ⑥ (v2 cascades via
    // config.removeProvider).
    emb_retired("config.removeModel", D::Config),
    // --- embedded dispatch: model catalog ----------------------------------
    both("model.list", D::Model),
    // --- embedded dispatch: plugin domain ----------------------------------
    // (M1.1 order: model.listProviders sits with the plugin block that
    // follows it in the dispatch match.)
    both("model.listProviders", D::Model),
    both("plugin.list", D::Plugin),
    both("plugin.install", D::Plugin),
    both("plugin.setEnabled", D::Plugin),
    both("plugin.setMcpServerEnabled", D::Plugin),
    both("plugin.remove", D::Plugin),
    both("plugin.reload", D::Plugin),
    both("plugin.info", D::Plugin),
    both("plugin.commands", D::Plugin),
    both("plugin.activateCommand", D::Plugin),
    // B3 gap implementation (Task 7): github-sourced update check.
    both("plugin.checkUpdates", D::Plugin),
    // --- embedded dispatch: MCP domain -------------------------------------
    both("mcp.list", D::Mcp),
    both("mcp.startupMetrics", D::Mcp),
    both("mcp.reconnect", D::Mcp),
    // P1 MCP config surface (batch 3): the layered read-write wire of
    // mcp-architecture §4.1 — new wires, both faces via the same engine
    // fall-through (the TS v2 facade lands in the same batch).
    both("mcp.config.list", D::Mcp),
    both("mcp.config.set", D::Mcp),
    both("mcp.config.remove", D::Mcp),
    both("mcp.config.setToolFilter", D::Mcp),
    // P2 MCP pool surface (batch 4): the app-level status view and the
    // fresh-connect config test — new wires, both faces via engine
    // fall-through (the TS v2 facade lands with the user-pool split in
    // the same batch).
    both("mcp.config.test", D::Mcp),
    // F-4 resolved-value preview (batch M5): the adjudication A5 sole
    // sanctioned outlet for environment-expanded config values — an
    // explicit request only, nothing cached, emitted, or logged.
    both("mcp.config.preview", D::Mcp),
    both("mcp.status", D::Mcp),
    // P3 MCP tool view (batch M1): the single-server tool listing with
    // per-tool filter state (ADR-004) — new wire, both faces via the
    // same engine fall-through.
    both("mcp.tools", D::Mcp),
    // mcp-architecture ADR-006 — the F-6 idempotent reconcile entry, both
    // faces via the same engine kernel (the CLI watcher rides it too)
    both("mcp.sync", D::Mcp),
    // retired: v1-era session-level MCP toggles, removed in batch ⑥ (v2
    // deleted them; EMB replies are silent acks, P9).
    emb_retired("mcp.enable", D::Mcp),
    emb_retired("mcp.disable", D::Mcp),
    emb_retired("mcp.enableTool", D::Mcp),
    emb_retired("mcp.disableTool", D::Mcp),
    emb_retired("mcp.toggleState", D::Mcp),
    // retired: v1-era global MCP surface, removed in batch ⑥ (v2 has no
    // global MCP RPC; EMB mixes acks and real implementations, P9).
    emb_retired("mcp.global.list", D::Mcp),
    emb_retired("mcp.global.tools", D::Mcp),
    emb_retired("mcp.global.reload", D::Mcp),
    emb_retired("mcp.global.create", D::Mcp),
    emb_retired("mcp.global.update", D::Mcp),
    emb_retired("mcp.global.delete", D::Mcp),
    emb_retired("mcp.global.toggle", D::Mcp),
    emb_retired("mcp.global.toggleState", D::Mcp),
    emb_retired("mcp.global.enableTool", D::Mcp),
    emb_retired("mcp.global.disableTool", D::Mcp),
    emb_retired("mcp.global.testConnect", D::Mcp),
    emb_retired("mcp.global.connect", D::Mcp),
    // --- embedded dispatch: misc session/agent/shell -----------------------
    // B1 serve flips: the embedded routes also ride the serve wire through
    // the engine fall-through (the print-mode pair keeps its
    // print-mode-only semantics, TSV note records it).
    both("session.generateAgentsMd", D::Session),
    both("session.waitForBackgroundTasksOnPrint", D::Session),
    both("session.handlePrintMainTurnCompleted", D::Session),
    both("session.addAdditionalDir", D::Session),
    both("shell.run", D::Shell),
    both("shell.cancel", D::Shell),
    // (renamed from the server's `session.startBtw` in batch ⑥, D4 §2).
    both("btw.start", D::Btw),
    // --- server registry only (no embedded arm) ----------------------------
    // rust-first keep: v2 enumerates agents via session.getMeta's agents map.
    srv_kept("agent.get", D::Agent),
    srv_kept("agent.list", D::Agent),
    // (renamed from the server's `approval.resolve` in batch ⑥, D4 §2).
    // B4-a: the embedded backfill mirrors the SRV handler wire-for-wire, so
    // the row is both-current (the serve wire keeps its transport-owned
    // handler).
    both("approval.decide", D::Approval),
    // rust-first keep: v2 consumes config via the event stream.
    srv_kept("config.watch", D::Config),
    // rust-first keeps: file domain (6) — v2 serves files over REST + agent
    // tools; no klient RPC counterpart.
    srv_kept("file.diff", D::File),
    srv_kept("file.git_status", D::File),
    srv_kept("file.grep", D::File),
    srv_kept("file.list", D::File),
    srv_kept("file.read", D::File),
    srv_kept("file.search", D::File),
    // M7 app-level event subscription surface (F-9, ADR-010): the
    // WebSocket connection registers on the engine's app-level MCP pool
    // broadcast and receives `event.mcp.server.status` notifications with
    // no session anywhere. Server-only — the embedded engine has no wire
    // route (the broadcast is consumed through the EngineAdapter seam).
    srv_kept("mcp.subscribe", D::Mcp),
    // ADR-010 Task 6 app-level event subscription surface (the
    // mcp.subscribe precedent): the WebSocket connection registers on the
    // engine's skill-catalog change broadcast and receives
    // `skill.catalogChanged` notifications with no session anywhere.
    // Server-only — the embedded engine exposes the broadcast through the
    // EngineAdapter seam (subscribe_skill_catalog_events), no embedded
    // wire route.
    srv_kept("skill.subscribe", D::Skill),
    // rust-first keep: v2 reads history via agent.getContext.
    srv_kept("message.list", D::Message),
    // B4-a: the embedded backfills mirror the SRV handlers wire-for-wire, so
    // the rows are both-current (the serve wire keeps its transport-owned
    // handlers).
    both("question.answer", D::Question),
    both("question.dismiss", D::Question),
    // rust-first keep: v2 equivalent is session.list?childOf.
    srv_kept("session.children", D::Session),
    // B1 EMB backfill: the serve wire keeps its transport-owned handler
    // (same wire shape as the embedded route).
    both("session.get", D::Session),
    // B1 EMB backfill: the serve wire keeps its transport-owned handler
    // (same wire shape as the embedded route).
    both("session.restore", D::Session),
    // rust-first keep: WS subscription cursor snapshot, no v2 counterpart.
    srv_kept("session.snapshot", D::Session),
    // rust-first keep: aggregated status; v2 clients self-assemble from six
    // probes (≈ agent.activity).
    srv_kept("session.status", D::Session),
    // rust-first keep: rich catalog; v2 equivalent is agent.getTools.
    srv_kept("tool.catalog", D::Tool),
    // (renamed from the server's `workspace.create` in batch ⑥, D4 §2).
    // B4-b: the EMB backfills mirror the SRV wire (single-bound-workspace
    // semantics), so all four rows are both-current now.
    both("workspace.createOrTouch", D::Workspace),
    both("workspace.delete", D::Workspace),
    both("workspace.list", D::Workspace),
    both("workspace.update", D::Workspace),
    // --- v2-contract gap rows: implemented by neither backend --------------
    // Unimplemented per D4 appendix §3 (MISSING + session.registerAgent,
    // §5.4). Gap-filling is out of program scope; these rows are facts only.

    // session.* (B1: the first three gap rows moved to `both`)
    both("session.countActive", D::Session),
    both("session.createChild", D::Session),
    // §5.4 dotted-name adjudication: klient sessionMetadata.registerAgent is
    // a real callable procedure, counted into the v2 surface.
    both("session.registerAgent", D::Session),
    // fs.* — B5-a gap implementations (embedded routes; the serve wire
    // rides the engine fall-through). Klient hostFolderBrowser shapes
    // (snake_case wire, per protocol/src/rest/fsBrowse).
    both("fs.browse", D::Fs),
    both("fs.home", D::Fs),
    // agent.*
    // agent.activity (B2 gap implementation, definitive dotted name per
    // controller adjudication): the v2 wire identity is
    // agentActivityView.state — the engine folds a per-agent
    // ActivityTracker from the TurnFlow/TurnFlowHooks observation points,
    // and the serve wire rides the engine fall-through.
    both("agent.activity", D::Agent),
    // B2 gap implementations (embedded routes; the serve wire rides the
    // engine fall-through).
    both("agent.getEffectiveThinkingLevel", D::Agent),
    both("agent.listCommands", D::Agent),
    both("agent.runCommand", D::Agent),
    // P6 gap implementations — the agent profile CRUD behind the web
    // `/api/v1/agents` REST routes (embedded routes over the
    // `<mirri-home>/profiles/` persistence the ProfileManager owns; the
    // serve wire rides the engine fall-through). No klient RPC
    // counterpart — the TS surface is REST — so the params/result
    // contracts are declared rust-first (the TS `ProfileEntry` wire
    // shape).
    both("agent.createProfile", D::Agent),
    both("agent.deleteProfile", D::Agent),
    // R4 Task 25 gap implementations — the enable/disable/reset lifecycle
    // over the same `.md` write channel (adjudication b: the web
    // `WsRpcApi` connects to these directly, no REST bridge). Rust-first
    // keeps: the TS transitional surface stays on the kap-server REST
    // action routes (`:enable` / `:disable` / `:reset`), so no klient RPC
    // lands — the params/result contracts are declared rust-first
    // (`{name}` → `{ok}`), the TS texts are the cross-engine reference.
    both_kept("agent.disableProfile", D::Agent),
    both_kept("agent.enableProfile", D::Agent),
    both("agent.getProfile", D::Agent),
    both("agent.listProfiles", D::Agent),
    both_kept("agent.resetProfile", D::Agent),
    both("agent.updateProfile", D::Agent),
    // task.*
    // B2 gap implementation (the task.cancel sister operation, with reason).
    both("task.stop", D::Task),
    // config.* — B5-b gap implementations (embedded routes over the
    // config.toml section pipeline the Settled config.get/config.set
    // handlers own; the serve wire rides the engine fall-through). Klient
    // configService section shapes; the memory target is honestly rejected
    // (no per-run memory layer) and inspect answers only the value/user
    // layers — TSV notes record the DRIFTs.
    both("config.getAll", D::Config),
    both("config.inspect", D::Config),
    both("config.reload", D::Config),
    both("config.replace", D::Config),
    both("config.replaceSections", D::Config),
    // model.* — B5-b gap implementations: the modelService record methods
    // and the modelResolver catalog writes (embedded routes over the
    // config.toml [models]/[providers] sections; the serve wire rides the
    // engine fall-through).
    both("model.delete", D::Model),
    // model.generate — B5-c gap implementation: the only streaming
    // contract (klient modelResolver.generate). The embedded handler
    // broadcasts best-effort `event.generate.chunk` notifications and
    // answers with the authoritative aggregated response; the serve wire
    // rides the engine fall-through (aggregated response only — chunk
    // frames are not forwarded over WS yet).
    both("model.generate", D::Model),
    both("model.get", D::Model),
    both("model.getProvider", D::Model),
    both("model.set", D::Model),
    both("model.setDefaultModel", D::Model),
    // provider.* — B5-b gap implementations (embedded routes over the
    // config.toml [providers] sections; the serve wire rides the engine
    // fall-through). Klient providerService CRUD shapes.
    both("provider.delete", D::Provider),
    both("provider.get", D::Provider),
    both("provider.list", D::Provider),
    both("provider.set", D::Provider),
    // workspace.* — B4-b gap implementations (embedded routes; the serve
    // wire rides the engine fall-through).
    both("workspace.get", D::Workspace),
    both("workspace.handlerFor", D::Workspace),
    // W8 trust management face (rust-first keep: the TS v2 surface stays on
    // the kap-server REST trust routes — GET/POST /workspaces/:id/trust,
    // POST /untrust — so no klient RPC lands; the params/result contracts
    // are declared rust-first, the TS `workspaceTrustResponseSchema` the
    // cross-engine reference).
    both_kept("workspace.setTrust", D::Workspace),
    // approval.* — B4-a gap implementation (the klient approvalRequest[]
    // shape, straight from the approval gate's pending snapshots; the serve
    // wire rides the engine fall-through).
    //
    // The B9 user-level permission rule CRUD (D-34) rides the same domain:
    // new wires over the shared config snapshot's [permission] section —
    // the write side goes through the config.toml pipeline then pushes the
    // new rule set into every live agent. Rust-first keeps: the TS v2
    // surface has no klient RPC counterpart, so both faces ride the engine
    // fall-through.
    both_kept("approval.addRule", D::Approval),
    both("approval.listPending", D::Approval),
    both_kept("approval.listRules", D::Approval),
    both_kept("approval.removeRule", D::Approval),
    // question.* — B4-a gap implementation (the klient questionRequest[]
    // shape, multiSelect-normalized; the serve wire rides the engine
    // fall-through).
    both("question.listPending", D::Question),
    // interaction.* — B4-a gap implementation (the klient interaction
    // envelope over both gates; `user_tool` has no rust engine source yet,
    // so that kind filter always answers an empty list — TSV note records
    // the constant). The serve wire rides the engine fall-through.
    both("interaction.isRecentlyResolved", D::Interaction),
    both("interaction.listPending", D::Interaction),
    both("interaction.respond", D::Interaction),
    // env.* — B5-a gap implementations (embedded routes; the serve wire
    // rides the engine fall-through). The klient bootstrapService
    // zero-arg reads, mirrored on the engine wire (camelCase shapes).
    both("env.arch", D::Env),
    both("env.blobsDir", D::Env),
    both("env.cacheDir", D::Env),
    both("env.clientIdentity", D::Env),
    both("env.clientVersion", D::Env),
    both("env.configPath", D::Env),
    both("env.cwd", D::Env),
    both("env.homeDir", D::Env),
    both("env.logsDir", D::Env),
    both("env.osHomeDir", D::Env),
    both("env.platform", D::Env),
    both("env.sessionsDir", D::Env),
    both("env.storeDir", D::Env),
    // flag.* — B5-a gap implementations (embedded routes over the engine's
    // FlagRegistry; the serve wire rides the engine fall-through). Klient
    // flagService shapes, rich fields honestly degraded (TSV notes record
    // the DRIFT mapping).
    both("flag.enabled", D::Flag),
    both("flag.enabledIds", D::Flag),
    both("flag.explain", D::Flag),
    both("flag.explainAll", D::Flag),
    both("flag.snapshot", D::Flag),
    // providerDiscovery.* — B5-a gap implementation (the honest empty
    // success until a remote provider-model discovery capability exists;
    // the serve wire rides the engine fall-through), the P6 B4a
    // `listCatalog` directory browse (G-8 前半 — same engine fall-through),
    // and the P7 B4b `importCatalog` directory import (G-8 后半 — a config
    // write through the same fall-through).
    both(
        "providerDiscovery.refreshProviderModels",
        D::ProviderDiscovery,
    ),
    both("providerDiscovery.listCatalog", D::ProviderDiscovery),
    both("providerDiscovery.importCatalog", D::ProviderDiscovery),
    // --- session-retention domain (rust-first, both faces) ------------------
    // The management-page API over the retention engine's archive /
    // compression / delete surfaces (session-retention-architecture §4;
    // typed contracts in [`crate::retention`]). Rust-first keeps: the TS
    // v2 surface has no klient RPC counterpart (the web management page is
    // rust-first too), so both faces ride the engine fall-through like the
    // cron-management and profile-lifecycle rows before them.
    both_kept("retention.list", D::Retention),
    both_kept("retention.stats", D::Retention),
    both_kept("retention.compressNow", D::Retention),
    both_kept("retention.delete", D::Retention),
    both_kept("retention.deleteTiered", D::Retention),
    both_kept("retention.getPolicy", D::Retention),
];

/// Every registered method entry.
pub fn all_methods() -> impl Iterator<Item = &'static MethodEntry> {
    METHODS.iter()
}

/// The entries a backend serves (availability `Embedded`/`Both` for
/// [`RpcBackend::Embedded`], `Server`/`Both` for [`RpcBackend::Server`]).
/// Unimplemented rows serve neither backend and are excluded.
pub fn methods_for(backend: RpcBackend) -> impl Iterator<Item = &'static MethodEntry> {
    METHODS.iter().filter(move |entry| match backend {
        RpcBackend::Embedded => entry.availability.serves_embedded(),
        RpcBackend::Server => entry.availability.serves_server(),
    })
}

/// The wire names a backend dispatches: the canonical [`MethodEntry::name`]
/// of every row that backend serves. (The M1.2-a rename-aware override
/// machinery was retired in M3-c once the last fold — the embedded
/// `plan.get` → `plan.status` rename — executed on the wire.)
pub fn current_names_for(backend: RpcBackend) -> impl Iterator<Item = &'static str> {
    methods_for(backend).map(|entry| entry.name)
}

/// Look up a method entry by its primary wire name.
pub fn get(name: &str) -> Option<&'static MethodEntry> {
    METHODS.iter().find(|entry| entry.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The B5-a pinned counts: 179 rows total. All six D4 §2 rename folds
    /// are executed (five server-side in batch ⑥, the embedded
    /// `plan.get` → `plan.status` in M3-c), the two server-retired rows
    /// (`tool.call`, `workspace.refresh`) were removed, and the M5-3b
    /// `core.echo` drill row joined the both surface, so the pre-B1
    /// surface was embedded 96, server 58, both 35. The B1 session
    /// vertical moved 9 more rows to `both` (5 serve flips + 3 gap
    /// implementations + the `session.get` EMB backfill), the Task-2
    /// `session.restore` EMB backfill moved one more, the B2 serve
    /// vertical flipped the 19 batch-2 embedded-only rows to `both`
    /// (goal/swarm engine-service extras' first serve exposure,
    /// plan.enter/cancel/clear/status, cron.list, task.detach,
    /// shell.run/cancel, core.info/experimentalFeatures,
    /// agent.getModel — all via the serve engine fall-through, zero
    /// transport code), the B2 gap vertical implemented five more rows
    /// (agent.getEffectiveThinkingLevel / listCommands / runCommand,
    /// task.stop, and agent.activity — the ActivityTracker-folded
    /// activity view), and the B3 serve vertical flipped the 12
    /// batch-3 embedded-only rows to `both` (the 9 plugin methods and
    /// the 3 mcp methods — same engine fall-through path), and the B3
    /// gap vertical implemented `plugin.checkUpdates` (the
    /// github-resolution update check, mirri-plugin-host), and the B4-a
    /// interaction vertical backfilled the 3 SRV-shape HITL methods on
    /// the embedded wire (`approval.decide`, `question.answer`,
    /// `question.dismiss` — both-current) and implemented the 5 gap rows
    /// (`approval.listPending`, `question.listPending`,
    /// `interaction.listPending/respond/isRecentlyResolved` — the
    /// blocking-HITL kernel over the session gates, serve wire via the
    /// engine fall-through), and the B4-b workspace vertical backfilled
    /// the 4 SRV-shape workspace methods on the embedded wire
    /// (`createOrTouch/list/update/delete` — wire mirrors, both-current)
    /// and implemented the 2 gap rows (`get/handlerFor` — domain shapes,
    /// serve wire via the engine fall-through), and the B5-a shallow
    /// batch implemented the 21 gap rows (the 13 `env.*` bootstrap
    /// reads, the 5 `flag.*` reads over the engine's FlagRegistry, the
    /// 2 `fs.*` host-folder-browser rows, and
    /// `providerDiscovery.refreshProviderModels` — serve wire via the
    /// engine fall-through), and the B5-b shallow batch B implemented the
    /// 14 gap rows (the 5 `config.*` section methods, the 5 `model.*`
    /// record/catalog methods, and the 4 `provider.*` CRUD rows — serve
    /// wire via the engine fall-through), and the B5-c deep batch
    /// implemented `model.generate`, and the B5-d deep batch implemented
    /// the 8 `auth.*` rows (since deleted with the managed-login domain),
    /// and the batch-3 P1 MCP
    /// config vertical registered the 4 new `mcp.config.*` rows (both
    /// faces via the same engine fall-through; the TS v2 facade lands in
    /// the same batch), and the M1 MCP tool view registered the
    /// `mcp.tools` row (the single-server tool listing with per-tool
    /// filter state, both faces via the same engine fall-through), and
    /// the P6 B4a task registered the `providerDiscovery.listCatalog`
    /// browse row (G-8 前半 — both faces via the same engine
    /// fall-through), and the M5 resolved-value preview registered the
    /// `mcp.config.preview` row (the adjudication A5 sole sanctioned
    /// outlet for expanded values, both faces via the same engine
    /// fall-through), and the P7 B4b task registered the
    /// `providerDiscovery.importCatalog` import row (G-8 后半 — both
    /// faces via the same engine fall-through) and the M7
    /// `mcp.subscribe` row (server-only), and the R4 Task 25 vertical
    /// registered the 3 `agent.*Profile` lifecycle rows
    /// (enable/disable/reset — rust-first keeps, both faces via the same
    /// engine fall-through), and the B9 approval vertical registered the
    /// 3 `approval.*Rule` CRUD rows (listRules/addRule/removeRule —
    /// rust-first keeps, both faces via the same engine fall-through), and
    /// the B7 task-overview vertical registered the `task.listWorkspace`
    /// row (the live-session task aggregation, both faces via the same
    /// engine fall-through), and the managed-login removal deleted the 8
    /// B5-d `auth.*` rows (the whole auth domain), and the ADR-010 Task 6
    /// vertical registered the `skill.reload` row (the manual
    /// skill-catalog reload face, a rust-first keep, both faces via the
    /// same engine fall-through), and the ADR-010 Task 6 event surface
    /// registered the `skill.subscribe` row (the app-level catalog-change
    /// subscription face, server-only), so the
    /// current surface is
    /// embedded 199, server 190, both
    /// 174 (the SDR-014 `session.touch` row joined the both-current
    /// settled surface; the ADR-010 skill.reload row joined it too). If
    /// any of these moves, the
    /// registry is out of sync with the engines or the adjudication, and
    /// this test goes red.
    #[test]
    fn should_pin_the_method_count_quadruple() {
        let embedded = methods_for(RpcBackend::Embedded).count();
        let server = methods_for(RpcBackend::Server).count();
        let both = all_methods()
            .filter(|entry| entry.availability == BackendAvailability::Both)
            .count();
        let total = all_methods().count();

        assert_eq!(embedded, 199, "embedded-available method count");
        assert_eq!(server, 190, "server-available method count");
        assert_eq!(both, 174, "embedded ∩ server method count");
        assert_eq!(total, 215, "total registered rows");
        // The current surface is self-consistent: the union of
        // the two backend views equals the 215 registered rows
        // (the last unimplemented set was cleared in B5; the 8 auth rows
        // left again with the managed-login domain removal).
        assert_eq!(total, embedded + server - both);
    }

    /// The disposition ledger, pinned to the D4 decisions plus the B1
    /// session vertical, the B2 gap vertical, the B3 gap vertical, the
    /// B4-a interaction vertical, the B4-b workspace vertical, the B5-a
    /// shallow batch A, the B5-b shallow batch B, the P6
    /// providerDiscovery.listCatalog row, and the P7
    /// providerDiscovery.importCatalog row (the 3 + 5 + 1 + 8 + 2 + 21 + 14
    /// gap rows moved Unimplemented → Settled; the
    /// B4-a/B4-b EMB backfills of the 3 + 4 SRV-shape rows were already
    /// Settled).
    #[test]
    fn should_record_the_disposition_ledger() {
        let retired = all_methods()
            .filter(|entry| entry.disposition == MethodDisposition::Retired)
            .count();
        let rust_first = all_methods()
            .filter(|entry| entry.disposition == MethodDisposition::RustFirst)
            .count();
        let unimplemented = all_methods()
            .filter(|entry| entry.disposition == MethodDisposition::Unimplemented)
            .count();
        let settled = all_methods()
            .filter(|entry| entry.disposition == MethodDisposition::Settled)
            .count();

        assert_eq!(
            retired, 23,
            "retired rows (23 embedded; the 2 server rows were removed in batch ⑥)"
        );
        assert_eq!(
            rust_first, 35,
            "rust-first keeps (the M7 mcp.subscribe row joined them; the R4 Task 25 profile lifecycle rows — enable/disable/reset — joined them next; the W8 workspace.setTrust row joined them after that; the 3 B9 approval rule CRUD rows joined them after that; the 2 B6 cron management rows joined them, the B7 task.listWorkspace overview row joined them, the 6 session-retention rows joined them, and the ADR-010 skill.reload / skill.subscribe rows joined them last)"
        );
        assert_eq!(
            unimplemented, 0,
            "v2 gap rows (60 − the 3 B1, the 5 B2, the 1 B3, the 5 B4-a, the 2 B4-b, the 21 B5-a, the 14 B5-b, the 1 B5-c, and the 8 B5-d implementations — every gap row is now settled)"
        );
        assert_eq!(
            settled, 157,
            "drift + remaining MISSING-ONE rows (98 both + the embedded-only and server-only settled rows + the 4 batch-3 P1 MCP config rows + the 2 batch-4 P2 MCP pool rows + the M1 mcp.tools row + the P6 providerDiscovery.listCatalog row + the M5 mcp.config.preview row + the P7 providerDiscovery.importCatalog row + the M6 mcp.sync row + the 2 collaboration mode-gate rows; the 8 B5-d auth rows left with the managed-login removal; the SDR-014 session.touch row joined)"
        );
        assert_eq!(retired + rust_first + unimplemented + settled, 215);
        // The live surface check: every registered
        // row is currently dispatched (the last unimplemented set was
        // cleared in B5; the 8 auth rows left again with the managed-login
        // domain removal).
        assert_eq!(
            retired + rust_first + settled,
            215,
            "currently-dispatched rows"
        );
        assert_eq!(
            all_methods().count() - retired,
            192,
            "non-retired rows (35 rust-first + the settled rows + 0 unimplemented gap rows)"
        );
    }

    /// The backend current views are duplicate-free:
    /// `current_names_for(Embedded)` names the 192-name canonical set
    /// (spot-checked around the executed rename folds — `plan.status` yes,
    /// `plan.get` no — the B1 session additions, and pinned by the node-sdk
    /// fixture e2e), and `current_names_for(Server)` the 190-name set after
    /// the batch-⑥ renames, the M5-3b core.echo drill row, the B1
    /// session vertical, the B2 serve flips (the 19 batch-2
    /// embedded-only rows now ride the serve wire through the engine
    /// fall-through), the B2 gap implementations, and the B3 serve
    /// flips (the 12 batch-3 plugin/mcp embedded-only rows) and the B3
    /// gap implementation (plugin.checkUpdates), plus the B4-a
    /// interaction vertical (the 3 EMB backfills and the 5 gap
    /// implementations are both-current), and the B4-b workspace vertical
    /// (the 4 EMB backfills and the 2 gap implementations are
    /// both-current), plus the M1-M7 MCP rows and the P6/P7
    /// providerDiscovery rows.
    #[test]
    fn should_report_the_backend_current_views() {
        fn assert_unique_and_count(backend: RpcBackend, expected: usize) -> Vec<&'static str> {
            let mut names: Vec<&'static str> = current_names_for(backend).collect();
            names.sort_unstable();
            let unique: HashSet<&'static str> = names.iter().copied().collect();
            assert_eq!(
                unique.len(),
                names.len(),
                "{backend:?} current names must be duplicate-free"
            );
            assert_eq!(names.len(), expected, "{backend:?} current name count");
            names
        }

        let embedded = assert_unique_and_count(RpcBackend::Embedded, 199);
        let server = assert_unique_and_count(RpcBackend::Server, 190);

        // Rename-affected names, spot-checked per backend:
        // - embedded dispatches `plan.status` (the M3-c rename); `plan.get`,
        //   `session.compact`, `skill.activate`, `session.startBtw` are not
        //   on the embedded wire today.
        assert!(
            embedded.contains(&"plan.status"),
            "embedded dispatches plan.status (renamed from plan.get in M3-c)"
        );
        assert!(
            !embedded.contains(&"plan.get"),
            "plan.get is not on the embedded wire"
        );
        assert!(!embedded.contains(&"session.compact"));
        assert!(!embedded.contains(&"skill.activate"));
        assert!(!embedded.contains(&"session.startBtw"));
        assert!(embedded.contains(&"session.beginCompaction"));
        assert!(embedded.contains(&"agent.activateSkill"));
        assert!(embedded.contains(&"btw.start"));
        // The M5-3b drill row joins the embedded dispatch (one route).
        assert!(embedded.contains(&"core.echo"));
        // The B1 session vertical: the 3 gap implementations and the
        // `session.get` EMB backfill are embedded-current.
        assert!(embedded.contains(&"session.countActive"));
        assert!(embedded.contains(&"session.createChild"));
        assert!(embedded.contains(&"session.registerAgent"));
        assert!(embedded.contains(&"session.get"));
        // The Task-2 EMB backfill: `session.restore` is embedded-current.
        assert!(embedded.contains(&"session.restore"));
        // - server dispatches the new names (the five batch-⑥ renames are
        //   settled); the old names and the removed retired rows are not on
        //   the server wire today.
        assert!(server.contains(&"session.beginCompaction"));
        assert!(server.contains(&"agent.activateSkill"));
        assert!(server.contains(&"btw.start"));
        assert!(server.contains(&"core.echo"));
        assert!(server.contains(&"approval.decide"));
        assert!(server.contains(&"workspace.createOrTouch"));
        // The B1 session vertical rides the serve wire too: the 5 serve
        // flips fall through to the engine, the 3 gap implementations and
        // `session.get` (which keeps its transport-owned serve handler) are
        // both-current.
        assert!(server.contains(&"session.getMeta"));
        assert!(server.contains(&"session.generateAgentsMd"));
        assert!(server.contains(&"session.waitForBackgroundTasksOnPrint"));
        assert!(server.contains(&"session.handlePrintMainTurnCompleted"));
        assert!(server.contains(&"session.addAdditionalDir"));
        assert!(server.contains(&"session.countActive"));
        assert!(server.contains(&"session.createChild"));
        assert!(server.contains(&"session.registerAgent"));
        assert!(server.contains(&"session.get"));
        // The B2 serve vertical rides the serve wire too: the 19 batch-2
        // embedded-only rows fall through to the engine (goal/swarm
        // engine-service extras, plan/cron/task/core/shell, agent.getModel).
        assert!(server.contains(&"agent.getModel"));
        assert!(server.contains(&"task.detach"));
        assert!(server.contains(&"cron.list"));
        assert!(server.contains(&"plan.enter"));
        assert!(server.contains(&"plan.cancel"));
        assert!(server.contains(&"plan.clear"));
        assert!(server.contains(&"plan.status"));
        assert!(server.contains(&"goal.create"));
        assert!(server.contains(&"goal.get"));
        assert!(server.contains(&"goal.pause"));
        assert!(server.contains(&"goal.resume"));
        assert!(server.contains(&"goal.cancel"));
        assert!(server.contains(&"swarm.enter"));
        assert!(server.contains(&"swarm.exit"));
        assert!(server.contains(&"swarm.get"));
        // The collaboration mode gate rides the serve wire the same way
        // (the EMB route via the engine fall-through).
        assert!(server.contains(&"collaboration.enter"));
        assert!(server.contains(&"collaboration.exit"));
        assert!(server.contains(&"core.info"));
        assert!(server.contains(&"core.experimentalFeatures"));
        assert!(server.contains(&"shell.run"));
        assert!(server.contains(&"shell.cancel"));
        // The B2 gap implementations are both-current (embedded route +
        // serve engine fall-through).
        assert!(embedded.contains(&"agent.getEffectiveThinkingLevel"));
        assert!(embedded.contains(&"agent.listCommands"));
        assert!(embedded.contains(&"agent.runCommand"));
        assert!(embedded.contains(&"task.stop"));
        assert!(server.contains(&"agent.getEffectiveThinkingLevel"));
        assert!(server.contains(&"agent.listCommands"));
        assert!(server.contains(&"agent.runCommand"));
        assert!(server.contains(&"task.stop"));
        // agent.activity (the B2 ActivityTracker-folded activity view) is
        // both-current: the embedded route serves it and the serve wire
        // rides the engine fall-through.
        assert!(embedded.contains(&"agent.activity"));
        assert!(server.contains(&"agent.activity"));
        // The B3 serve vertical rides the serve wire too: the 12 batch-3
        // embedded-only rows fall through to the engine (9 plugin methods +
        // 3 mcp methods).
        assert!(server.contains(&"plugin.list"));
        assert!(server.contains(&"plugin.install"));
        assert!(server.contains(&"plugin.setEnabled"));
        assert!(server.contains(&"plugin.setMcpServerEnabled"));
        assert!(server.contains(&"plugin.remove"));
        assert!(server.contains(&"plugin.reload"));
        assert!(server.contains(&"plugin.info"));
        assert!(server.contains(&"plugin.commands"));
        assert!(server.contains(&"plugin.activateCommand"));
        assert!(server.contains(&"mcp.list"));
        assert!(server.contains(&"mcp.startupMetrics"));
        assert!(server.contains(&"mcp.reconnect"));
        // The M1 mcp.tools row is both-current: the embedded route
        // serves it and the serve wire rides the engine fall-through.
        assert!(embedded.contains(&"mcp.tools"));
        assert!(server.contains(&"mcp.tools"));
        // The M5 mcp.config.preview row is both-current the same way.
        assert!(embedded.contains(&"mcp.config.preview"));
        assert!(server.contains(&"mcp.config.preview"));
        // The M6 mcp.sync row is both-current the same way (the CLI
        // watcher rides the same kernel).
        assert!(embedded.contains(&"mcp.sync"));
        assert!(server.contains(&"mcp.sync"));
        // The B3 gap implementation is both-current: the embedded route
        // serves the update check and the serve wire rides the engine
        // fall-through.
        assert!(embedded.contains(&"plugin.checkUpdates"));
        assert!(server.contains(&"plugin.checkUpdates"));
        // The B4-a interaction vertical: the 3 EMB backfills (SRV wire
        // shapes) and the 5 gap implementations (klient shapes over the
        // session's HITL gates) are both-current — the serve wire keeps
        // its transport-owned handlers for the backfills and rides the
        // engine fall-through for the gap rows.
        assert!(embedded.contains(&"approval.decide"));
        assert!(server.contains(&"approval.decide"));
        assert!(embedded.contains(&"question.answer"));
        assert!(server.contains(&"question.answer"));
        assert!(embedded.contains(&"question.dismiss"));
        assert!(server.contains(&"question.dismiss"));
        assert!(embedded.contains(&"approval.listPending"));
        assert!(server.contains(&"approval.listPending"));
        assert!(embedded.contains(&"question.listPending"));
        assert!(server.contains(&"question.listPending"));
        assert!(embedded.contains(&"interaction.listPending"));
        assert!(server.contains(&"interaction.listPending"));
        assert!(embedded.contains(&"interaction.respond"));
        assert!(server.contains(&"interaction.respond"));
        assert!(embedded.contains(&"interaction.isRecentlyResolved"));
        assert!(server.contains(&"interaction.isRecentlyResolved"));
        // The B4-b workspace vertical: the 4 EMB backfills (SRV wire
        // mirrors) and the 2 gap implementations (domain shapes) are
        // both-current.
        assert!(embedded.contains(&"workspace.list"));
        assert!(server.contains(&"workspace.list"));
        assert!(embedded.contains(&"workspace.createOrTouch"));
        assert!(server.contains(&"workspace.createOrTouch"));
        assert!(embedded.contains(&"workspace.update"));
        assert!(server.contains(&"workspace.update"));
        assert!(embedded.contains(&"workspace.delete"));
        assert!(server.contains(&"workspace.delete"));
        assert!(embedded.contains(&"workspace.get"));
        assert!(server.contains(&"workspace.get"));
        assert!(embedded.contains(&"workspace.handlerFor"));
        assert!(server.contains(&"workspace.handlerFor"));
        // The W8 trust management row is a both-faces rust-first keep.
        assert!(embedded.contains(&"workspace.setTrust"));
        assert!(server.contains(&"workspace.setTrust"));
        // The B5-a shallow batch A: the 21 gap implementations (13 env, 5
        // flag, 2 fs, 1 providerDiscovery) are both-current — the embedded
        // routes serve them and the serve wire rides the engine
        // fall-through.
        assert!(embedded.contains(&"env.platform"));
        assert!(server.contains(&"env.platform"));
        assert!(embedded.contains(&"env.clientIdentity"));
        assert!(server.contains(&"env.clientIdentity"));
        assert!(embedded.contains(&"env.sessionsDir"));
        assert!(server.contains(&"env.sessionsDir"));
        assert!(embedded.contains(&"flag.enabled"));
        assert!(server.contains(&"flag.enabled"));
        assert!(embedded.contains(&"flag.explainAll"));
        assert!(server.contains(&"flag.explainAll"));
        assert!(embedded.contains(&"fs.browse"));
        assert!(server.contains(&"fs.browse"));
        assert!(embedded.contains(&"fs.home"));
        assert!(server.contains(&"fs.home"));
        assert!(embedded.contains(&"providerDiscovery.refreshProviderModels"));
        assert!(server.contains(&"providerDiscovery.refreshProviderModels"));
        // And the P6 B4a browse rides the same both-current fall-through.
        assert!(embedded.contains(&"providerDiscovery.listCatalog"));
        assert!(server.contains(&"providerDiscovery.listCatalog"));
        // And the P7 B4b import rides it too (a config write through the
        // same engine fall-through).
        assert!(embedded.contains(&"providerDiscovery.importCatalog"));
        assert!(server.contains(&"providerDiscovery.importCatalog"));
        // The R4 Task 25 profile lifecycle rows are both-current rust-first
        // keeps: the embedded routes serve them and the serve wire rides
        // the engine fall-through.
        assert!(embedded.contains(&"agent.enableProfile"));
        assert!(server.contains(&"agent.enableProfile"));
        assert!(embedded.contains(&"agent.disableProfile"));
        assert!(server.contains(&"agent.disableProfile"));
        assert!(embedded.contains(&"agent.resetProfile"));
        assert!(server.contains(&"agent.resetProfile"));
        // The B5-b shallow batch B: the 14 gap implementations (5 config
        // section methods, 5 model record/catalog methods, 4 provider CRUD
        // rows) are both-current — the embedded routes serve them and the
        // serve wire rides the engine fall-through.
        assert!(embedded.contains(&"config.getAll"));
        assert!(server.contains(&"config.getAll"));
        assert!(embedded.contains(&"config.replaceSections"));
        assert!(server.contains(&"config.replaceSections"));
        assert!(embedded.contains(&"model.get"));
        assert!(server.contains(&"model.get"));
        assert!(embedded.contains(&"model.setDefaultModel"));
        assert!(server.contains(&"model.setDefaultModel"));
        assert!(embedded.contains(&"provider.list"));
        assert!(server.contains(&"provider.list"));
        assert!(embedded.contains(&"provider.set"));
        assert!(server.contains(&"provider.set"));
        assert!(embedded.contains(&"skill.list"));
        assert!(embedded.contains(&"skill.getContent"));
        assert!(embedded.contains(&"skill.setEnabled"));
        // The ADR-010 skill.reload row is both-current the same way (the
        // manual catalog-reload face riding the engine fall-through).
        assert!(embedded.contains(&"skill.reload"));
        assert!(server.contains(&"skill.reload"));
        // The session-retention vertical: all 6 rows are both-current
        // rust-first keeps (embedded route + serve engine fall-through).
        assert!(embedded.contains(&"retention.list"));
        assert!(server.contains(&"retention.list"));
        assert!(embedded.contains(&"retention.stats"));
        assert!(server.contains(&"retention.stats"));
        assert!(embedded.contains(&"retention.compressNow"));
        assert!(server.contains(&"retention.compressNow"));
        assert!(embedded.contains(&"retention.delete"));
        assert!(server.contains(&"retention.delete"));
        assert!(embedded.contains(&"retention.deleteTiered"));
        assert!(server.contains(&"retention.deleteTiered"));
        assert!(embedded.contains(&"retention.getPolicy"));
        assert!(server.contains(&"retention.getPolicy"));
        assert!(!server.contains(&"session.compact"));
        assert!(!server.contains(&"skill.activate"));
        assert!(!server.contains(&"session.startBtw"));
        assert!(!server.contains(&"approval.resolve"));
        assert!(!server.contains(&"workspace.create"));
        assert!(!server.contains(&"tool.call"));
        assert!(!server.contains(&"workspace.refresh"));
        assert!(!server.contains(&"plan.get"));
    }

    /// The authoritative method table (`authoritative-methods.tsv`) is the
    /// machine-readable home of the D4 adjudication, and `METHODS` must agree
    /// with it row-for-row: name, domain, disposition, availability, and the
    /// per-backend current wire names. The table and the registry are
    /// maintained together; this test fails on the first drift so the
    /// mismatch is attributable.
    #[test]
    fn should_agree_with_the_authoritative_method_table() {
        let tsv = include_str!("authoritative-methods.tsv");
        let rows: Vec<&str> = tsv
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
            .collect();

        assert_eq!(
            rows.len(),
            METHODS.len(),
            "tsv row count vs METHODS len — regenerate the table alongside the registry"
        );

        for (row, entry) in rows.iter().zip(METHODS.iter()) {
            let cols: Vec<&str> = row.split('\t').collect();
            assert_eq!(
                cols.len(),
                13,
                "row for {} must have 13 tab-separated columns, got {}: {row}",
                entry.name,
                cols.len()
            );

            let name = cols[0];
            let domain = cols[1];
            let batch = cols[2];
            let v2 = cols[3];
            let emb_current = cols[4];
            let srv_current = cols[5];
            let emb_current_name = cols[6];
            let srv_current_name = cols[7];
            let disposition = cols[8];

            assert_eq!(name, entry.name, "row order must match METHODS");
            assert_eq!(domain, entry.domain.name(), "domain mismatch for {name}");
            assert_eq!(
                batch.parse::<u8>(),
                Ok(entry.domain.batch().as_u8()),
                "batch mismatch for {name} — batch is the M2/M3 migration-ordering input"
            );
            assert_eq!(
                disposition,
                disposition_cell(entry),
                "disposition mismatch for {name}"
            );

            let expected_emb = if entry.availability.serves_embedded() {
                "Y"
            } else {
                "N"
            };
            let expected_srv = if entry.availability.serves_server() {
                "Y"
            } else {
                "N"
            };
            assert_eq!(emb_current, expected_emb, "emb_current mismatch for {name}");
            assert_eq!(srv_current, expected_srv, "srv_current mismatch for {name}");

            let expected_emb_name = current_cell(entry, RpcBackend::Embedded);
            let expected_srv_name = current_cell(entry, RpcBackend::Server);
            assert_eq!(
                emb_current_name, expected_emb_name,
                "emb_current_name mismatch for {name}"
            );
            assert_eq!(
                srv_current_name, expected_srv_name,
                "srv_current_name mismatch for {name}"
            );

            // The disposition / availability invariants every row must keep:
            // unimplemented rows are unavailable on both backends, and the
            // v2 column equals "not Retired and not RustFirst".
            if entry.disposition == MethodDisposition::Unimplemented {
                assert_eq!(
                    entry.availability,
                    BackendAvailability::Unimplemented,
                    "{name}"
                );
            }
            if entry.disposition == MethodDisposition::Retired
                || entry.disposition == MethodDisposition::RustFirst
            {
                assert_eq!(v2, "N", "v2 must be N for {name} (not in the v2 contract)");
            } else {
                assert_eq!(v2, "Y", "v2 must be Y for {name} (in the v2 contract)");
            }
        }

        // The v2 contract surface: every adjudicated row except the retired
        // and rust-first ones (215 − 23 − 35 = 157; the B1 gap
        // implementations were already v2=Y rows — the 4 batch-3 P1
        // MCP config rows registered v2=Y with their TS facade landing in
        // the same batch, and the 2 batch-4 P2 MCP pool rows register
        // v2=Y with the TS facade landing with the user-pool split, and
        // the M1 mcp.tools row registered v2=Y with its klient contract
        // landing in the same batch, and the M5 mcp.config.preview row
        // registered v2=Y with its klient contract landing in the same
        // batch).
        let v2_rows = rows
            .iter()
            .filter(|row| row.split('\t').nth(3) == Some("Y"))
            .count();
        assert_eq!(v2_rows, 157, "v2=Y row count");
    }

    /// TSV rendering of an entry's disposition cell.
    fn disposition_cell(entry: &MethodEntry) -> String {
        match entry.disposition {
            MethodDisposition::Settled => "Settled".to_string(),
            MethodDisposition::RustFirst => "RustFirst".to_string(),
            MethodDisposition::Retired => "Retired".to_string(),
            MethodDisposition::Unimplemented => "Unimplemented".to_string(),
        }
    }

    /// TSV rendering of the current wire name for `backend`, or `-` when the
    /// entry does not serve that backend (mirrors [`current_names_for`]).
    fn current_cell(entry: &MethodEntry, backend: RpcBackend) -> &'static str {
        let serves = match backend {
            RpcBackend::Embedded => entry.availability.serves_embedded(),
            RpcBackend::Server => entry.availability.serves_server(),
        };
        if serves { entry.name } else { "-" }
    }

    /// Every name is registered exactly once — duplicates would silently
    /// skew the counts above.
    #[test]
    fn should_register_every_method_name_exactly_once() {
        let mut seen = HashSet::new();
        for entry in all_methods() {
            assert!(
                seen.insert(entry.name),
                "method {} registered more than once",
                entry.name
            );
        }
    }

    /// Spot-checks tying registry facts to the source surfaces they were
    /// extracted from (embedded `call_method` match, mirri-server's
    /// transport registry behind the EngineAdapter seam) and to the M1.2-a
    /// rename fold.
    #[test]
    fn should_record_availability_facts() {
        // Shared core surface.
        assert_eq!(
            get("session.create").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("message.send").unwrap().availability,
            BackendAvailability::Both
        );
        // The M5-3b drill row is a both-method (one embedded route; the
        // serve wire rides the engine fall-through).
        assert_eq!(
            get("core.echo").unwrap().availability,
            BackendAvailability::Both
        );
        // Embedded-only (no server handler).
        assert_eq!(
            get("config.diagnostics").unwrap().availability,
            BackendAvailability::Embedded
        );
        // B4-b: the 4 EMB backfills mirror the SRV handlers, so the rows
        // are both-current now (the serve wire keeps its transport-owned
        // handlers).
        assert_eq!(
            get("workspace.list").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("workspace.createOrTouch").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("workspace.update").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("workspace.delete").unwrap().availability,
            BackendAvailability::Both
        );
        // B4-b: the 2 gap implementations are both-current (domain shapes;
        // the serve wire rides the engine fall-through).
        assert_eq!(
            get("workspace.get").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("workspace.handlerFor").unwrap().availability,
            BackendAvailability::Both
        );
        // W8: the trust management face is a both-faces rust-first keep
        // (embedded route; the serve wire rides the engine fall-through).
        assert_eq!(
            get("workspace.setTrust").unwrap().availability,
            BackendAvailability::Both
        );
        // B4-a: the EMB backfill mirrors the SRV handler, so the row is
        // both-current now (the serve wire keeps its transport-owned
        // handler).
        assert_eq!(
            get("approval.decide").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("question.answer").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("question.dismiss").unwrap().availability,
            BackendAvailability::Both
        );
        // B4-a: the 5 gap implementations are both-current (the klient
        // shapes over the session's HITL gates; the serve wire rides the
        // engine fall-through).
        assert_eq!(
            get("approval.listPending").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("question.listPending").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("interaction.listPending").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("interaction.respond").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("interaction.isRecentlyResolved").unwrap().availability,
            BackendAvailability::Both
        );
        // Rename folds: the target rows gained a backend side; the old names
        // are no longer primary rows.
        assert_eq!(
            get("session.beginCompaction").unwrap().availability,
            BackendAvailability::Both
        );
        // B1: the session.get EMB backfill is both-current (the serve wire
        // keeps its transport-owned handler).
        assert_eq!(
            get("session.get").unwrap().availability,
            BackendAvailability::Both
        );
        // B1: the 3 session gap implementations are both-current.
        assert_eq!(
            get("session.countActive").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("session.createChild").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("session.registerAgent").unwrap().availability,
            BackendAvailability::Both
        );
        // B1: the 5 serve flips are both-current (embedded route + serve
        // engine fall-through).
        assert_eq!(
            get("session.getMeta").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("session.addAdditionalDir").unwrap().availability,
            BackendAvailability::Both
        );
        // B2: the 19 serve flips are both-current (embedded route + serve
        // engine fall-through; goal/swarm engine-service extras' first
        // serve exposure, plan.enter/cancel/clear/status, cron.list,
        // task.detach, shell.run/cancel, core.info/experimentalFeatures,
        // agent.getModel — including plan.status, renamed from plan.get in
        // M3-c).
        assert_eq!(
            get("plan.status").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("plan.enter").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("plan.cancel").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("plan.clear").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("goal.create").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("goal.get").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("goal.pause").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("goal.resume").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("goal.cancel").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("swarm.enter").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("swarm.exit").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("swarm.get").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("collaboration.enter").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("collaboration.exit").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("cron.list").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("task.detach").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("shell.run").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("shell.cancel").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("core.info").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("core.experimentalFeatures").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("agent.getModel").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("agent.activateSkill").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("btw.start").unwrap().availability,
            BackendAvailability::Both
        );
        // B2: the 4 gap implementations are both-current (embedded route +
        // serve engine fall-through).
        assert_eq!(
            get("agent.getEffectiveThinkingLevel").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("agent.listCommands").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("agent.runCommand").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("task.stop").unwrap().availability,
            BackendAvailability::Both
        );
        // agent.activity — the B2 gap implementation: the engine folds a
        // per-agent ActivityTracker (mirri-core, TurnFlow/TurnFlowHooks
        // observation points) and both backends serve the snapshot.
        assert_eq!(
            get("agent.activity").unwrap().availability,
            BackendAvailability::Both
        );
        assert!(
            get("agent.activity")
                .unwrap()
                .availability
                .serves_embedded()
        );
        assert!(get("agent.activity").unwrap().availability.serves_server());
        // B3: the 12 serve flips are both-current (embedded route + serve
        // engine fall-through; the 9 plugin methods ride the same path as
        // their EMB answers — empty list / not_implemented without a
        // plugin_manager — and the 3 mcp methods likewise answer through
        // the engine's MCP manager).
        assert_eq!(
            get("plugin.list").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("plugin.install").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("plugin.setEnabled").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("plugin.setMcpServerEnabled").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("plugin.remove").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("plugin.reload").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("plugin.info").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("plugin.commands").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("plugin.activateCommand").unwrap().availability,
            BackendAvailability::Both
        );
        // B3: the gap implementation is both-current (embedded route +
        // serve engine fall-through — the update check reads the plugin
        // store and the injectable GitHub resolver, never the transport).
        assert_eq!(
            get("plugin.checkUpdates").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("mcp.list").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("mcp.startupMetrics").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("mcp.reconnect").unwrap().availability,
            BackendAvailability::Both
        );
        // M1: the single-server tool listing is both-current (embedded
        // route + serve engine fall-through).
        assert_eq!(
            get("mcp.tools").unwrap().availability,
            BackendAvailability::Both
        );
        // M5: the resolved-value preview is both-current the same way.
        assert_eq!(
            get("mcp.config.preview").unwrap().availability,
            BackendAvailability::Both
        );
        // M6: the idempotent reconcile entry is both-current (embedded
        // route + serve engine fall-through).
        assert_eq!(
            get("mcp.sync").unwrap().availability,
            BackendAvailability::Both
        );
        // M7: the app-level event subscription surface is server-only (a
        // WebSocket connection registers on the engine's pool broadcast —
        // the embedded engine exposes it through the EngineAdapter seam,
        // not a wire route).
        assert_eq!(
            get("mcp.subscribe").unwrap().availability,
            BackendAvailability::Server
        );
        // B4-b moved the workspace rows to both (see the facts above).
        assert!(get("plan.get").is_none());
        assert!(get("session.compact").is_none());
        assert!(get("skill.activate").is_none());
        assert!(get("session.startBtw").is_none());
        assert!(get("approval.resolve").is_none());
        assert!(get("workspace.create").is_none());
        // B5-a: the 21 shallow-batch gap implementations are both-current
        // (embedded routes + serve engine fall-through).
        assert_eq!(
            get("env.platform").unwrap().availability,
            BackendAvailability::Both
        );
        assert!(get("env.platform").unwrap().availability.serves_embedded());
        assert!(get("env.platform").unwrap().availability.serves_server());
        assert_eq!(
            get("env.clientIdentity").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("flag.explain").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("flag.snapshot").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("fs.browse").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("fs.home").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("providerDiscovery.refreshProviderModels")
                .unwrap()
                .availability,
            BackendAvailability::Both
        );
        // P6 B4a: the directory browse is both-current (the same engine
        // fall-through as the B5-a refresh row).
        assert_eq!(
            get("providerDiscovery.listCatalog").unwrap().availability,
            BackendAvailability::Both
        );
        // P7 B4b: the directory import is both-current (the same engine
        // fall-through — a config write through the single transaction
        // pipeline).
        assert_eq!(
            get("providerDiscovery.importCatalog").unwrap().availability,
            BackendAvailability::Both
        );
        // B5-c: model.generate is both-current (the streaming contract,
        // embedded chunk broadcast + aggregated response, serve wire via
        // the engine fall-through).
        assert_eq!(
            get("model.generate").unwrap().availability,
            BackendAvailability::Both
        );
        // B5-b: the 14 shallow-batch-B gap implementations are both-current
        // (embedded routes over the config.toml section/record pipeline +
        // serve engine fall-through).
        assert_eq!(
            get("config.getAll").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("config.inspect").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("config.replace").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("model.get").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("model.getProvider").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("provider.get").unwrap().availability,
            BackendAvailability::Both
        );
        assert_eq!(
            get("provider.delete").unwrap().availability,
            BackendAvailability::Both
        );
        // Unknown methods are absent, not defaulted.
        assert!(get("no.such.method").is_none());
    }

    /// M1.1/M1.2-a registers every method with the shared-mapping error
    /// semantics (D3: per-method declarations arrive with the M2/M3 domain
    /// moves).
    #[test]
    fn should_default_every_entry_to_the_shared_error_mapping() {
        for entry in all_methods() {
            assert_eq!(
                entry.error_semantics,
                MethodErrorSemantics::SharedMapping,
                "{} must default to the shared D5 mapping",
                entry.name
            );
        }
    }
}
