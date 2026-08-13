# Ocean Team Manager — Gate 1 Product and Implementation Manifest

**Date:** 2026-08-09

**Status:** proposed; awaiting operator acceptance

**Implementation authority:** none until this manifest is accepted

**Program:** Community Board, closed-loop work, Team Home, and extension-owned team management

**Repository scope:** `ocean-os`, `ocean-surface`, `ocean-bedrock`, `ocean-agents`, `risingtides-agents`, and Pasture/Stitchpad integration seams

**Evidence baseline:** `ocean-os` local `main` at `2a1e349`, ahead of `origin/main` by one commit and dirty on 2026-08-09; current public/private repository state inspected on the same date

## 1. Operator decision requested

Accept Ocean Team Manager as a staged product program with one durable work
model and several honest projections. The first projection is a community/team
board. The end product is a transparent manager that captures possible work,
helps humans decide what is real, assigns accepted commitments to humans or
agents, detects stalled loops, and makes the team's present state legible from
one daily front page.

Acceptance selects the contracts and dependency order in this document. It does
not accept implementation in advance, does not declare the existing board work
shipped, and does not authorize the agent-execution gate that depends on an
unwritten Crew Stage B-or-later implementation manifest.

## 2. Product decision

Ocean Team Manager is a team operating system, not a Trello clone and not an
activity monitor.

- A **work item** is the durable semantic object.
- Board, List, Triage, Attention, Room sidebars, call chips, Team Home, and daily
  briefs are access-filtered projections over the same work history.
- A detected commitment is a **candidate**, not an obligation.
- A candidate becomes a commitment only through an authorized acceptance
  event or an explicit, inspectable Room policy.
- A closed item must retain owner, outcome, evidence, and acceptance history.
- Room transcripts remain the local durable event authority. Bedrock supplies
  authenticated federated ordering and cross-Room projections.
- Organization-specific dispatch, leasing, escalation, and workflow policy
  remain extension-owned. Ocean core exposes only generic sessions, events,
  permissions, cancellation, capability providers, and extension seams.

The product has two modes over shared infrastructure:

| Mode | Intent | Assignment semantics | Private connectors |
| --- | --- | --- | --- |
| Community coordination | Transparent coordination with soft ownership and public context | Requests and volunteered claims | Off by default |
| Team manager | Accountable commitments with strict identity, review, evidence, escalation, and scoped private context | Authority-checked assignments and queue claims | Explicit grants only |

The interface must label the active mode. Team-manager guarantees must never be
implied in a community space that has not enabled its stronger governance.

## 3. Truth about the current state

### 3.1 Delivered substrate

Current Ocean already has useful foundations:

- durable persistent Rooms with paginated transcript reads, live SSE, mentions,
  per-Room agent session continuity, and federation;
- Bedrock global ordering, membership/access projections, outbox recovery, and
  visible pending/recovering/revoked states;
- typed call detection through `CallTaskDetected`, including title, inferred
  assignee, due text, source quote, and confidence;
- Surface Rooms, live call UI, permission/run attention, and a session-scoped
  render-protocol Kanban component;
- permission-gated sessions, cancellation, capability providers, extension
  lifecycle work, and an accepted extension/Crew architecture;
- voice-intake source anchors and dedupe, plus Gmail/Slack courier transports
  with idempotency and draft-before-send safety.

These are ingredients. They are not yet a shared team manager.

### 3.2 Recoverable but unshipped board work

The current `ocean-os` checkout contains uncommitted board work:

- an untracked `crates/ocean-board/` pure event envelope and transcript fold;
- an untracked TUI board component plus `/board <room-key>` interactions;
- a dirty daemon `GET /v1/rooms/persistent/{key}/board` projection route;
- workspace/index/devlog edits associated with that work.

That work is a promising local board projection prototype. It is not shipped,
team-safe, or recoverable from `origin/main`. It currently has a free-form
column workflow, one untyped string assignee, hardcoded TUI participant `ec`,
full-transcript folding per request, no Surface board route, and no complete
work-item/closure model.

Every demo before Gate 1 exit must be labeled:

> Prototype — local Room board projection only; not an authenticated team
> manager and not shipped.

The existing Surface render-protocol Kanban is also not the board. It remains an
ephemeral agent-rendered preview that may eventually deep-link through a stable
`work_item_id` to the canonical first-class Surface route.

## 4. Non-negotiable product invariants

1. **One authority, many views.** A board mutation appends a durable Room work
   event. No first-party surface owns a second card database.
2. **Candidate is not commitment.** Passive extraction cannot silently obligate
   a person or agent.
3. **Identity is derived, not claimed.** Team-facing mutation requires
   daemon-derived human or agent identity backed by authenticated membership.
4. **Authorization is evaluated at read and write.** Cross-Room projections
   disclose only fields the viewer may see; aggregate counts and briefs are
   subject to the same rule.
5. **Every accepted item has one accountable owner or an explicitly visible
   unowned/team-queue state.**
6. **Done is evidence-bearing.** Without completion evidence and required
   acceptance, completion is `Review`, not `Done`.
7. **Reassignment is epoch-safe.** A late former owner cannot complete or cancel
   the current assignment.
8. **All extraction is source-cited and idempotent.** Replays and retries do not
   create duplicate candidates.
9. **Gaps are visible.** Unsupported schema, federation lag, missing connector
   access, redaction, stale mirrors, and failed dispatch are product states.
10. **No hidden performance scoring.** Ocean coordinates work; it does not
    silently rank people or turn uncertain extractions into personnel facts.
11. **Core stays generic.** No core daemon/runtime task scheduler, named-agent
    fleet, organization workflow, or acceptance ledger.
12. **Administrative work has a budget.** If Ocean needs more coordination labor
    than it removes, that is a product defect.

## 5. Durable work model

### 5.1 Envelope decision

Define a new versioned envelope kind, `ocean.work.item`, rather than expanding
the prototype's `ocean.board.card` v1 into the semantic authority.

- `ocean.work.item` owns lifecycle, typed assignment, source, closure, evidence,
  and external-link semantics.
- `ocean.board.card` v1 remains a prototype/compatibility presentation envelope
  until Gate 0 decides whether to migrate or retire it.
- Unknown envelope versions are counted and surfaced; they are never silently
  treated as ordinary chat or silently omitted from work views.
- Encoders and projectors use golden fixtures across Rust, Surface, Bedrock,
  and connector owners before implementation fans out.

### 5.2 Identity and event envelope

Each work item uses a globally unique UUIDv7 `work_item_id` and carries an
`owning_room_id`. A separately unique `event_id` makes every mutation
idempotent. Cross-Room references use `work_item_id`; ownership transfer is an
explicit event with source and destination authorization.

Every event records:

- schema version and event kind;
- work item and owning Room ids;
- actor id and actor kind (`human`, `agent`, `system`, `connector`);
- event id and source idempotency key;
- occurred and recorded clocks;
- Room local sequence and optional Bedrock global sequence;
- operation payload;
- visibility/redaction class;
- assignment epoch when the event depends on a current assignment.

Comments use a stable `comment_id`; dedupe never relies on matching text.

### 5.3 Work-item fields

The accepted schema must represent:

- title and description;
- workflow status;
- typed assignee: human, agent, team queue, or unassigned;
- requester and accountable project/Room owner;
- priority, labels, due date, and next action;
- blockers and dependencies by stable work-item id;
- acceptance criteria;
- one or more source references with extraction confidence and redaction class;
- candidate disposition and approval state;
- assignment epoch and optional execution/lease references;
- activity checkpoints;
- completion evidence and accept/reject/reopen history;
- external mirrors with per-mirror sync/error state;
- created, updated, started, review-requested, completed, and canceled clocks.

Presentation labels and board columns do not define the wire workflow.

### 5.4 State machine

Canonical statuses are:

`Candidate → Triage → Backlog → Ready → In Progress → Blocked → Review → Done`

`Canceled` is terminal but reopenable by explicit event. Not every item visits
every state. `Blocked` retains the prior execution state and blocker reason.
`Done` requires the terminal contract in §5.5.

Default board grouping is intentionally smaller than the wire state machine:

- Triage is a separate view, not a board column;
- `Backlog` and `Ready` may share a planning column;
- `In Progress` and `Blocked` remain distinct;
- `Review` is explicit;
- `Done` is collapsed by default.

List is the primary mobile view. Board is a desktop spatial projection.

### 5.5 Closed-loop terminal contract

An item reaches `Done` only when the projection can answer:

1. What commitment was accepted?
2. Who owned the current assignment epoch?
3. What was the next action and acceptance criterion?
4. What happened?
5. Where is the evidence?
6. Who or what policy accepted the outcome?

A policy may permit auto-acceptance only when it is explicit, visible, scoped,
and reversible. Otherwise a completion request lands in `Review`. Rejection
must explain what is missing and reopen the current epoch without erasing the
attempt or evidence.

### 5.6 Conflicts across sources

Semantic conflict is first-class, not a last-write guess. The projection may
mark `conflicting_owner`, `conflicting_due_date`, `canceled_in_source`, or
`needs_human_disposition`. Conflicts produce one deduplicated Attention item
and retain all source citations until an authorized disposition event resolves
them.

## 6. Governance and mutation authority

Gate 1 implementation may not begin until an operator accepts this minimum
role matrix or an explicit replacement:

| Action | Room owner | Project/manager role | Contributor | Guest | Agent |
| --- | --- | --- | --- | --- | --- |
| Create manual candidate | Yes | Yes | Yes | Policy | Propose only |
| Accept candidate for self | Yes | Yes | Yes | Policy | No |
| Assign another human | Yes | Yes | Request only | No | Propose only |
| Claim team queue | Yes | Yes | Yes | Policy | Only through dispatch policy |
| Reassign active item | Yes | Yes | Current owner may hand off | No | Request only |
| Approve completion | Yes | Yes | Requester when authorized | No | Policy only |
| Dismiss/cancel/reopen | Yes | Yes | Own/requested items within policy | No | Propose only |
| Override conflict | Yes | Yes | No | No | No |

Every override is audited. Missing or inactive requesters fall back to the
project/Room owner; absence of both creates Attention rather than implicit
acceptance. A source message cannot assign someone who is outside the
authorized membership boundary.

No team-facing pilot, personal Team Home, or shared authoritative board claim
may ship until tests prove authenticated human identity, agent identity,
reassignment attribution, and unauthorized mutation rejection across daemon,
Surface, TUI, and federation.

## 7. Ordering, federation, and projection correctness

- Local Room sequence is authoritative for local replay. Bedrock
  `global_sequence` is authoritative for confirmed federated order.
- A local optimistic event and its later confirmed form are one logical event.
  Live-tail accumulation deduplicates by `client_event_id`/event id and replaces
  the pending clock with the confirmed clock. It must not apply the operation
  twice or leave it permanently pending.
- Hydrate plus live tail equals full replay.
- Full replay is deterministic and idempotent.
- Per-field conflict resolution cannot permit a stale assignment epoch to
  mutate the current epoch.
- A cross-Room projection encountering a global-sequence gap marks affected
  Rooms and items `federation_gap`, names the missing range when safe, and shows
  a degraded count on Team Home. It never silently drops the Room.
- Recovering, revoked, offline, redacted, unsupported, and connector-delayed
  sources remain distinguishable.
- Projection work happens outside long-held shared Room locks and supports
  bounded pagination/cursors.

## 8. Capture and triage

### 8.1 Rooms and calls

Rooms support explicit task syntax/actions and conservative extraction. Calls
persist `CallTaskDetected` facts as candidates with source quote, confidence,
and call/segment anchor. The current detect-and-notify-only behavior is not a
closed loop.

During a call, a person may Accept, Edit and accept, Assign, Merge, or Dismiss.
At call end plus a grace period, unresolved candidates create one deduplicated
Attention item based on durable disposition events, not transient UI state.

The current detector emits at most one candidate per segment. Gate 4 must label
that recall limitation and measure it; a later summary-level pass may improve
recall without silently changing commitment semantics.

### 8.2 Email, Slack/messages, voice, GitHub, and Linear

Connectors consume only sources explicitly granted to Ocean. Each candidate
uses an idempotency key derived from provider/account/source message or thread
plus extractor version. Source text remains in its owning system unless the
grant permits bounded excerpts.

Existing voice-intake ledgers and courier draft/idempotency patterns should be
reused. GitHub/Linear start as links or mirrors; they do not silently become a
second authority. Mirror failure is visible on the work item.

### 8.3 Triage operating contract

Each Room selects a triage owner, rotation, or shared role. Triage provides:

- accept, edit-and-accept, merge, dismiss, and defer;
- batch merge/accept/dismiss controls with confidence and source filters;
- desktop multi-select and accessible keyboard flows;
- mobile swipe alternatives plus undo;
- aging digests that collapse old candidates into one Attention item;
- optional per-Room auto-dismiss policy, never a global default;
- visible accountable triage role and volume/age metrics.

Raw confidence renders as low/medium/high; the numeric score is secondary.
Human-edited fields remain visually distinguishable from extracted values.

## 9. Assignment and execution

### 9.1 Humans — Gate 5a

Human assignment creates one notification/Attention item and an immutable
assignment event. Humans can advance state from the item without opening a
form. A configurable first checkpoint may imply `In Progress`. Team-queue claim
is a first-class epoch-incrementing event. Concurrent change shows “changed
while you were viewing” and offers reload/compare; it never silently overwrites
newer assignment truth.

Human-to-agent delegation and agent-to-human handoff are assignment transitions
that retain item context, epoch, and evidence history.

### 9.2 Agents — Gate 5b, separately blocked

Agent execution requires atomic lease, assignment epoch, bounded budget,
permission ceiling, heartbeat/checkpoint, cancellation, resume policy, terminal
evidence, and human/policy acceptance.

The accepted Extension Stage A sequence A3a → A3b → A4 → A5 supplies service
hosting and metadata lifecycle only. It explicitly does not supply execution
requests, leases, cancellation by host execution id, continuation, durable
effects, or the Crew engine. Gate 5b therefore requires a separately written
and operator-accepted Crew Stage B-or-later implementation manifest. Acceptance
of this Team Manager manifest does not authorize Gate 5b.

Lease expiry detection, host cancellation, budget stops, retry identity, and
late-result rejection must be frozen in that later manifest. Until then Ocean
may show human assignments and proposed agent delegation but must not claim a
durable agent-managed loop.

## 10. One deduplicated Attention model

Attention is a product entity derived from work truth, not several independent
red-dot lists. Its minimum projection is:

```text
attention_id
work_item_id
kinds[] = stale | blocked | approval | uncertainty | conflict |
          dispatch_failure | unresolved_call | federation_gap | mirror_failure
dedupe_key = work_item_id
rank
created_at
snoozed_until_by_viewer
acknowledged_by[]
```

One item may carry several badges but appears once in the inbox. Viewer snooze
does not mutate team state. Acknowledge does not resolve the underlying work
condition. Sections cap visible items and offer a truthful “+N more” rollup.
Notification budgets, quiet hours, and maximum once-per-day stale nudges are
part of the user policy, not connector defaults.

## 11. Team Home

Team Home must answer within ten seconds:

- What matters today?
- What is blocked, stale, overdue, conflicted, or awaiting approval?
- What changed since yesterday?
- Which Rooms are active or quiet despite obligations?
- What are humans and agents doing, and where do they need help?
- Is federation, capture, or a connector degraded?

The default page contains:

1. a compact top strip: due/overdue, blocked, approvals, untriaged, and degraded;
2. one deduplicated Attention inbox;
3. My work / Team queue / In progress / Review sections;
4. Rooms pulse with cited recent decisions and commitments;
5. human and agent load/checkpoint state without ranking people;
6. a cited daily brief with freshness and source-health disclosures.

Personal views depend on authenticated cross-device identity. List is the
primary phone experience; work detail is a bottom sheet/full page; board drag
has a keyboard/tap alternative and touch tests. Optional brief delivery through
existing courier boundaries is opt-in, permission-gated, quiet-hours-aware, and
draft-confirmed where the courier contract requires it.

“Room quiet despite obligations” is explicitly heuristic: configurable N days
without a human message while open human-owned work exists. The UI labels the
heuristic and excludes agent-only work with recent checkpoints.

## 12. Privacy, derived-data minimization, and anti-surveillance

Every Team Home/read-model field belongs to one class:

| Class | Example | Who may see it |
| --- | --- | --- |
| Globally aggregable | Count of degraded sources, when policy permits | Authorized team viewers |
| Team-visible, source-redacted | “One legal item overdue” only if existence itself is permitted | Authorized team viewers |
| Room-only | Work title, owner, Room-level status | Current Room members |
| Source-only | Email body, private Slack excerpt, call recording | Viewers with independent source access |
| Never summarized | Secrets, hidden recipients, private personnel content | No Team Home projection |

Redaction is visible: “source withheld — insufficient access,” with a bounded
reason class. Empty space must not masquerade as missing provenance. Counts,
labels, names, load, alerts, and generated brief text undergo the same access
test as raw excerpts. Adversarial tests cover inference leakage from each.

Ocean Team Manager does not ship:

- individual productivity or responsiveness scores;
- hidden manager-only rankings derived from work activity;
- blame language generated from uncertain extraction;
- source excerpts beyond the viewer's independent grant;
- silent capture from personal accounts;
- automatic obligation creation from ambient conversation by default.

Materially different manager-only analytics require a separate governance and
policy decision visible to the people being measured.

## 13. Pilot, SLOs, and success metrics

The first pilot is observe-and-triage before Ocean becomes commitment authority.
It uses one willing team, a small Room set, bounded calls, no automatic private
connectors, and a reversible opt-out. The pilot defines training, “why did this
appear?” explanations, human override, notification budget, support owner, and
rollback before launch.

Minimum pilot-readiness SLOs are fixed during Gate 1 acceptance and include:

- Room/work projection freshness;
- connector/federation lag thresholds and visible degradation;
- daily brief generation latency and citation coverage;
- duplicate candidate rate;
- unresolved unsupported-event count;
- call-candidate disposition and recall sample coverage;
- stale false-positive rate;
- unauthorized read/write rejection;
- zero silent pending-to-confirmed duplication.

Metrics split into two families:

**System correctness:** duplicate candidates, projection lag, unsupported
events, federation gaps, dispatch/lease failures, mirror drift, evidence
coverage, reopen rate, and closure-without-acceptance attempts.

**Organizational value:** dropped commitments versus baseline, meeting-to-action
recall, median capture-to-disposition, human coordination time saved, tool hops
removed, reliance/trust score, and work still maintained outside Ocean.

Administrative-burden metrics include triage actions/person/day, median triage
session length, median human interactions per accepted item, percentage closed
with at most two human touches, snooze/digest use, and candidate dismiss rate by
source. The pilot begins with a target of no more than five steady-state triage
actions per person per day; exceeding the agreed budget is a product bug, not a
reason to demand more team discipline.

## 14. Repository ownership

| Owner | Responsibilities |
| --- | --- |
| `ocean-os` | Room work envelope/fold, local projection API/live tail, typed call facts, authenticated identity enforcement, generic extension/events/permissions seams |
| `ocean-surface` | Team Home, canonical Board/List/Triage/Attention/detail/timeline, Room/call capture interactions, mobile/PWA behavior |
| `ocean-bedrock` | Authenticated federated cross-Room work projection, global ordering, membership-filtered query/subscription, gap truth |
| `ocean-agents` | Conservative extractor and typed capability/tool boundary; no durable board authority |
| `risingtides-agents` | Source-specific voice/courier policy, ledgers, connector grants, dedupe, draft/send safety |
| Pasture/Stitchpad | Optional planning/agent-lane adapter and visible work mirror; never canonical authority |
| Team Manager extension | Organization policy, dispatch, leases, escalation, execution graph, acceptance automation after separate Crew authorization |

Cross-repository changes follow `docs/OCEAN_PROJECT_MAP.md`. Proximity in a
monorepo snapshot does not transfer ownership.

## 15. Ordered implementation program

No feature implementation starts until this manifest is accepted. Each gate
requires its own bounded review, clean ownership, verification, upstream
reconciliation, operator-facing rebuild where applicable, and devlog pass.

### Gate 0 — Recover and classify the current board work

1. Inventory every dirty board-related file separately from unrelated local
   changes.
2. Reproduce the existing narrow tests without declaring the feature shipped.
3. Independently review identity, locking, replay, schema, and client behavior.
4. Decide which prototype pieces are reusable under `ocean.work.item` and which
   are retired.
5. Reconcile the inaccurate “phase 1 done” chronology with recoverable WIP
   truth; do not rewrite history silently.

**Exit:** reviewed recovery branch/commit plan with no user work lost. Gate 0
does not authorize landing until its overlap with the dirty tree is resolved.

### Gate 1 — Freeze product, schema, governance, privacy, and pilot contracts

1. Accept or amend this manifest.
2. Publish exact `ocean.work.item` DTOs, operations, fixtures, state machine,
   assignment epoch, source/ref, conflict, and closure rules.
3. Freeze the governance matrix and identity prerequisites.
4. Freeze derived-data minimization and anti-surveillance rules.
5. Freeze Attention and triage models.
6. Freeze pilot cohort, SLO values, admin budget, and rollback.
7. Ratify migration/retirement for `ocean.board.card` v1.

**Exit:** operator acceptance plus independent review of the exact schema and
threat model. This proposal is the Gate 1 input, not its acceptance record.

### Gate 2 — Deterministic projection foundation

1. Implement the pure versioned work-event decoder and fold in the owning
   `ocean-os` crate.
2. Add golden fixtures and property tests for replay, order, per-field conflict,
   unsupported versions, epochs, and closure.
3. Implement pending-to-confirmed replacement by stable event identity.
4. Prove local/federated convergence and visible gap behavior.
5. Add bounded snapshot/cursor strategy before large-room claims.

**Exit:** hydrate + tail = replay, permutation/idempotency tests pass, pending
confirmation never duplicates, and two federated seats converge.

### Gate 3 — Canonical first-party Board/List/detail

1. Build canonical Surface routes backed only by the projection.
2. Repair TUI identity and expose equivalent honest commands/views.
3. Add timeline, source/redaction state, extracted-vs-edited fields, undo,
   filters, bulk controls, WIP/load signals, and accessibility.
4. Bind Surface and TUI to authenticated federated identity.
5. Add list-first mobile interactions, touch tests, and drag alternatives.
6. Keep session Kanban a preview/deep-link surface only.

**Exit:** two devices and one authorized agent seat converge on the same Room;
unauthorized viewers cannot infer protected fields; mobile does not require
drag or desktop layout.

### Gate 4 — Manual, Room, and call capture

1. Ship explicit manual/Room creation.
2. Persist call candidates with source and confidence.
3. Ship batch/aging/owned triage and durable call-end disposition checks.
4. Make detector recall limitations visible and measured.
5. Add conflict disposition and merge tests.

**Exit:** a real call produces cited candidates, accepted work survives
restart/federation, dismissed work stays dismissed, and unresolved items make
one Attention item.

### Gate 5a — Closed-loop human assignment

1. Implement typed human/team-queue assignment and epochs.
2. Ship one-tap progress, claim/handoff, Review, evidence, accept, reject, and
   reopen.
3. Add notification/snooze/quiet-hours behavior and stale attention.
4. Prove late former owners cannot close the current epoch.

**Exit:** one human commitment closes with evidence and acceptance, survives
restart, and remains auditable from source to outcome.

### Gate 5b — Agent execution

Blocked pending a separately accepted Crew Stage B-or-later implementation
manifest. That manifest must own execution request identity, leases,
heartbeats, cancellation, continuation, effects, permission/budget ceilings,
late results, restart recovery, and acceptance.

**Exit:** not defined or authorized here.

### Gate 6 — Private-source connectors

1. Add Gmail/Slack/authorized-message intake behind explicit grants.
2. Reuse courier/source ledgers and idempotency.
3. Keep source access independent from work-view access.
4. Expose connector lag, revoked access, retries, and mirror drift.

**Exit:** repeated ingest creates no duplicates, revocation stops new reads,
and unauthorized viewers learn no protected source content or aggregate fact.

### Gate 7 — Team Home and daily rhythm

1. Build the access-filtered front page and cited brief.
2. Add deduplicated Attention, Rooms pulse, human/agent state, and sync health.
3. Add opt-in courier delivery and one quiet-hours-aware escalation step.
4. Enforce SLO degradation states and administrative-burden metrics.

**Exit:** the pilot can run a daily stand-up from Team Home for the agreed
window without a hidden spreadsheet and without exceeding the interaction
budget.

### Gate 8 — Mirrors, scale, and production hardening

1. Add optional GitHub/Linear mirrors with visible per-mirror state.
2. Run large-Room, partition, offline/reconnect, pagination, load, and recovery
   tests.
3. Complete privacy/security review, rollout cohorts, support, backup/rebuild,
   rollback, and incident runbooks.
4. Compare correctness and organizational-value metrics to pilot baseline.

**Exit:** the product meets accepted SLOs, privacy tests, burden budget, and
recovery objectives under production-like scale.

## 16. Required adversarial scenarios

At minimum, automated or scripted acceptance covers:

- duplicate webhook, Room message, call segment, and connector retry;
- pending local event later confirmed globally without duplicate application;
- late former-assignee completion after reassignment;
- call candidate accepted during call, then no false call-end alert;
- one item simultaneously overdue, blocked, and awaiting approval appears once;
- conflicting owner/due/cancellation facts across sources;
- unsupported envelope version and malformed event;
- federation partition, missing global range, recovery, and revocation;
- unauthorized Room, source, aggregate count, title, owner, and brief access;
- redacted provenance remains visibly redacted;
- source edit/delete after candidate acceptance;
- connector revoked during retry;
- agent timeout, canceled lease, late result, permission block, and budget stop
  after Gate 5b is separately authorized;
- mirror outage and divergent external status;
- two-device concurrent move/reassign/comment;
- Room rename/archive/member removal;
- mobile triage without drag and with accessible undo;
- rebuild from ledger produces the same authorized projection.

## 17. Stop conditions

Stop and amend this manifest if implementation would:

- put durable work authority in Surface, TUI, a connector, or a second database;
- trust client-asserted identity or source access;
- add a core task scheduler, named-worker runtime, organization policy, or Crew
  engine;
- begin Gate 5b without a separately accepted Crew implementation manifest;
- hide unsupported events, ordering gaps, redaction, or connector failure;
- expose aggregate facts a viewer cannot independently access;
- auto-assign people from passive extraction without accepted governance;
- mark work Done without evidence/acceptance policy;
- reuse the session Kanban as canonical state;
- require destructive handling of the current dirty checkout;
- exceed the pilot administrative budget without treating it as a launch
  blocker.

## 18. Verification and review

Every code gate runs the nearest owning repository checks plus cross-repository
fixtures. For `ocean-os`, the completion floor is:

```bash
cargo check --workspace
cargo check --workspace --tests
cargo xtask ci --compatibility
cargo +1.88.0 xtask ci --msrv
cargo xtask ci
```

Docs-only review runs:

```bash
cargo xtask docs-check
git diff --check
```

Feature, security, protocol, and architecture changes require fresh independent
review. A feature is not delivered from a stash, dirty tree, detached worktree,
or unmerged branch. Shipping also requires upstream reconciliation, canonical
checkout fast-forward, and rebuild/reinstall of affected operator-facing
binaries under the repository's standing operational contract.

## 19. Definition of product success

Ocean Team Manager succeeds when a small team can trust one transparent system
to collect possible commitments from authorized Rooms/calls/messages, decide
which are real with low administrative effort, assign them safely to humans or
agents, see blockers and uncertainty early, and close each accepted loop with
evidence and accountable acceptance.

It has not succeeded merely because cards can be moved. It has succeeded when
the team spends less time coordinating, drops fewer commitments, keeps private
context private, and can reconstruct why every obligation exists and how it
ended.
