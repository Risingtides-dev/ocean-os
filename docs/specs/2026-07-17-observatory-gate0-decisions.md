# Ocean Observatory — Gate 0 Decisions

**Date:** 2026-07-17
**Status:** Accepted by operator (Smaths) on 2026-07-17 with the rulings recorded in the Revisions section
**Type:** Decision record
**Scope:** Resolves all 8 unresolved questions from section 12 of `2026-07-17-ocean-observatory-architecture.md`

---

## Overview

This document records structured decisions on eight foundational questions that must be accepted before Ocean Observatory implementation begins. Each decision includes a recommendation, rationale, risks, and reversibility analysis.

The Observatory is a truthful operator view of root agents, extension-owned subagents, their relationships, and work state. It answers: What needs attention? What's active? How are executions related? What happened? What can the operator safely do?

**Invariant:** Extension ownership of subagent orchestration is preserved. The core daemon provides only generic permission-gated execution seams; dispatch, spawn, retry, recursion, and budget policy remain extension-owned per `2026-07-14-ocean-extensions-architecture-and-migration-manifest.md`.

---

## Decision 1: Persistence Owner

**Question:** Where does Observatory durable state live? A dedicated crate or an existing owner?

### Recommendation

**Create a dedicated `ocean-observatory` crate** under `crates/ocean-observatory/` owned by the core daemon authority.

### Rationale

- **Single responsibility:** Observatory is a distinct data plane from session management, permissions, or tool execution. Separating persistence allows clear contracts, independent schema evolution, and focused testing.
- **Audit and retention isolation:** Observatory events are observability and audit facts, not operational session data. Dedicated storage allows independent retention, redaction, and export policies without affecting session/permission state.
- **Domain clarity:** A dedicated crate signals that Observatory is core infrastructure, not an opportunistic addition to room/session storage. This establishes the contract for future observers and extensions.
- **Independent lifecycle:** If a session is archived or migrated, Observatory audit trail remains intact and queryable. This supports the "dock and restore" workflows where an operator reviews historical activity independent of live sessions.
- **Access control:** Scoped observer principals (section 9) can be implemented independently without widening access to session storage.

### Risks

- **Operational overhead:** Running SQLite/WAL adds a persistent background process and recovery considerations (WAL checkpoints, database locks). Mitigation: standard SQLite practices, startup/shutdown guards, health checks in daemon startup sequence.
- **Schema coordination:** Adding a new persistence layer introduces one more source of schema migrations and version coordination across daemon restarts. Mitigation: version-gated append-only event log; schema is stable, payload enum variants are additive.
- **Backup/restore complexity:** Operators now manage Observatory database alongside session storage. Mitigation: clear runbooks, documented backup strategy, consider eventual integration with existing session archive tooling.

### Reversibility

**Reversible if done carefully:** If Observatory proves to be a poor fit in a crate, its event log can be re-exported and re-appended to a different owner. The difficulty is maintaining event ordering and cursor continuity during migration. Recommended: Accept this decision for at least one full release cycle (6 months) before considering alternatives.

---

## Decision 2: Credential Distribution

**Question:** How do first-party clients (web, extension, Tauri, CLI) obtain scoped observer authority without embedding a bearer secret?

### Recommendation

**Use scoped bearer tokens with sub-authority delegation, distributed via secure cookie (web/Tauri) and environment variable (CLI/extension).**

Specifically:
- **Web/Tauri:** Observatory principal granted as a scoped bearer token in a `secure`, `httponly`, `samesite=strict` cookie when the client connects to the proxy. Token is narrowly scoped to `observatory:summary` by default; `observatory:content` requires explicit user approval and re-authentication.
- **CLI/extension:** Scoped bearer token provided via `OCEAN_OBSERVER_TOKEN` environment variable. The token is read from the secure local `.ocean/config` directory (mode `0600`) during daemon startup and passed to child processes.
- **Token structure:** Tokens include: principal scope, daemon instance, expiry, and a cryptographic (HMAC) signature keyed by a daemon-local secret, or are opaque random tokens validated against daemon-held state. Tokens are never persisted to disk, logs, event streams, or URLs. Refresh requires re-authenticating with the daemon.
- **No query-string credentials:** Tokens are never embedded in URLs or query parameters. They travel only via secure HTTP headers or environment variables.

> **Gate 1 implementation refinement:** The accepted Gate 1 manifest supersedes these distribution details for V1. The daemon now publishes a rotating boot-bound summary bearer to the mode-0600 `.ocean/observatory-token` file; trusted native clients and the Ocean Surface proxy read it immediately before a request, and the proxy injects the `Authorization` header on the daemon-side hop. `OCEAN_OBSERVER_TOKEN` is an explicit isolated-process override only and is never injected globally. The daemon accepts `Authorization-Observer` only as compatibility cookie input and never issues it. See Gate 1 §3.4 for the current contract.

### Rationale

- **Separation of concerns:** Tokens are scoped to observation, not control. They cannot approve permissions or cancel executions, addressing the "no implied control scope" requirement from section 9.
- **Standard practice:** Secure cookies and environment variables are well-understood credential channels that don't leak to URLs, logs, or browser history.
- **Multi-client compatibility:** Cookies work for web/Tauri; environment variables work for CLI and local extension orchestration. Daemon bootstrap can unify credential distribution.
- **Revocability:** Short-lived tokens (15-60 minute lifetime) allow quick credential rotation without requiring all clients to re-authenticate. Operator can kill switch by invalidating a daemon instance or revoking observer scope.
- **Simplicity at scale:** Avoids complex OIDC, mTLS, or certificate distribution for local observability. Appropriate for a single-operator, single-host observation model.

### Risks

- **Secret sprawl:** If `.ocean/config` or environment variables are not carefully guarded, tokens could leak to build logs, process inspection, or debugging sessions. Mitigation: clear documentation, `.ocean/` as a restricted directory, daemon process runs with restricted umask.
- **Proxy complexity:** The proxy must forward or re-issue scoped tokens correctly. If the proxy leaks, transforms, or logs tokens, the entire model breaks. Mitigation: contract tests for token forwarding, proxy code review, explicit prohibition on token logging.
- **Credential lifetime mismatch:** If a token expires while an observer is still connected, the SSE stream must reset cleanly with an explicit error. Mitigation: implement token refresh seam in the Observatory API; SSE reset is acceptable failure mode.

### Reversibility

**Reversible.** If scoped bearer tokens prove insufficient, can transition to:
- Short-lived (1-minute) tokens refreshed via a `/v1/observatory/token/refresh` endpoint.
- Client certificate (mTLS) for CLI/extension.
- Browser session binding to daemon-authenticated user (if multi-user becomes supported).

Token distribution is an implementation detail; the contract is "clients must present a scoped observer principal." Keep decision reversible by not embedding distribution mechanism into the protocol.

---

## Decision 3: Initial Scope

**Question:** What does the Observatory observe: the whole daemon, a single project, or the active session?

### Recommendation

**Observe the whole daemon** as the product goal, but **calibrate with active-session-only view during development.**

Specifically:
- **V1 (current):** Observatory shows all active root executions in the daemon and their observed subagents. This covers all sessions, all projects, all concurrent activity.
- **Limitations in V1:** Remote children (section 6) are omitted; only locally observable executions and extension-attested topology are included.
- **Calibration slice during development:** Developers can run a single-session daemon or use namespace filtering (`?project=<name>` query parameter) to focus on a smaller scope for testing and debugging.
- **Future:** Once the whole-daemon view is proven, can optimize for project-scoped observers (e.g., `observatory:project:foo` scope) to support delegated project observation in shared daemon scenarios.

### Rationale

- **Product goal:** A truthful operator view of the daemon means seeing all work, not a filtered slice. This enables the "what needs attention?" workflow that is central to Observatory's purpose.
- **Architectural simplicity:** Whole-daemon observation requires no project annotation, namespace splitting, or access-control filtering at the storage layer. Events flow into one log; filtering is a read-side concern.
- **Extension visibility:** Extension-orchestrated subagents are often cross-project. Whole-daemon scope ensures the parent/child relationship is visible without requiring extensions to coordinate project boundaries.
- **Calibration path:** Starting with active-session-only allows rapid development without requiring daemon-wide coordination. As features stabilize, can flip to whole-daemon by removing the session-filter query parameter.

### Risks

- **Scale and complexity:** Observing a whole daemon with thousands of concurrent executions creates observability overhead (event volume, storage, CPU). Mitigation: cursor-based tail streaming, configurable retention, deterministic event coalescing (section 11).
- **Operator overload:** A whole-daemon view can be overwhelming. Mitigation: attention shelf (section 11) highlights what matters now; semantic list allows filtering by phase/producer/error; empty/degraded states gracefully reduce information at scale.
- **Privacy:** If the daemon contains sensitive projects, whole-daemon observation leaks topology to any observer. Mitigation: scoped principals (section 9) can limit visibility; project-scoped observer scope is a reversible future decision.

### Reversibility

**Reversible.** If whole-daemon proves unworkable:
- Can transition to project-scoped storage (separate event log per project) with a schema migration.
- Can implement project-aware redaction (hide topology of unauthorized projects) as a read-side filter.
- Whole-daemon is easier to build; splitting is harder but possible.

Accept whole-daemon for V1; revisit if scale or privacy concerns emerge.

---

## Decision 4: Entity Vocabulary

**Question:** What are the canonical observable entities? Executions with attached facts, or independent first-class entities?

### Recommendation

**Root and child executions are the canonical observable nodes.** Sessions, turns, requests, tools, and permissions are attached facts, not independent topology entities.

Specifically (from spec section 5):
- **Executions:** One root or child attempt, identified by `execution_id`. Immutable parent/root relationships. Retries create new execution IDs.
- **Sessions, turns, requests:** Correlation identifiers that link executions to the transcript authority. Not part of the topology graph.
- **Tools and permissions:** Activity correlations (`tool_call_id`, `permission_id`), not graph nodes. Tool activity is recorded as an event; permission waits/resolves are events. The normal transcript remains the authority for tool output and permission reasoning.
- **Topology elements:** Nodes (executions), edges (parent/child), and producers (daemon or activated extension).

### Rationale

- **Coherent semantics:** Executions represent work; the topology represents orchestration relationships. Sessions, turns, and tools represent the transcript authority. Separating these concerns makes the model easier to reason about.
- **Durable identity across session boundaries:** An execution_id persists independently of session archival or migration. This allows post-hoc analysis of historical executions across many sessions.
- **Non-duplication:** Without making sessions first-class topology nodes, we avoid the risk of double-counting activity or inferring relationships that are not explicitly stated.
- **Extension clarity:** Extensions define their own subagent topology by attesting parent/child relationships (section 7). They do not define "sessions" or "turn concepts"; those remain host-owned. This preserves the extension ownership invariant.
- **Conformance with spec:** This decision directly mirrors the "truth and identity model" from spec section 5.

### Risks

- **Mapping complexity:** Operators must understand that a single "session" can contain multiple root executions and that a single execution can span multiple turns. Mitigation: the Surface UI should make this relationship clear in the semantic list and inspector.
- **Correlation burden:** If an operator wants to know "all activity in session X," they must scan execution topology and correlate via session_id. Mitigation: UI supports filtering by session_id; Surface reducer can group by session for display purposes.
- **Over-specified identity:** If the spec later requires fine-grained control over individual turns or tools, the execution-as-node model might feel too coarse. Mitigation: accept that fine-grained control is a future evolution; V1 observes the execution topology, not the transcript granularity.

### Reversibility

**Difficult to reverse.** If we later need sessions or tools as first-class nodes, the schema must be extended with new event types and projections. The risk is duplicate topology (both explicit edges and inferred from turn sequence). Recommended: Accept for V1; if needed, add a careful migration in Gate 3 or 4.

---

## Decision 5: Content Retention

**Question:** Should Observatory retain prompts, thinking, tool arguments/output, or assistant text?

### Recommendation

**No content retention by default. Observatory holds only metadata: executions, phases, tools/permissions lifecycle, and attention signals.**

Specifically:
- **Never retained:** prompts, system instructions, thinking text, assistant deltas, tool arguments, tool output, errors, environment variables, paths, or any decision/permission/binding secrets.
- **Safe to retain:** execution IDs, phases (admitted, running, finished, error), phase timestamps, tool names (not args), permission reasons (fixed enum), model aliases, duration metrics, byte counts, tracing IDs.
- **Redaction occurs before append:** Not after. When an event is created, forbidden fields are stripped before the event is written to the durable log and broadcast.
- **Future content projection:** If an operator needs bounded access to prompts or output, that is a separate `observatory:content` scope (section 9) with explicit opt-in and a separate privacy/encryption/export decision. It is not part of V1 Observatory.

### Rationale

- **Privacy by default:** Prompts and output are often sensitive (customer data, proprietary reasoning, API keys). Storing them in Observatory violates the principle of least privilege.
- **Compliance:** Many organizations require prompts/output to be ephemeral or encrypted. Avoiding content retention sidesteps those requirements for the metadata view.
- **Performance:** Metadata-only events are smaller and faster to append, stream, and replay. This allows Observatory to scale to high-volume daemon activity without storage overhead.
- **Schema stability:** If content is not retained, the schema cannot accidentally leak it. Redaction is compile-time guaranteed by the type system, not a runtime filter.
- **Separation of concerns:** The transcript (sessions, turns, output) is the authority for content. Observatory is the authority for topology and observability. Do not conflate them.

### Risks

- **Debugging friction:** When an operator sees an execution fail, they cannot immediately view the prompt/output from Observatory. They must navigate to the transcript. Mitigation: Observatory inspector includes links into the session transcript; operator follows the link for content.
- **Future change:** If a future requirement demands "show me the prompt for this execution," the feature requires a new approval decision, schema migration, and audit trail. Mitigation: this is intentional — content retention is a conscious privacy/security trade-off, not a default.

### Reversibility

**Not easily reversible.** If Observatory is deployed without content retention and an operator later needs it, retroactive content injection is impossible (the prompts/output is gone). However, forward-looking content retention can be implemented in a future decision (section 9, `observatory:content` scope).

**Recommended:** Accept metadata-only for V1. If future requirements demand content retention, it must be an explicit scope approval and encryption decision, separate from this Gate 0 acceptance.

---

## Decision 6: Remote Children

**Question:** Should Observatory show subagents spawned on remote hosts or via external orchestration APIs?

### Recommendation

**Omit remote children in V1. Show only locally observable executions and extension-attested topology.**

Specifically:
- **Locally observable:** Root executions in the daemon and child executions admitted via the extension binding seam (section 7).
- **Extension-attested:** An activated extension can attest that it has spawned children outside the local daemon (e.g., on a remote host). These appear in the topology as `extension_attested` facts (spec section 5), labeled `reported`, not `live`.
- **Remote without binding:** If an extension mentions a remote child in an event payload without going through the admission seam, it is rejected and logged as an admission failure.
- **Future: remote binding:** Gate 2 or 3 can introduce a remote binding seam where the local daemon trusts a remote authority to validate child relationships. For V1, this is unresolved.

### Rationale

- **Trust boundary:** Remote children cannot be validated by the local host without a network call and authentication handshake. For V1, accepting only locally validated or explicitly attested facts keeps the model simple and audit-able.
- **Scope reduction:** Observing only local + attested children allows the first release to focus on the extension admission/binding seam and the core topology graph. Remote observation is a separate concern that introduces federation, consistency, and recovery challenges.
- **Provenance clarity:** By distinguishing `host_observed` (local), `extension_attested` (reported by extension), and `derived` (folded from events), the UI can clearly indicate what is known vs. what is claimed.
- **Operator safety:** An operator can trust that `host_observed` children are actually running on the local daemon and can be canceled/inspected. Remote children are labeled separately.

### Risks

- **Incomplete view:** If an extension spawns children on multiple hosts (e.g., a distributed inference pipeline), the local Observatory shows only the root and children attested by the extension, not the full fanout. Mitigation: extension can attest as many children as needed via repeated admission requests; the limitation is architectural (no remote query), not a quota.
- **Stale attestation:** An extension can attest a child that has already failed on the remote host. Observatory shows it as `reported` until the extension sends an update. Mitigation: this is correct behavior — Observatory records what the extension claimed, not what actually happened; the extension owns reconciliation and updates.

### Reversibility

**Reversible.** Adding remote binding in a later gate is a schema extension. The decision is conservative (omit remote for now); adding later is forward-compatible.

---

## Decision 7: Retention Defaults

**Question:** How long should Observatory retain metadata events and projections?

### Recommendation

**Accept the proposed retention defaults from spec section 9:**

- **Metadata events:** 7 days and 1 GiB maximum (whichever comes first)
- **Nonterminal nodes/edges:** Never prune during activity; only clean up after daemon restart sets them to `interrupted`
- **Terminal projections:** 30 days (e.g., a completed execution remains queryable for a month)
- **Restart behavior:** Previous nonterminal host executions transition to `interrupted` state; this is a state change, not deletion
- **Lease expiry:** Extension-attested children whose lease expires transition to `disconnected`, never `completed`
- **Schema:** Operator-configurable per environment; defaults are reasonable for single-host development/small-team deployment

### Rationale

- **Conservative sizing:** 7 days of metadata fits on typical machines (text events, ~1 KiB each, 100-1000 events/day ≈ 700MiB/week). 1 GiB cap prevents runaway storage on long-running daemons.
- **Audit trail:** 7 days is sufficient for incident investigation ("what happened with execution X?") without requiring explicit archive management.
- **Terminal projection lifetime:** 30 days allows a completed execution to remain queryable for trend analysis and reconciliation. Metadata events older than 30 days are pruned, but the final state is preserved if the execution was terminal.
- **Restart semantics:** Marking nonterminal executions as `interrupted` (not deleted) preserves the audit trail and avoids false success claims. Operators can see which work was interrupted by a restart.
- **Lease expiry:** Extension children that lose their lease are `disconnected`, not `completed`, signaling that the relationship ended, not that the work succeeded.

### Risks

- **Capacity planning:** If a daemon runs 24/7 for months, metadata will fill available storage. Mitigation: runbook includes archival strategy; operators should monitor `.ocean/observatory.db` size and rotate storage if needed.
- **Replay truncation:** Querying replay across retention boundary (e.g., events from 8 days ago) returns explicit gap/410 response. Operator experience is a little jarring but operationally sound.
- **Configurable boundaries:** If retention is per-environment, operators must set it correctly for their use case. Mitigation: clear documentation, example configs, health checks to warn if storage is near capacity.

### Reversibility

**Fully reversible.** Retention is a configuration parameter in `ObservatoryStore::new(retention_policy)`. Changing retention does not require a schema migration; old events are pruned according to the new policy on the next compaction cycle. Accept the defaults; adjust based on operational experience.

---

## Decision 8: Control

**Question:** Should Observatory support canceling executions, approving permissions, or other state changes?

### Recommendation

**Read-only Observatory in V1. All mutations remain in existing authoritative surfaces (session control routes, permission approval, transcript).**

Specifically:
- **Observable only:** `/v1/observatory/*` routes are read-only. They return `Cache-Control: no-store`.
- **Mutations elsewhere:** Canceling an execution uses the existing `DELETE /v1/session/<id>` or permission approval uses the existing `/v1/permission/<id>/approve` routes.
- **Intent signaling:** If UI later needs to signal intent (e.g., "user wants to cancel"), the signal goes through the existing authoritative route, not Observatory.
- **Future:** A future gate can introduce `observatory:control` scope with explicit consent, but V1 is read-only.

### Rationale

- **Authorization simplicity:** Observer scope does not imply mutation capability. No need to replicate permission checks, state machine validation, or cascading side effects.
- **Single authority:** Session state is the authority for execution lifecycle. Avoiding an alternative mutation path prevents divergence and confusion.
- **Safety:** An operator can confidently connect an Observatory view to a read-only session (e.g., audit, historical review) without risk of accidental state changes.
- **Incremental adoption:** V1 can focus on truthful observation. Mutation support is a distinct design phase with its own security review.

### Risks

- **Workflow friction:** An operator views a failing execution in Observatory, then must navigate away to cancel it. Mitigation: Ocean Surface UI includes transcript links; operator can cancel from the transcript view.
- **Future complexity:** If mutations are added later, the protocol must coordinate between the control/authoritative surface and Observatory's read-only view. Mitigation: keep the contract simple — Observatory never initiates mutations; it only reflects facts written by authoritative surfaces.

### Reversibility

**Fully reversible.** Adding control in a future gate requires a new scope (`observatory:control`) and explicit review. The read-only contract in V1 does not preclude this. Keep all Observable routes read-only for now; future decisions can extend.

---

## Extension Ownership Invariant

**Assertion:** Subagent orchestration remains in extensions.

Per `2026-07-14-ocean-extensions-architecture-and-migration-manifest.md`, the core daemon provides only generic permission-gated execution seams. Observatory **does not change this invariant.**

Specifically:
- **Extension admission seam (section 7, decision 3 implementation):** The daemon validates and mints IDs for topology relationships, but the extension owns the semantics (which child to spawn, under what conditions, with what lifetime).
- **No core scheduler:** The daemon does not add a `spawn_worker`, task queue, retry loop, or join strategy. These remain extension-owned.
- **No core subagent runtime:** Observatory records facts about subagents; it does not execute them or manage their lifecycle.
- **Topology attestation:** Extensions attest that they have spawned children; the daemon validates the claim and records the fact. The extension remains accountable for reconciliation and updates.

**If a design challenge later suggests moving orchestration to core, that change must be **explicitly approved as a separate decision**, not inferred from Observatory's existence.**

---

## Anti-Foundations List

**The following design patterns and implementation shortcuts are explicitly rejected for Observatory:**

1. **No Claude Design export consumption:** The downloaded `Agent Floor` pixel office, mock event model, fixed rooms, and inferred-state infrastructure are disposable concept evidence only. Implementation must not derive from that export.
2. **No `/v1/agent/events?all=1` mirroring:** Observatory is not a wrapper around the global agent firehose. It has its own event semantics and redaction pipeline.
3. **No raw prompt/output/thinking in any retention tier:** Forbidden fields are enforced by type system, not runtime filters or audits.
4. **No client-side authorization:** Authorization is enforced by the daemon; the proxy and client reject unauthorized access, but the daemon is the authority.
5. **No timestamp-based event joining:** Events are ordered by `cursor`, not client timestamp. Clients never join independent streams by timestamp.
6. **No implicit control scope:** Observer scope does not grant permission approvals, execution cancellations, or other mutations.
7. **No random UUID event IDs as sole recovery position:** Events are ordered by monotonic cursor; random IDs provide audit trail only. Recovery position is cursor, not event ID.
8. **No unbounded observability queues:** Snapshot and tail streaming are bounded by cursor, watermark, and explicit gaps.
9. **No one connection per actor:** Observatory does not require per-execution or per-session connections; SSE uses a single multiplexed stream.
10. **No canvas-only semantics:** The UI is DOM-first with semantic controls; canvas renders visuals only and is `aria-hidden`.
11. **No desktop-only delivery:** Compact mode and reduced-motion behavior are in-scope for V1, not deferred.
12. **No subagent inference:** Parent/child relationships are explicit (admission seam) or extension-attested, never inferred from names, tools, timestamps, or cwd.

---

## Gate 0 Acceptance Checklist

**Operator must review and accept all eight decisions above before any code implementation begins.**

- [x] **Decision 1:** Dedicated `ocean-observatory` crate for persistence
- [x] **Decision 2:** Scoped bearer token credential distribution (with cryptographic token revision below)
- [x] **Decision 3:** Whole-daemon observation scope (with active-session calibration during dev)
- [x] **Decision 4:** Executions as canonical nodes; sessions/turns as attached facts
- [x] **Decision 5:** Metadata-only, no content retention by default
- [x] **Decision 6:** Omit remote children in V1 (extension-attested only)
- [x] **Decision 7:** 7d/1GiB metadata, 30d terminal projections retention defaults
- [x] **Decision 8:** Read-only Observatory in V1; mutations via existing authoritative routes

**If any decision is revised, update this document and document the revision in a clear "Revision" section below before proceeding to Gate 1.**

---

## Next Steps

1. **Operator review and acceptance:** Present this document for operator review. Document any revisions in a "Revisions" section.
2. **Gate 1 Implementation Manifest:** After Gate 0 is accepted, write `docs/specs/2026-07-XX-observatory-gate1-implementation-manifest.md` with the exact API contracts, schema, auth token format, and test requirements for Gate 1 tasks (2–8 in the task plan).
3. **No code until Gate 0 approved:** Do not begin crate creation, schema design, or API implementation until this document is formally accepted.

---

## Revisions

### R1 — Operator acceptance and visual-direction ruling (2026-07-17)

The operator (Smaths) accepted all eight decisions and issued one product ruling:

- **Visual direction is decided:** Ocean Floor ships with full 90s-CPU-game visual parity — the
  pixel-art isometric game aesthetic from the Agent Floor concept exports is the accepted product
  presentation layer. The architecture spec's "operational instrument, not a virtual office" line is
  overridden **on aesthetics only**. Ambient game-like presentation is permitted; every displayed
  *fact* (lifecycle, activity, attention, topology) must still originate from recorded truthful
  events per spec sections 5 and 11.
- **Real events only:** the renderer is wired to real Ocean daemon events through the Observatory
  contract (snapshot/live/replay). No mock event model ships.
- **Durable event store ("longhouse events"):** the append-only durable Observatory event log in
  the dedicated `ocean-observatory` crate is confirmed as a required v1 deliverable, not optional.
- **Security invariants unchanged:** metadata-only v1, scoped observer auth, redaction before
  append, read-only v1. The concept export's APPROVE/DENY permission buttons and
  "speak to this agent" turn-submission box are **excluded from v1**; all mutations stay in
  existing authoritative routes.

### R2 — Token signature correction (2026-07-17)

Decision 2's draft described a "non-cryptographic signature." Corrected: observer tokens must be
cryptographically signed (HMAC with a daemon-local secret) or opaque random tokens validated
against daemon-held state. Unauthenticated token structure is not acceptable.

---

## References

- `docs/specs/2026-07-17-ocean-observatory-architecture.md` — Full Observatory spec
- `docs/specs/2026-07-14-ocean-extensions-architecture-and-migration-manifest.md` — Extension ownership and extension semantics
- `AGENTS.md` (root) — Observatory not approved for implementation until Gate 0 decisions accepted
- `ROADMAP.md` — Observable listed; Gate 0 is first checkpoint
