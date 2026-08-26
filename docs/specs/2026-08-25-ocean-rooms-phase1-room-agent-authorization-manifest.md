# Ocean Rooms — Phase 1 implementation manifest: local room-agent authorization

**Status:** proposed; requires independent review and operator acceptance before any route, schema, or migration lands
**Date:** 2026-08-25
**Phase:** 1 of the Decision 6 capability delivery order
**Authorizing documents:**
[architecture](2026-08-16-ocean-rooms-distributed-workspace-architecture.md) (ratified 2026-08-17) ·
[Gate 0 decisions and threat model](2026-08-17-ocean-rooms-gate0-decisions-and-threat-model.md) (accepted)
**Boundary:** Gate 0 §8

## 0. What this manifest is, and what it refuses to be

Decision 6 fixes the delivery order. Stage 1 is **local room-agent
authorization** and nothing else. This document specifies that stage exactly
enough to implement and review, and it is deliberately useless for anything
further.

Per Gate 0 §8 this manifest **does not include, and its implementation must not
introduce**: Tailscale or any network-identity binding, node enrollment,
resource grants, remote execution, package transfer, worker graphs, or
multi-node scheduling. A pull request implementing this manifest that touches
those surfaces is out of scope by definition, not by judgement.

The single question Phase 1 answers is:

> **By what durable, revocable, attributable authority does a specific agent
> act inside a specific room on this machine?**

Today there is no answer. `RoomParticipant` is `{ id, kind, display_name }` —
a *label*. Nothing about it is authorizing, nothing pins what code runs, and
nothing records who permitted it. Phase 1 supplies the missing authority
record. It adds no new capability to any agent; on the contrary, its correct
implementation makes some things that work today stop working until an
operator authorizes them.

### 0.1 Why this must land before contributed folders

Decision 6's ordering is "intentionally hard to reverse; it prevents a broad
remote shell from becoming the accidental foundation." Folder grants
(stage 2) name *authorized_agent_member_ids*. Remote reads (stage 3) resolve
authority through the room-agent binding. Every later stage takes the binding
as an input it does not validate for itself. If the binding is a display label
when stage 2 ships, every subsequent authority check is decoration.

## 1. Ownership: where the binding lives, and why not Bedrock

**The durable owner of a room-agent binding is the local Ocean daemon**, in the
existing room store (`<config_dir>/rooms.db`, `ocean-store`). It is not the
room coordinator, and specifically not Ocean Bedrock.

The architecture is explicit on both halves of this:

- §5.1 — "For an authorized Rising Tides deployment, Ocean Bedrock may
  implement the authenticated shared room service." Bedrock is a legitimate
  control plane.
- §5.1 — "The coordinator never becomes local execution authority."

A binding is execution authority. It therefore lives on the machine that will
enforce it. A coordinator-owned binding would mean a hosted service could grant
an agent the right to act on an operator's computer, which inverts the local
custody invariant (§12.1) and would make Bedrock availability a precondition
for local work.

This also satisfies the public-Ocean constraint: Phase 1 works with **no
coordinator at all**. A standalone daemon with a local room authorizes agents
and enforces them. Federation, when present, contributes compatibility inputs
only (§7).

### 1.1 Consequences of local ownership

- Two nodes in the same federated room may hold **different** bindings for the
  same room-agent identity. This is correct, not a conflict to reconcile: each
  operator independently decides what may run on their machine.
- Revocation is local and immediate. It does not require, and must not wait
  for, coordinator reachability.
- There is no binding sync protocol in Phase 1. Introducing one is a separate
  manifest with its own threat model.

## 2. Records

### 2.1 `room_agent_binding` (new, daemon-owned)

The durable authority record. One row per (room, agent member identity).

```text
RoomAgentBinding
  room_id                     TEXT NOT NULL          -- room key
  agent_member_id             TEXT NOT NULL          -- stable room-scoped agent identity
  agent_package_id            TEXT NOT NULL          -- what code this is
  agent_definition_digest     TEXT NOT NULL          -- pinned content digest at authorization
  agent_definition_revision   TEXT                   -- human-readable revision, display only
  display_name                TEXT NOT NULL
  owner_member_id             TEXT NOT NULL          -- the human who owns this agent in-room
  authorized_by               TEXT NOT NULL          -- operator principal id (§3)
  authorized_at               TIMESTAMPTZ NOT NULL
  activation_policy           TEXT NOT NULL          -- §5
  context_policy              TEXT NOT NULL          -- §6
  memory_scope                TEXT NOT NULL          -- §6
  requested_capabilities      JSON NOT NULL          -- what the package asked for
  room_capability_grants      JSON NOT NULL          -- what the operator allowed
  status                      TEXT NOT NULL          -- active | suspended | stale | revoked
  generation                  INTEGER NOT NULL       -- bumped on every authority change
  decision_id                 TEXT NOT NULL          -- replay key (§3.3)
  request_digest              TEXT NOT NULL          -- what was approved (§3.3)
  revoked_at                  TIMESTAMPTZ
  revoked_by                  TEXT

  PRIMARY KEY (room_id, agent_member_id)
```

`generation` exists for the same reason `LocalRoomResourceGrant.generation`
does in the architecture (§7.4): it prevents a request authorized under old
authority from surviving a re-authorization. Every admission carries the
generation it was planned against and is refused if it no longer matches.

**Nothing in this record is federated in Phase 1.** `authorized_by`,
`request_digest`, `decision_id`, and the capability sets are local operator
facts; projecting them would leak operator identity and policy into a hosted
service for no Phase 1 benefit.

### 2.2 Status semantics

| status | new execution | existing in-flight | recovery |
| --- | --- | --- | --- |
| `active` | admitted | continues | — |
| `suspended` | **refused** | cancelled at next checkpoint | operator resumes; generation bumps |
| `stale` | **refused** | cancelled at next checkpoint | re-authorization required (§4) |
| `revoked` | **refused** | cancelled at next checkpoint | terminal; a new binding is a new row |

`stale` is entered automatically and only by the digest check in §4.2. An
operator never sets it. This distinction matters for the UI: `stale` means "the
code changed", `suspended` means "a human paused this", and the Surface must
not render them identically (cf. architecture §14 on not conflating states).

## 3. The authorizer: local operator principal

This is the part with no existing foundation, and the part most likely to be
implemented wrongly by accident.

### 3.1 The problem

The daemon binds `127.0.0.1:4780` and has historically treated every caller as
the operator, with `OCEAN_YOLO=1` as the default. That is defensible for a
loopback-only tool. It is **not** defensible as the basis for a durable
authority record, because the surface proxy legitimately binds
`0.0.0.0:8790` and forwards to the daemon. Any tailnet peer, and any web page
the operator visits, can therefore reach an authorization route unless one is
explicitly built to reject them.

Gate 0 §8 requires "fail-closed stop if authenticated authorizer identity is
unavailable." Phase 1 must therefore introduce an authorizer identity rather
than infer one.

### 3.2 Operator principal

An **operator principal** is established at daemon start and is the only
identity that may create, modify, suspend, or revoke a binding.

- Its credential is a high-entropy secret generated on first run and stored at
  `<config_dir>/operator.key`, mode `0600`, owned by the daemon's user.
- Authorization routes require it in an `X-Ocean-Operator` header. It is never
  accepted from a query string, a cookie, or a request body — the same
  header-only rule Bedrock's room event stream is frozen under, for the same
  reason: a credential that can ride in a URL will eventually be logged.
- **If the key file is absent or unreadable, authorization routes return `503`
  and no binding may be created or changed.** Read-only inspection stays
  available. This is the required fail-closed stop; it must not degrade to
  "assume the caller is the operator."
- `OCEAN_YOLO` has no effect on authorization routes. It governs per-call tool
  gating for an already-authorized agent. Conflating the two would make the
  operator default silently disable the authority model.

### 3.3 Replay-safe approval

Every mutation carries an operator-generated `decision_id` (UUID) and the
daemon computes a `request_digest` over the canonical approved content
(room, agent identity, package id, definition digest, activation policy,
context policy, memory scope, capability grants).

- First submission: applied; `decision_id` and `request_digest` are stored.
- Re-submission with the **same** `decision_id` and the **same**
  `request_digest`: returns the existing binding, `200`, no state change.
  A retried request after a lost response is safe.
- Re-submission with the same `decision_id` and a **different**
  `request_digest`: `409 decision_replay_mismatch`, no state change. An
  approval for one thing can never be replayed to authorize another.

`decision_id` is unique per room across all bindings, so a decision approved
for agent A cannot be reused for agent B.

### 3.4 Browser anti-CSRF boundary

Authorization routes reject any request that:

- carries a `Cookie` header (the daemon has no cookie auth; its presence means
  a browser is being driven, and ambient credentials must never authorize);
- carries an `Origin` or `Referer` not in the configured allowlist
  (default: the local Surface origin only); or
- lacks the `X-Ocean-Operator` header, which a cross-origin form post cannot
  set.

The header requirement alone defeats classical CSRF; the `Origin` check is
defence in depth and the cookie rejection is a tripwire for a future change
that adds session auth without revisiting this boundary.

## 4. Pinned package identity and revision behavior

### 4.1 What is pinned

At authorization the daemon resolves the agent package and records
`agent_definition_digest` — a content digest over the package's authoritative
definition (instructions, declared tools, declared capability requests, model
role binding). The digest, not the version string, is authority. A revision
label is stored for display only and is never compared for admission.

### 4.2 Drift detection

Before **every** admission the daemon recomputes the digest of the package it
is about to run and compares it to the binding.

- Equal → proceed.
- Different → transition the binding to `stale`, refuse admission with
  `409 binding_stale`, and emit an audit fact naming both digests.

This is the mechanism behind the architecture's requirement that "changing
requested capabilities requires re-authorization" (§7.1). It is enforced by
digest rather than by trusting a package to declare its own change, because a
package that can declare "nothing changed" can lie.

### 4.3 Re-authorization

Re-authorization is a fresh operator decision over the new digest. It preserves
`agent_member_id` — so transcript history, attribution, and any future
`authorized_agent_member_ids` reference remain stable — and bumps `generation`.
It is not an edit; it is a new approval of new content under a stable identity.

## 5. Room role and activation policy

`activation_policy` is one of:

| policy | meaning |
| --- | --- |
| `explicit_only` | acts only when directly invoked by a room member |
| `mention` | additionally acts when @mentioned in the room |
| `task_and_thread` | additionally acts on assigned tasks and replies within its own threads |

**Default is `explicit_only`.** Gate 0 Decision 7 governs approval defaults;
this manifest's contribution is that the *quietest* policy is the default and
widening it is an explicit operator act recorded in `generation`.

Activation policy is not a capability. It decides *when the agent is asked*,
never *what it may do*. An agent with `task_and_thread` and no capability
grants can talk and nothing else.

Room role in Phase 1 is limited to the existing owner/member distinction:
**only a room member holding the owner role may authorize agents into that
room.** Delegated authorization roles are deferred.

## 6. Room-scoped context and memory boundaries

`context_policy`:

| policy | the agent's turn may read |
| --- | --- |
| `invocation_only` | only the invoking message and its direct thread |
| `room_recent` | additionally a bounded recent window of room transcript |
| `room_history` | additionally durable room history via explicit retrieval |

`memory_scope`:

| scope | writes persist to |
| --- | --- |
| `none` | nothing |
| `room` | a room-partitioned memory namespace |

**Hard boundary:** a room-scoped agent turn may not read or write the operator's
global agent memory, and may not read another room's partition. Memory keys are
prefixed by `room_id` at the store boundary rather than by caller convention,
so a prompt-injected agent cannot address another room's namespace by
constructing a key.

Cross-room memory synchronization is Gate 0 Decision 10 and is out of scope
here; Phase 1 only has to not violate it.

## 7. Capability request and runtime grant intersection

The architecture fixes the rule (§6): authority is an intersection, never a
union, and "no layer may widen another layer."

Phase 1 implements the locally-available subset of that intersection:

```text
effective capability set =
    package requested_capabilities        (what the code asks for)
  ∩ binding room_capability_grants        (what the operator allowed)
  ∩ runtime permission decision           (what the gate allows at call time)
```

Node policy, resource grants, and execution-instance budgets are later stages
and must be added as further intersection terms, never as alternatives.

Implementation requirements:

- The intersection is computed **once per turn at admission** and carried on the
  execution instance. A capability absent at admission cannot appear mid-turn.
- A capability the package did not request is unavailable even if the operator
  granted it — the operator can only ever narrow.
- An empty intersection is a valid, admissible state: the agent runs with
  conversational ability and no tools.
- The runtime permission gate is unchanged by this manifest. It is the third
  term, not a replacement for the first two.

## 8. API semantics

All routes are daemon-local, operator-authenticated per §3, and namespaced
under the existing persistent-rooms surface.

| Method | Route | Purpose |
| --- | --- | --- |
| `POST` | `/v1/rooms/persistent/{key}/agents` | authorize an agent (add) |
| `GET` | `/v1/rooms/persistent/{key}/agents` | inspect bindings in a room |
| `GET` | `/v1/rooms/persistent/{key}/agents/{agent_member_id}` | inspect one binding |
| `POST` | `/v1/rooms/persistent/{key}/agents/{agent_member_id}/reauthorize` | approve a new digest |
| `POST` | `/v1/rooms/persistent/{key}/agents/{agent_member_id}/suspend` | pause |
| `POST` | `/v1/rooms/persistent/{key}/agents/{agent_member_id}/resume` | unpause |
| `DELETE` | `/v1/rooms/persistent/{key}/agents/{agent_member_id}` | revoke (terminal) |

Semantics:

- **Add** requires operator principal, room owner role, `decision_id`, and an
  explicit `room_capability_grants` (an omitted grant set is `{}` — never "all
  requested"). Deny-extra body.
- **Inspect** never returns `authorized_by` credentials, the operator key, or
  raw package contents. It returns digests, policies, grants, status, and
  generation.
- **Re-authorize** is §4.3.
- **Suspend/resume** bump `generation` and take effect on new admissions
  immediately; in-flight executions cancel at their next checkpoint.
- **Revoke** is terminal for that row. Re-adding creates a new binding with a
  new `agent_member_id`, so a revoked identity can never be resurrected — the
  same rule Bedrock's room membership uses for removed members.

Every mutation emits a durable room audit fact (§10).

### 8.1 Existing agent routes

`POST /v1/rooms/persistent/{key}/members/agents` currently registers agent
participants without authorization content. Under this manifest it becomes a
**compatibility route** (§9): it may continue to create participant labels, and
it must not create, imply, or upgrade a binding. It is marked deprecated in the
same change that ships the routes above.

## 9. Compatibility: existing participants and federation bindings

Gate 0 §8 requires current participant ownership and federation bindings to be
"explicitly treated as non-authorizing compatibility inputs." Concretely:

1. Existing `RoomParticipant { kind: Agent }` rows keep rendering in rosters and
   transcripts. Attribution of historical messages is unchanged. **They confer
   no authority.**
2. Federated agent descriptors arriving through the room bridge
   (`register_agents` / `public_agent_descriptor`) are display projections.
   They are never read as authorization on this node.
3. Any execution path that admits an agent turn **must** consult
   `room_agent_binding`. Absence is refusal, not fallback.
4. There is **no implicit migration** that converts existing participants into
   bindings. Every binding is an operator decision. This is the intended
   breakage: agents that "work" today via a name picker stop acting until
   authorized, and the Surface must explain exactly that (§11).

Migration therefore adds a table and rewrites no existing row.

## 10. Audit and attribution

Every authorization mutation and every admission decision emits a durable fact
carrying: room id, agent member id, package id, both digests where relevant,
generation, operator principal id, decision id, outcome, and reason code.

Content boundary (Gate 0 Decision 13): audit facts record *that* a decision
occurred and under what authority. They do not embed package source, prompt
text, or capability payloads.

Attribution requirement: a message or side effect produced by an agent turn is
attributed to its `agent_member_id` **and** the binding `generation` in force at
admission. An observer reading history must be able to tell that a message was
produced under authority that has since been revoked.

## 11. Surface authorization flow

The Surface today offers a bare global-name picker sourced from `GET /v1/agents`.
That picker is the visible form of the missing authority model and is replaced.

Required flow:

1. Operator opens a room's **Agents** panel (owner role only; others see a
   read-only roster).
2. Choosing a package shows: package identity, resolved definition digest, the
   capabilities the package **requests**, and a plain statement that granting is
   narrowing-only.
3. The operator sets activation policy (default `explicit_only`), context
   policy, memory scope, and ticks the capabilities to grant. Nothing is
   pre-ticked.
4. Confirmation shows exactly what will be authorized and issues a
   `decision_id`.
5. The panel renders `active` / `suspended` / `stale` / `revoked` as visibly
   distinct states. `stale` explains that the package changed and offers
   re-authorization showing a **digest diff summary**, never a silent re-pin.

The Surface must not present an agent as available in a room when no active
binding exists. An unauthorized agent is absent, not greyed-out-and-clickable.

## 12. Tests

The manifest is not implementable without these passing. Each is a required
test, not a suggestion.

**Authority**

1. An agent turn with no binding is refused.
2. A `suspended`, `stale`, or `revoked` binding refuses new admission.
3. A binding for room A does not admit in room B.
4. A capability granted but not requested by the package is unavailable.
5. A capability requested but not granted is unavailable.
6. The runtime gate can still deny a capability that survived both.
7. Admission planned against generation N is refused after a bump to N+1.

**Operator identity**

8. Missing `X-Ocean-Operator` → `503` on mutations, `200` on inspection.
9. An unreadable/absent operator key → mutations `503`, and **no** fallback to
   ambient trust.
10. `OCEAN_YOLO=1` does not permit an unauthenticated mutation.
11. A request bearing a `Cookie` header is rejected.
12. A cross-origin `Origin` is rejected.
13. A non-owner room member cannot authorize.

**Replay**

14. Same `decision_id` + same `request_digest` → idempotent `200`, single row.
15. Same `decision_id` + different `request_digest` → `409`, no state change.
16. A `decision_id` from agent A cannot authorize agent B.

**Pinning**

17. A package edited after authorization moves the binding to `stale` on next
    admission and refuses.
18. Re-authorization preserves `agent_member_id` and bumps `generation`.
19. A revision-label change with an identical digest does **not** cause `stale`.

**Memory and context**

20. A `memory_scope: room` agent cannot read global operator memory.
21. Room A's agent cannot read room B's memory partition, including via a
    constructed key.
22. `invocation_only` context does not receive unrelated transcript.

**Compatibility**

23. An existing agent participant with no binding renders in the roster and is
    refused admission.
24. A federated agent descriptor does not create or imply a binding.
25. Historical attribution of pre-migration messages is unchanged.

**Migration**

26. Migration is additive: no existing row is rewritten.
27. Rollback (§13) restores pre-migration behavior with no orphaned state.

## 13. Migration, rollback, downgrade

- **Forward:** one additive migration creating `room_agent_binding` and its
  indexes. No existing table is altered. No data is backfilled.
- **Rollback:** dropping the table returns the daemon to label-only agent
  participants. Because nothing else was rewritten, rollback is lossless with
  respect to pre-existing data; authorizations themselves are lost, which is
  the safe direction.
- **Downgrade:** an older daemon binary ignores the unknown table. It will
  admit agents by the old permissive path — so **downgrade is a security
  regression, not merely a feature regression**, and must be documented as such
  in the release note rather than treated as a routine rollback.
- **Forward/back compatibility of the store:** the table is created with
  `IF NOT EXISTS` and re-running the migration is idempotent, matching the
  convention used by the federated-rooms migrations.

## 14. Cross-repository validation and rollout gates

**`ocean-os`** — store migration, binding records, operator principal, routes,
admission intersection, audit facts, and every test in §12.

**`ocean-surface`** — the §11 flow replacing the global-name picker, and the
distinct rendering of the four statuses.

**`ocean-bedrock`** (authorized deployment only) — **no change is required or
permitted by this manifest.** Federation continues to project agent descriptors
as display data. Any Bedrock change that makes a federated descriptor
authorizing on a node violates §1 and §9.

Rollout gates, in order:

1. `ocean-os` implementation merged with §12 green.
2. Independent review confirming the §7 intersection is computed at admission
   and carried immutably, and that §3 fails closed.
3. `ocean-surface` flow merged; the bare picker is gone, not merely hidden.
4. A migration rehearsal on a copy of a real `rooms.db`, including rollback.
5. Operator acceptance recorded in `ROADMAP.md`, with the Phase 1 checkbox
   closed and stage 2 (contributed folders) explicitly **not** opened.

## 15. Open questions for review

These are deliberately unresolved; a reviewer should close them before
acceptance.

1. **Operator key rotation.** §3.2 defines a key but no rotation. Should
   rotation invalidate in-flight admissions, or only future mutations?
2. **Owner-role source of truth.** Phase 1 reads room owner role from the local
   room store. In a federated room the coordinator also has an opinion. Which
   wins locally, and does a federated demotion suspend local bindings?
3. **Digest scope.** Should the digest cover the model role binding? Pinning it
   makes a `[roles]` edit stale every binding; excluding it means authorized
   code can silently run on a different model.
4. **`stale` and in-flight work.** §2.2 cancels in-flight executions at the next
   checkpoint. For a long build that may be worse than letting the turn finish
   under the old digest. Should `stale` be admission-blocking only?
5. **Multi-node divergence.** §1.1 accepts that two nodes may hold different
   bindings for one room-agent. The Surface has no way to show an operator that
   their teammate refused an agent they authorized. Is that acceptable for
   Phase 1?
