# Ocean Crew Orchestration and Durable Workflow Manifest

**Status:** accepted by the operator 2026-07-21 — ratifies §12 items (a)–(i); Stage A authorized. Stages B–E still require their own implementation manifests before code.
**Date:** 2026-07-18
**Revision:** 2026-07-21 — operator-ratified lane names **Undertow** (local) / **Offshore** (remote) with facade tools renamed accordingly; normative member completion envelope + acceptance ledger (§7.7) and budget ladder / attention states / member control (§7.8); artifact topology snapshot and Surface flow-graph sidebar projection (§6.4, §7.6); durable per-attempt facts, batched continuations, grant rows (§7.4–7.5); informative pi-subagents concept study (§13)
**Scope:** design ratification only — no code changes are authorized by this document alone
**Parent contracts:** [`2026-07-14-ocean-extensions-architecture-and-migration-manifest.md`](2026-07-14-ocean-extensions-architecture-and-migration-manifest.md) (extension architecture; Phase 6 requires this ratification), [`2026-07-17-ocean-observatory-architecture.md`](2026-07-17-ocean-observatory-architecture.md) (read-only execution truth), [`2026-07-17-observatory-gate0-decisions.md`](2026-07-17-observatory-gate0-decisions.md), [`2026-07-17-observatory-gate1-implementation-manifest.md`](2026-07-17-observatory-gate1-implementation-manifest.md)

---

## 1. Decision

Ocean gains durable multi-agent orchestration as an **installed extension** — working name **Ocean Crew** (`risingtides.ocean-crew`) — that executes bounded, typed, SQLite-durable task graphs over ordinary Ocean turns. The operator-facing delegation facade is the Longhouse namespace, exposed by the Crew extension as `longhouse__delegate_undertow` and `longhouse__delegate_offshore`. The two execution lanes carry operator-ratified names: **Undertow** — local delegation as ordinary daemon child turns — and **Offshore** — remote delegation over the existing permission-gated remote-compute primitives. The compiled `ocean-longhouse` crate remains advisory/read-only and does not become an orchestrator. The host (`ocean-os`) gains only **generic, orchestration-agnostic seams**; it never learns what a crew, worker, subagent, join, graph, undertow delegate, or offshore delegate is.

This manifest is the separate design ratification that Phase 6 of the extension architecture requires for "the future orchestration transport, persistence model, and correlation contract." It also formally relocates the June 2026 durable-workflow design (typed async DAG, SQLite run-state, idempotent retries, closeouts — the "R5 workflow engine" recorded in `ocean-agents/docs/AGENT_FILESYSTEM_ARCHITECTURE.md`, commit `115dc4c`, and `ocean-agents/docs/ocean-agents-builds.toml` entry `R5`) from its original placement as a new `ocean-os` core crate to extension ownership. The R5 design is absorbed, not discarded: its persistence and recovery model becomes Crew's internal state machine.

## 2. Why a manifest, not an implementation wave

- Phase 6 of the extension manifest explicitly forbids building orchestration transport without ratified design.
- The extension host is not ready: Phase 1 (schema/tool lane) is implemented but not accepted; Phases 2–5 (lifecycle observers, supervised services, package management, reference packages) are pending. Crew depends on them.
- The interaction surface (interactive pinned workflow artifacts) touches `ocean-os`, `ocean-surface`, and the render protocol at once; an unratified contract would fragment across repos.
- Prior art must be reconciled, not duplicated: Bedrock workflow specs (data), the OCEAN-338/340 `WorkflowBrief` loader and `POST /v1/workflows/prepare` (discovery), and the R5 design (execution, never built) are three layers of one system.

## 3. Vocabulary

- **Crew** — one extension-owned orchestration job: a proposed, staged, or running task graph plus its members and results.
- **Task graph** — the extension's control-flow graph: nodes, edges, joins, bounded cycles, ceilings. Extension vocabulary only.
- **Member** — one graph node bound to a packaged folder-agent definition plus per-run overrides (task, model, budget).
- **Execution graph** — the host/Observatory record of what actually ran: execution ids, parent/child edges, lifecycle facts. Host vocabulary only.
- **Workflow spec** — portable declarative workflow data (Bedrock `workflows/*.workflow.json`, private `risingtides-agents` repo `docs/orchestrator/workflows/*.md`). Input to Crew, never authority.
- **Staging artifact** — the single pinned, interactive UI component through which the operator inspects, edits, starts, or stops a proposed crew.
- **Continuation** — an ordinary, package-attributed, deduplicated turn the extension requests in the root session to deliver aggregated results.
- **Delegation facade** — the two extension-provided Longhouse tools, `longhouse__delegate_undertow` and `longhouse__delegate_offshore`, that accept one bounded task or graph proposal and route it to Crew. They are not core daemon APIs.
- **Undertow / Offshore** — the ratified names of Crew's two execution lanes. Undertow runs a member as an ordinary local daemon child turn; Offshore routes a member through the existing permission-gated remote-compute provider adapter. Lane selection never widens capabilities. (The initial draft's `local` label is superseded.)
- **Execution target** — a member's extension-owned lane choice: `undertow` or `offshore`.
- **Member completion envelope** — the bounded, typed result a member attempt returns: inline summary under byte/line ceilings, optional schema-validated structured output, artifact and session references, usage, per-attempt model record, and budget outcomes. Never a transcript copy.
- **Acceptance ledger** — Crew's durable record of how a member result earned trust: claimed → attested → checked → verified → reviewed → accepted/rejected, with evidence, parent-run verification results, and the root/operator decision.
- **Capability profile** — the named, extension-owned set of real Ocean tools requested for a member role. It is an upper-bound request, never a grant; the host computes the effective set.

## 4. Lineage and reconciliation

Three existing layers are reconciled by this design; none is deleted by it.

| Layer | Artifact | Status | Role under this manifest |
| --- | --- | --- | --- |
| Spec (data) | `ocean-bedrock/workflows/*.workflow.json`, private `risingtides-agents` `docs/orchestrator/workflows/*.md` | shipped | Portable workflow definitions Crew may consume as proposals; never scheduler authority |
| Discovery (advisory) | `crates/ocean-longhouse/src/prepare.rs` `WorkflowIndex` (OCEAN-338), `POST /v1/workflows/prepare` (OCEAN-340) | shipped, read-only | Ranks candidate workflow specs for a turn; feeds the root agent's crew proposals; unchanged |
| Execution (durable) | R5 `Workflow`/`Step`/`Contract` + SQLite run-state design | proposed, never built | Becomes Crew's extension-internal engine; does **not** become a core crate |
| Delegation metadata (compatibility) | `POST /v1/subagents/spec`, `SubagentSpec.allowed_tools` | shipped, advisory only | Migration input only; current abstract tool-name strings do not spawn workers or grant executable tools and must not be treated as the Crew capability contract |

Compatibility surfaces (`/v1/subagents/spec`, `AgentDef.subagents`, filesystem `subagents/` discovery) remain metadata-only pending their separately approved migration, exactly as the extension manifest states.

## 5. Ownership boundaries

### 5.1 `ocean-os` (host)

Owns, unchanged in authority: sessions, turns, tools, permissions, cwd/workspace binding, secrets, cancellation, model routing, ceilings, audit identity, scoped event delivery, Observatory admission and truth. Gains only the generic seams in §6. Explicitly never gains: `Crew`, `Worker`, `Subagent`, `GraphNode`, `Join`, `task`, `spawn_worker`, a fleet scheduler, or a named-worker runtime.

### 5.2 Longhouse delegation facade (extension-provided)

The Crew package registers `longhouse__delegate_local` and `longhouse__delegate_offshore` through the generic extension tool lane. Longhouse owns the product vocabulary, advisory preparation, relevant skill/SOP retrieval, and delegation request shape; Crew owns every execution decision behind the facade. The existing `ocean-longhouse` core crate gains no spawning, scheduling, lifecycle, or remote-compute code.

### 5.3 Ocean Crew (extension)

Owns and tests: member roles/definitions/prompts, capability profiles and local/offshore target adapters, task-graph shape and validation, spawn/join/retry/cycle semantics, budgets and recursion policy, grace-period staging, result aggregation, its own SQLite durable state, staging-artifact content, and extension-attested topology labels.

### 5.4 `ocean-agents`

Owns deployed agent packages and typed contract fixtures (builds A8/A12). Crew consumes packaged folder-agent definitions through the host; it does not read `ocean-agents` conventions directly.

### 5.5 `ocean-bedrock`

Owns portable workflow specs as shared data, relevant skills/SOP knowledge, and optional discovery metadata. Crew may receive a bounded, provenance-labeled Bedrock context bundle prepared for a member; Bedrock never carries live grants, permission decisions, secrets, or scheduler state.

### 5.6 `ocean-surface`

Renders: the interactive workflow artifact on Surface, and Ocean Floor as the read-only execution-graph view. No orchestration logic.

## 6. Host seam contract (generic, orchestration-agnostic)

Six seams, each meaningful for any extension (deploys, imports, approval flows), none interpreting orchestration vocabulary. All are gated by activation grants and ordinary permission policy; none can widen grants.

1. **Extension execution request.** An activated service requests an ordinary turn: package/actor audit identity, workspace/session binding, requested provider route, model and capabilities, opaque correlation id, idempotency key, optional Observatory execution binding. Returns a durable host execution id. Provider route is generic capability-provider identity, not a core `local`/`offshore` enum. The host computes effective capabilities as `member request ∩ parent-delegable capabilities ∩ extension activation grants ∩ operator policy ∩ workspace/session ceilings`; absent or unknown capabilities fail closed and target selection cannot widen the result.
2. **Generic cancellation.** Cancel by host execution id. Host remains the process/session cleanup authority.
3. **Scoped lifecycle delivery.** The service receives lifecycle facts only for executions it owns or was granted: turn start/finish, tool start/finish metadata, permission waiting/resolved, cancellation, model reroute, session interruption. Extends the Phase 2 observer vocabulary; no transcript or argument payloads beyond the ratified metadata envelope.
4. **Extension UI artifact lane.** Publish/update/unmount one session-scoped interactive artifact per artifact id; receive authenticated, revisioned, idempotent operator commands for owned artifacts. Producer identity and session scope are host-injected into the envelope, never extension-asserted. Artifacts recover as inert (`paused`) after service restart until re-validated.
5. **Continuation request.** Request one ordinary, package-attributed turn in an originating session, carrying bounded structured results. Deduplicated, rate-limited, audit-visible, rejected if the session no longer permits it. Not an interceptor and not context injection.
6. **Extension state directory.** A confined per-package state directory (under the daemon-owned local state root from §12.1 of the extension manifest). Crew keeps SQLite here. Extensions never touch daemon session JSON.

The interactive artifact lane additionally requires one generic render-protocol kind (working name `workflow_control`: phase, deadline, rows with bounded editable fields, actions, revision, and a bounded topology snapshot — node ids, kinds, labels, edges, per-node lifecycle/acceptance/attention status — sufficient for graph rendering without transcript content) rendered by the TUI pinned slot and by Surface. The existing TUI gap — confirm interactions resolve locally and never POST to `/v1/component/event` (`crates/ocean-tui/src/shell/components/chat.rs`) — is closed as part of this seam, benefiting all components, not just Crew.

## 7. Extension design

### 7.1 Longhouse delegation facade and target flow

The Crew extension publishes exactly two explicit facade tools:

```text
longhouse__delegate_undertow(task, role, capability_profile, model_policy?, budget?, output_schema?)
longhouse__delegate_offshore(task, role, capability_profile, model_policy?, budget?, output_schema?)
```

Both calls create or revise a Crew proposal and return its durable proposal id/status; neither bypasses staging, grace, operator edits, or ordinary permission checks. A one-member delegation is a one-node Crew graph, not a separate runtime. `undertow` submits each admitted member through the generic host execution-request seam as an ordinary workspace-bound child turn. `offshore` routes the admitted member through Crew's adapter over the existing permission-gated `offshore_*` provider primitives; one remote session is used per job, ship/fetch remains explicit, remote credentials never enter prompts, and the adapter must report the same durable lifecycle/result envelope. Offshore is a lane choice, not a larger trust zone.

The root request contributes only the bounded task, selected target/profile, workspace/session binding, budgets, and optional structured output schema. Longhouse preparation may attach a compact provenance-labeled bundle of relevant Bedrock skills/SOPs and parent-approved context. Raw parent transcripts, secrets, ambient memory, and unrelated Bedrock records are excluded by default. Repository or Bedrock text is untrusted task data, never authority to widen tools, alter role, or bypass review.

### 7.2 Real tool capability profiles

Crew role definitions request canonical Ocean capability ids that the host can actually assemble; they do not reuse the compatibility `/v1/subagents/spec` aliases (`read_file`, `list_dir`, `run_command`) unless a separately versioned migration maps them. Initial profiles are bounded upper limits:

| Profile | Requested Ocean tools | Mutation posture |
| --- | --- | --- |
| `planner` / `researcher` | `read`, `ls`, `grep`, `glob`, `web_fetch`, `lsp` | Read-only; emits a structured plan/result through the member completion envelope, not a repository file |
| `implementer` | planner set + `edit`, `write`, `bash` | Mutations and commands remain individually permission-gated and workspace-bound |
| `reviewer` | `read`, `ls`, `grep`, `glob`, `lsp`, bounded `bash` for checks | No source edits by default; findings/result only |
| `synthesizer` | bounded upstream member results plus `read` when explicitly needed | No mutation by default |

Profiles are extension-owned, versioned data. Every attempt records requested and host-effective capability ids. Unknown ids, missing provider support, changed grants, or a target unable to enforce the effective set block admission/start rather than silently dropping controls. The child sees only tools in the host-effective set; tool descriptions and provider adapters cannot synthesize aliases that bypass it. Local and offshore attempts with the same profile must be policy-equivalent even when their physical implementations differ.

### 7.3 Graph model (v1)

Node kinds: `agent`, `join` (all/any/threshold), `reduce`, `checkpoint` (operator approval), `finish`. Edge semantics: `success`, `failure`, `cancelled`, `condition`, `retry`, with dependency/data mapping. Graph policy ceilings, validated before staging and again before start:

```text
max_nodes 8 · max_parallel 3 · max_depth 1 · max_cycles 1 (review/revise only)
max_dynamic_expansion 0 (v1) · aggregate turn/token/tool budgets · required deadline
failure_policy: fail | continue_partial
```

Runtime states: `proposed → staged → running → (waiting_for_dependency | waiting_for_permission | waiting_for_operator) → completed | completed_partial | failed | cancelled`.

### 7.4 Durability (the absorbed R5 model)

- SQLite in the confined state directory; every meaningful transition checkpointed in a transaction: graph revision, node state, attempt number, input/output references, host execution id, command-dedup ids, per-attempt model record (requested role→alias resolution, tried model, failure class), requested and host-effective capability ids, budget-ladder outcomes, acceptance-ledger status, operator grant rows, aggregate usage, pending operator action.
- **At-least-once recovery:** execution requests carry idempotency keys; UI commands carry command ids; retries are explicit new attempts with new host execution ids; completion reconciliation queries host truth before relaunching anything.
- **Immutable revisions:** operator edits and (later) planner expansions create a new validated graph revision; running nodes stay bound to the revision that admitted them.
- **Closeouts:** every terminal state runs a closeout (cancel orphans, release artifacts, final ledger row) — the R5 closeout policy, extension-side.

### 7.5 Staging UX and grace safety

Flow: during a normal turn the root agent calls `crew.propose` (validate → persist → publish staging artifact → return immediately, "starts in Ns unless changed"); the root turn finishes normally; the supervised service owns the countdown, launch, supervision, and the final continuation.

Grace policy: first-install default manual start; `auto_start_after_grace` is an operator setting (default grace 7s); countdown starts only after an authorized surface acknowledges the artifact; editing pauses and resets it; unavailable model, changed grants, stale revision, or failed re-validation blocks start; restarts never retroactively launch an expired proposal — staged crews recover paused. Countdown derives from one absolute deadline; the extension emits state changes, never per-second events. The artifact always displays aggregate ceilings; exceeding the displayed envelope requires a fresh start action, and each such raise is recorded as a durable operator grant row that never applies retroactively to a running attempt. Ordinary permission gates still apply to every member turn.

Continuation batching: sibling member completions arriving within a short debounce window group into one continuation; failures, cancellations, and needs-attention signals bypass batching and may fire immediately, still deduplicated and rate-limited by the host continuation seam.

### 7.6 Observatory relationship

Proposals are extension control state and never enter Observatory. At launch, Crew requests Observatory admission per child attempt (host-minted execution/root/edge ids, one-time binding), submits the ordinary child turn with that binding, and keeps its private member↔execution mapping. The staging artifact merges extension-attested topology with host-observed lifecycle; Ocean Floor shows the same children through its existing read model, deep-linked by `root_execution_id`. Observatory routes carry no control commands — the Gate 0 read-only ruling is unchanged.

Surface additionally renders the merged projection as a read-only **flow-graph sidebar**: nodes and edges come from the artifact's extension-attested topology snapshot; per-node status comes only from host-observed lifecycle plus Crew's durable acceptance/budget state; Undertow and Offshore members render as the same node kind distinguished by a lane label. The sidebar deep-links to Ocean Floor by `root_execution_id` and is a projection, not a second control surface — operator actions remain on the staging artifact.

### 7.7 Member completion envelope and acceptance ledger

Every member attempt terminates into one typed completion envelope: a bounded inline summary (byte/line ceilings with an explicit truncation marker and a confined artifact reference for overflow), optional structured output validated against the node's declared JSON schema, session/execution references (never transcript copies — invariant 6), usage, the per-attempt model record, requested and host-effective capability ids, and budget outcomes. A `file-only` output node returns only the reference plus a one-line summary.

Member results are accepted, not believed. Crew maintains a durable acceptance ledger per node with ascending levels:

- **attested** — the member's own structured report (changed files, commands run, residual risks);
- **checked** — Crew's deterministic runtime checks over the envelope and host lifecycle facts (e.g. a read-only role produced no mutation executions);
- **verified** — named verification commands executed by Crew as ordinary permission-gated host executions under the root workspace's grants, never trusted from the child's own claims;
- **reviewed** — an independent reviewer-profile member (never the writer) returns blocker/non-blocking findings.

Each graph node declares its required level (defaults: read-only roles attest; mutating roles reach at least verified when the workspace names check commands). The ledger records evidence, verification results, review findings, and the terminal accepted/rejected decision by the root agent or operator. Join and finish nodes gate on ledger status, not on raw completion.

### 7.8 Budget ladder, attention states, and member control

Ceilings alone are not a policy; Crew ratifies a between-turn enforcement ladder. Because the host owns the agent loop, Crew never mutates a running turn: soft thresholds (elapsed time, turns, tool calls, tokens) raise a recorded advisory and flip the node's attention state; hard thresholds cancel the attempt through generic host cancellation with a recorded `budget_exhausted` outcome; a member cancelled for budget may receive one bounded wrap-up attempt — a short-deadline continuation turn in the same member session requesting a partial-result report — counted against the graph's aggregate budget. Every outcome (`within-budget`, `soft-reached`, `wrap-up`, `exhausted`) is a durable per-attempt fact.

Attention states are derived, never invented: Crew computes `active-long-running` and `needs-attention` only from host lifecycle facts (elapsed time, turn/tool counts, permission-waiting, repeated tool-failure metadata) and its own ledger, and surfaces them on the artifact and the flow-graph sidebar.

Member control uses only existing seams: **interrupt** is generic host cancellation leaving the node `paused`; **resume** is an ordinary package-attributed continuation turn on the member's durable session, idempotent by command id; **steering** guidance is queued and delivered only at the next attempt boundary. Mid-turn message injection into a running member turn is excluded (§10). Every control action carries the same dedup/audit discipline as §6 commands.

## 8. Staged implementation plan

Strict order; each stage gates the next. Stages A–C live in `ocean-os` and follow the extension manifest's own phases; D–E live in the extension package repo.

- **Stage A — extension host readiness.** Accept Phase 1; implement Phases 2–3 (lifecycle observers, supervised services, installed/trusted/enabled state, inspect/doctor, local/Git install). *Gate:* a supervised no-op service survives restart with confined state and scoped events.
- **Stage B — generic seams.** Execution request + cancellation + scoped delivery + continuation + state directory (§6.1–6.3, 6.5–6.6). *Gate:* host conformance tests prove grant non-widening, session isolation, idempotent replay, dedup, audit identity, and package-removal safety — with no orchestration vocabulary in core.
- **Stage C — interactive workflow artifacts.** SDK types, daemon artifact registry/routing, TUI renderer + component-event transport, Surface renderer including the read-only flow-graph sidebar projection. *Gate:* revision/idempotency tests; a non-Crew demo extension drives the artifact end to end.
- **Stage D — Crew v1.** Package registers the two Longhouse delegation facade tools; implements the Undertow and Offshore lane adapters plus the four versioned capability profiles from §7.2; implements the member completion envelope, acceptance ledger, and budget ladder (§7.7–7.8); supports fixed fan-out ≤3, join, reduce, one batched continuation; SQLite durability with crash-recovery tests; staging→running hot-swap; Observatory attestation; Ocean Floor deep link. *Gate:* host conformance proves capability non-widening and requested/effective audit records; Undertow/Offshore policy-equivalence fixtures pass; acceptance-ledger and budget-ladder fixtures pass; kill-and-recover tests (daemon restart, service restart, child failure, budget exhaustion, cancellation) all reconcile against host truth.
- **Stage E — bounded growth.** Conditional routing → one evaluator/revise cycle → bounded dynamic map fan-out → workflow-spec templates (consuming the Bedrock workflow layer and any repo-local `docs/orchestrator/workflows/` dir — now hosted by the private `risingtides-agents` repo — via the OCEAN-340 endpoint) → human checkpoints. Hierarchy and model-authored subgraphs remain out until separately ratified. Saved trigger-driven automation ("Ocean Flows") is a separate future product, not Crew scope creep.

## 9. Critical invariants

1. No orchestration vocabulary, scheduler, named-worker runtime, or undertow/offshore delegation enum in `ocean-os` core.
2. Every member attempt is an ordinary permission-gated host execution; the effective capability set is an intersection, labels and targets never widen it, and requested/effective ids are audit-visible.
3. Longhouse facade tools are extension-provided and route only to staged Crew proposals; the compiled `ocean-longhouse` crate remains advisory/read-only.
4. Bedrock supplies bounded provenance-labeled knowledge only; it never carries grants, secrets, or live scheduler state.
5. Observatory stays read-only; control never travels through it; proposals never enter it.
6. Extension state lives only in the confined state directory; daemon session JSON is never touched by extensions.
7. All extension-originated effects (executions, commands, continuations) are idempotent and package-attributed.
8. Auto-start requires operator opt-in, surface acknowledgement, and passing re-validation; restarts never auto-launch.
9. Graph ceilings are validated before staging and before start; running nodes bind to immutable revisions.
10. The chat transcript remains the primary experience; supervision is one pinned artifact, not a second application.
11. Member results cross joins only through the acceptance ledger; verification commands run under the root workspace's grants and are never trusted from a child's own claims.
12. Attention and budget states shown on any surface derive from host lifecycle facts and durable Crew state, never from extrapolation.

## 10. Explicit exclusions

Core `task`/`spawn_worker`/`delegate_undertow`/`delegate_offshore`/fleet APIs; treating advisory `/v1/subagents/spec` tool strings as executable grants; unrestricted in-process extension code execution; Pi-subagents porting or parity; mid-turn message injection or capability mutation of a running member turn; transcript forking as member context (curated provenance-labeled bundles are the only context lane); recursive fan-out, saved chain DSLs, worktree automation, watchdogs, or arbitrary agent creation in Crew v1; cross-session permission approval from the staging widget; Observatory as a control plane; Bedrock as a scheduler or grant plane; per-second countdown streaming.

## 11. Stop conditions requiring a design decision

Stop and consult the operator if: a seam cannot be expressed without orchestration vocabulary in core; artifact interaction cannot be authenticated/scoped without weakening component security; continuation semantics conflict with session authority; Observatory admission would require widening the metadata envelope; or SQLite recovery cannot reconcile with host truth without exactly-once assumptions.

## 12. Acceptance criteria

This manifest is accepted when the operator ratifies: (a) extension ownership of the absorbed R5 engine, (b) the six-seam host contract, (c) the extension-provided Longhouse Undertow/Offshore facade and lane semantics, (d) the real-tool capability profiles plus host intersection/non-widening rule, (e) the Bedrock knowledge-only boundary, (f) the staged plan and its gates, (g) the grace/auto-start safety policy, (h) the exclusions, and (i) the member completion envelope, acceptance ledger, and budget-ladder/attention semantics (§7.7–7.8). Acceptance authorizes Stage A work under the extension manifest's existing phases; Stages B–E each require their own implementation manifest before code, mirroring the Observatory Gate 1 pattern.

---

## 13. Informative annex — pi-subagents concept study (non-normative)

Studied 2026-07-21 against `pi-subagents` v0.35.1 (TypeScript, ~49k lines) as prior art for what a production subagent surface accumulates. This annex maps each concept to its Ocean disposition. It is informative only: §10's no-porting/no-parity exclusion stands, and concepts are translated idiomatically per the docs contract, never line-by-line. Where a concept graduated into normative text, the section is named.

| pi-subagents concept | Ocean disposition | Where |
| --- | --- | --- |
| Agent definition files (tools, model + fallbacks, thinking, budget/acceptance defaults) | Crew member roles as packaged folder-agent data plus versioned capability profiles; raw tool lists replaced by profile ids the host intersects | §7.2 |
| single/parallel/chain modes with `{previous}`/`{chain_dir}` templating | Typed graph nodes with explicit edge data mapping; no string templating in v1 | §7.3 |
| fresh vs fork context; fork preamble | Fresh + curated provenance-labeled bundle only; transcript forking excluded | §7.1, §10 |
| Turn/tool budget ladders (soft → wrap-up → grace → hard block) | Between-turn ladder: advisory → generic cancellation → one bounded wrap-up attempt; outcomes durable per attempt | §7.8 |
| Absolute deadlines and timeouts | Required graph deadline plus per-member deadlines enforced through host cancellation | §7.3, §7.8 |
| Output truncation, artifact spillover, `file-only` mode | Completion-envelope ceilings plus confined artifact references | §7.7 |
| Structured output with JSON schema per step | Schema-validated structured results on the envelope | §7.7 |
| Acceptance framework (attested/checked/verified/reviewed, evidence kinds, parent-run verify commands, reviewer gate, durable ledger) | Adopted as the acceptance ledger; verification runs as parent-grant host executions | §7.7 |
| Activity/attention states with time/turn/token/failure thresholds | Derived only from host lifecycle facts plus Crew ledger; drives artifact and sidebar states | §7.8 |
| steer/interrupt/resume/stop with per-target delivery states | Cancellation plus durable-session continuation turns; boundary-delivered steering; delivery states recorded | §7.8 |
| Stale-run reconciler, owner liveness, PID tracking | Service-restart reconciliation against host truth; recovered runs come back `paused` | §7.4, §7.5 |
| Session spawn budget + operator grants with history | Aggregate ceilings on the staging artifact; ceiling raises are staged operator actions recorded as grant rows | §7.5 |
| Completion batching (debounce grouping; failures bypass) | Batched continuation policy | §7.5 |
| Depth guard via env propagation; child = min(parent, agent) | Host-side Observatory admission depth plus Crew graph ceilings; the intersection rule throughout | §7.3, §7.6 |
| Workflow-graph snapshot (nodes/phases/status) for widgets | Bounded topology snapshot on the artifact; merged with host lifecycle for the flow-graph sidebar | §6.4, §7.6 |
| Nested run routing (control inbox, capability token, event sink) | Host scoped delivery, opaque correlation ids, and single-use Observatory binding tokens | §6, §7.6 |
| Watchdog (complementary-model review of child work) | Excluded v1; the reviewer acceptance gate covers the need; revisit no earlier than Stage E | §10 |
| Scheduled/deferred runs | Excluded; belongs to the separate Ocean Flows product | §8 Stage E, §10 |
| Worktree isolation + setup hooks | Excluded v1; workspace binding stays host-owned | §10 |
| `subagent_wait` parent blocking | Not needed: the daemon is long-lived and the continuation seam replaces parent blocking | §6.5 |

The one structural lesson that changed normative text: pi-subagents can act *inside* a child's loop because it owns the runtime, so its budgets and steering are mid-turn. Ocean's host owns the loop, so every equivalent control lands at turn boundaries through cancellation, continuation turns, and durable sessions — the same outcomes through honest seams.
