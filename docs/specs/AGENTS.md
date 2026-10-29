# docs/specs/ Documentation Governance Conventions

> Purpose: separation of design from execution — spec documents change only when the **design changes**, never because **execution progressed**.

> This is the English version; the Chinese original is kept at [`AGENTS_CN.md`](./AGENTS_CN.md) (authoritative in case of divergence).

## Document Types

Four types of documents are allowed under `docs/specs/`:

| Type | Content | Allowed modification trigger |
|---|---|---|
| **Design/Spec** (`*-requirements.md`, `*-architecture.md`, `rust-core-design/0*.md`, plus registered in-domain design subclasses — currently `tool-calling-builtin-tools.md` (per-tool interface spec), `skills-builtin-skills.md` (per-builtin-skill requirements spec), and `file-reference-uiux.md` (per-domain interaction contract for the file-reference feature) | **Final state only**: goal descriptions, contracts, requirements, architecture decisions, trade-offs, risks | Only when the design itself changes (new/revised decisions, requirement changes — appended as SDR/ADR, see below) |
| **Reference** (`*-context.md`) | Inventories of current state and current pain points (code facts, side-by-side diff matrices, recon output, gap overviews) | Updated as recon/current state changes; each carries a snapshot date |
| **Plan** (`*/plans/*.md`, `{domain}-ROADMAP.md`; a directory-level `ROADMAP.md` inside a domain directory is equivalent; `proposals.md` — the cross-domain un-adjudicated proposal registry) | Migration plans, phase splits, open-item rollups, execution status | Updated freely as execution progresses |
| **Shared terminology** (`glossary.md`) | The single canonical definition of terms that cross domain boundaries (canonical name, shape/example, where the term appears, banned synonyms, same-name-different-meaning warnings) | Updated in the **same change** as the domain spec that introduces or revises a cross-domain term (see "Cross-Domain Terminology") |

## The Four Prohibitions in Design Documents

Design/spec documents **must not contain**:

1. **Execution status elements**: implemented / not started / in progress / delivered / to-be-implemented / ✅ status words; baseline numbers that drift (total test counts); phase progress annotations
2. **Current-state inventories**: code fact references (file:line-level "what exists today"), side-by-side implementation diff matrices, current-state gap overviews — these belong in `*-context.md`
3. **Migration narrative**: process phrasing like "aligns with current state / revised / supersedes the original route"
4. **Technical implementation details**: interface method names and signatures, implementation mechanisms, system structure descriptions — these belong in `*-architecture.md`

**Product-language requirement for requirement docs**: `*-requirements.md` must be written in **product language readable by non-technical people** — describing what users want and what will be accepted, not how the system implements it. Technical terms must be translated into product semantics (e.g., "optimistic locking" is expressed as "concurrent-edit conflict detection", "idempotent reconciliation" as "only patches the diff, zero cost when nothing changed"); technical shapes of interfaces (method names, parameters, return structures, mechanisms) belong in `*-architecture.md`, which the requirement doc points to in one sentence. Litmus test: hand the document to a product manager — it only passes if they can read it and spot errors without an engineer in the room.

**Motivation vs current state boundary**: design documents may (and should) retain **motivation** — but it must be an **abstract problem statement** ("with multiple workspaces in one process, a user-level server gets spawned M times as duplicate subprocesses"), carrying no code facts (file paths, line numbers, method lists, numbers). Litmus test: motivation states a problem (which disappears once the final state is reached); a current-state inventory states facts (which drift as the code evolves). Detailed current-state evidence lives in the corresponding `*-context.md`, pointed to from the design doc in one sentence.

## Conventions for Reference Documents (`*-context.md`)

- One per domain, sitting alongside that domain's design documents (`mcp-architecture.md` ⇄ `mcp-context.md`; the `rust-core-design/` domain keeps a single unified `context.md`)
- Snapshot date in the header; content organized in domain sections
- It describes no goals and contains no decisions — those live in design documents
- The relationship between a design document and its context is one-way referencing (design → context); the context makes no decisions and plans nothing

## The Single Home for Status

| Status kind | Single maintenance point |
|---|---|
| Per-domain open items and execution status | That domain's ROADMAP: top-level domains use `{domain}-ROADMAP.md` (e.g. `mcp-ROADMAP.md`); inside domain directories use a directory-level `ROADMAP.md` (e.g. `rust-core-design/ROADMAP.md` — the two are equivalent) |
| Per-batch execution ledger | `.superpowers/sdd/<program>/progress.md` (gitignored) |
| Per-domain current-state inventory | `*-context.md` (with snapshot date) |
| Code reality (test counts, registry line counts) | The code and CI themselves |

ROADMAPs and plan files do status/progress management only and must not carry requirements or design content; conversely, requirement/design documents must not carry any progress information.

## Cross-Domain Terminology

Some terms are bigger than one domain — "model" appears in config.toml, the engine, the wire contract, web/TUI, and agent tools. Such terms get **one** canonical definition in `glossary.md`:

1. **Collection criterion** (either one qualifies): the term appears in two or more domains' specs, or on a **shared surface** (config.toml keys/values, wire contract fields, user-visible CLI/web/TUI copy).
2. **Single source of truth**: domain specs may keep a local view of a glossary term (trim columns, add domain context) but must not redefine it; on conflict, `glossary.md` wins.
3. **Same-change registration**: a spec change that introduces or revises a cross-domain term must update `glossary.md` in the same change — never "spec first, glossary later".
4. **Domain-private terms** stay in the owning domain's spec; do not pour them into the glossary (a glossary of everything drifts into a stale dump).
5. **Boundary: terminology only**. Glossary entries carry the canonical name, shape/example, definition, where the term appears, banned synonyms, and same-name-different-meaning warnings — **no code, no implementation mechanisms, no decisions**: code facts and domain context belong in the domain's spec/context, mechanisms and constraints in the architecture doc. A code block, method name, or file path in the glossary is a defect.

## Requirement/Design Change Records (SDR / ADR)

When a requirement or design changes, the change must be **written back into the corresponding requirement/design document itself**, appended as a decision record — it is not allowed to exist only in a ROADMAP, a plan file, a commit message, or a conversation:

- **Requirement change** → appended to the corresponding `*-requirements.md`, recorded as an **SDR** (Specification Decision Record)
- **Design change** → appended to the corresponding `*-architecture.md` / design document, recorded as an **ADR** (Architecture Decision Record)

Each SDR/ADR contains at least:

1. **Number and date** (e.g. `SDR-001`, `ADR-003`)
2. **Context and motivation**: abstract problem statement, no code facts (current-state evidence points to `*-context.md`)
3. **Decision**: the post-change final-state requirement
4. **Rejected alternatives** and the trade-off rationale
5. **Impact scope**: which requirement/design items are affected

History is **appended, never rewritten**: entries superseded by later decisions are marked superseded with a pointer to the new record, and the body entries are updated to the final state in sync.

## Review Checklist for Requirement Definitions

When writing or reviewing any requirement document (`*-requirements.md`), self-check item by item — any judgment question answered "no / unsure" is a defect; fix before submitting (rule origin: review viewpoints established in the 2026-09-08 MCP settings-page requirements review):

| # | Rule | Judgment question |
|---|---|---|
| R1 End consumer | Change-type requirements must be written down to the real consumer — executing tasks, call pipelines, UI; the acceptance subject is the consumer, not an intermediate layer (connection, config surface) | After this change lands, **who** perceives it **at what moment**? Intermediate-layer phrasing like "the available surface / connection has been updated" does not pass |
| R2 Visibility ≠ enforcement | Security/permission requirements must define hard enforcement points (the call-validation stage, prior to connection establishment/reuse), with interception and pass-through defined **symmetrically** | Will calling directly, bypassing the list, be blocked? At which pipeline stage is the enforcement point? Is the pass-through after revocation also defined? |
| R3 Concreteness | Acceptance of UI/interaction requirements must map item by item to concrete behavior | Can you draw a "what changed → how the UI changes" comparison table? Are update principles (who triggers / which side wins / zero action when no change) written? "Real-time refresh" or "instant update" appearing alone does not pass |
| R4 Signal truthfulness | Every user-perceivable signal (button, log, toast) must match the real effect | Does what it claims to happen actually happen? Does a single-target operation produce a global/misleading signal? |
| R5 Availability honesty | No "exists but broken" features on the delivered surface | Is the backend capability behind every UI entry complete? Either fully usable or the entry does not exist — a placeholder control that "errors when clicked" is a defect, not a degradation |
| R6 Analyze before ruling | Proposals to "remove/simplify" must come with objective analysis first | Are all five questions answered — what is the current mechanism? Which stages could break? Which external participants exist? How far does the automatic mechanism cover? Is a manual escape hatch needed? Do not propose removal without full answers |
| R7 Precedent exhaustion | Before asking the user to decide, exhaust the existing facts first | Have you checked the existing implementation's behavior? Read the existing spec documents? Does the question carry factual evidence, instead of outsourcing "didn't check" into the user's options? |
| R8 Language layering | Requirement documents and decision questions use audience-appropriate language; technical details belong in architecture documents | Can a product manager read it without an engineer present? Are there untranslated technical terms in the decision question? |
| R9 Explicit recording | Requirements must be explicitly recorded as first-class, acceptance-testable items | Implicit semantics hidden in other items' wording do not count — scattered indirect phrasing must be converged into explicit items |

R1–R3 together form the completeness check: the reach of a change-type requirement must cover **runtime tasks (R1) → call enforcement (R2) → UI presentation (R3)** — three consumers, with both directions (appear/remove, enable/disable) defined symmetrically.

**Acceptance sentence form**: acceptance criteria and Given-When-Then-style conditions are written in Chinese word order — 「**假如**…、**当**…，**那么**…」— never as inline English `Given/When/Then` markers inside Chinese sentences (unreadable mixed script; 2026-10-28 rule, provider-credential review). The English term "Given-When-Then" may appear only when naming the format itself.

## Checks When Modifying Design Documents

Before submitting a design document change, ask yourself:

1. Will this paragraph still hold six months from now? (No → it's a status or current-state element; move or delete it)
2. Is this "what to achieve" or "what currently is"? (What currently is → move to `*-context.md`)
3. If it's motivation: can the file paths/line numbers/numbers be removed, leaving only the problem itself? (Yes → keep it, abstracted)
4. Is this change a requirement/design change? (Yes → append the corresponding SDR/ADR in this document, rather than writing it only into a ROADMAP or commit message)
5. Does every identifier-like term in this document resolve to exactly one definition? (No → fix the wording or add a disambiguation note before submitting. The test: could two readers map a term to two different values — e.g. reading "alias id" as either the full `provider/model` string or the raw model name — or could one reader mistake a display name for a valid parameter value? If so, give the term a canonical name with a concrete shape/example in a terminology table, ban self-invented synonyms in the body, and explicitly flag same-name-different-meaning collisions, especially words reused across wire fields, RPC parameters, and config keys. If the term crosses domain boundaries, its canonical home is `glossary.md` — register/verify it there in the same change)
