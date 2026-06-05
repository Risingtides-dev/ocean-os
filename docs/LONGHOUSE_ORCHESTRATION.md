# Longhouse — non-hierarchical agent coordination on Ocean's one event bus

> Status: **design**. Replaces `COUNCIL_ORCHESTRATION.md` (the chair-led council is
> discarded — see "Why the council died" below). Rides on the shipped
> `CapabilityRegistry` + `ocean-mcp` work and the existing session / SSE / turn-injection
> model. New daemon surface is deliberately small.
>
> One-line thesis: **emergent coordination for exploration, escrowed authority for
> termination.** No standing chair. Agents converge on a shared blackboard by quorum;
> a separate, non-working *escrow* function (titles, not persons) decides when it's over
> and can pull authority. Every guardrail here is **daemon-enforced**, not LLM-trusted.

---

## 0. Why the council died (and what survives)

`COUNCIL_ORCHESTRATION.md` put a **chair** at the center: a lead agent that decides who
speaks next, curates every worker's input, and declares consensus. That is the
orchestrator-worker pattern (Anthropic's research system, Brief 2) — and it is exactly
the single point of cost, latency, capture, and failure the non-hierarchical literature
exists to remove. It also re-imports a Roman org chart agents don't need: the chair burns
a strong model on every routing decision, becomes the bottleneck the whole system waits
on, and is the one thing whose capture compromises everything.

What survives from the council doc, re-homed:

| Council concept | Fate in Longhouse |
|---|---|
| One SSE event bus tagged by `session_id` | **Kept verbatim** — it is the substrate. |
| Turn-injection as the steering primitive | **Kept** — it's how convergence and termination write into a session. |
| Blackboard (shared deliberation artifact) | **Promoted to a first-class primitive** (stigmergy), not chair-curated. |
| `spawn_worker` etc. as registry capabilities | **Kept**, re-scoped: any agent can convene, none chairs. |
| Chair decides who speaks | **Deleted.** Replaced by domain-routed convening + quorum. |
| Chair declares consensus | **Deleted.** Replaced by daemon-computed quorum + an escrow `firekeeper` role. |
| `max_rounds` / token budget | **Kept and hardened** into daemon-enforced termination. |

---

## 1. Philosophy — why agents flip the org chart

The flat-org literature (Brief 3) documents *why* human flatness fails: Zappos lost
~18-29% of staff to Holacracy because flatness without governance produced coordination
collapse, untrackable accountability across 10-20 micro-roles, and informal power capture.
Co-ops "degenerate" toward hierarchy under scale. The elected-dictatorship theorem
(arXiv:2506.07935) proves you cannot have *full* flatness AND a single accountable party.

**But almost every reason human flatness fails is a property of humans, not of coordination
itself.** This is the sharp claim the design rests on. Walk the failure causes:

1. **Manufacturing trust / legitimacy.** Most of the Haudenosaunee apparatus (Brief 1) —
   the Condolence ceremony's grief-processing, the three-warning reputation gradient,
   kinship obligation, the lack of central coercive enforcement — exists because humans can
   defect, mourn, die, and must be *persuaded* to comply. **Ocean agents share an
   enforcement substrate.** The daemon already gates every mutating tool call through
   `PermissionPolicy` and serializes turns on a per-session lock. The orchestrator can
   simply *refuse to run a turn*. So the entire legitimacy-manufacturing layer — the single
   biggest fraction of human governance — is **dead weight**. We drop it. (It returns only
   across trust boundaries; see §6 and the Kaswentha note.)

2. **Bounded bandwidth.** Humans can't read 40 transcripts. Pre-aggregation through nested
   councils (Brief 1) and the chair's curation both exist to keep a central human from
   drowning. Agents read the bus in parallel; a blackboard projection is cheap. We keep
   *load-shedding* (subsidiarity) because it saves tokens, not because anyone would drown.

3. **Career / recognition / mentorship.** Zappos' ladder collapse, co-op member apathy —
   irrelevant. Titles here are addressable strings, not careers.

4. **Grief and death.** The "requickening" that transfers a dead chief's qualities to a
   successor maps to a **state handoff**, nothing more. An agent crash is a `kill -9`, not
   a funeral.

What does *not* dissolve under "they're just agents," and therefore must be **built**:

- **Deadlock on symmetric options is physics, not psychology.** Lamport's Buridan's
  Principle proves any deterministic continuous decider has a balanced midpoint with
  unbounded decision time. Two equally-good plans hang forever without an inhibitory
  channel + a forced tie-break. Agents are *more* exposed to this than humans (no
  impatience, no boredom, no "let's just pick one"). **Must build:** cross-inhibition +
  hard timeout (§5).
- **Accountability dissolution is a theorem, not a vibe.** The only diffusion-free
  decision among 2+ agents is an elected single decider. So termination authority **must**
  collapse to one signed, logged actor — the escrow `firekeeper` role (§3). A swarm cannot
  be "the one to blame."
- **Sybil capture is cheaper for agents than for anyone.** A human can't fork 50 copies of
  themselves to fake a quorum; an agent runtime's whole job is `spawn_worker`. Quorum that
  counts messages instead of credentials is trivially flooded (§6).

So the org chart flips because **agents lack the constraints the hierarchy was compensating
for** (trust, bandwidth, mortality, careers) **while retaining the few that are
structural** (deadlock, accountability, capture). Longhouse keeps the three structural
defenses and discards the rest. That is the entire design in one sentence.

Two load-bearing imports from Brief 1, both *structural* not *cultural*:

- **Authority held in escrow by a body that does not wield it.** Clan mother *owns* the
  title; sachem *exercises* it but cannot renew or transfer it; War Chief *executes*
  removal. Grant / act / revoke live in three different hands. This is the EROS-style
  revoker-capability pattern, and it is the most valuable thing the Haudenosaunee material
  offers software. It gives fail-safe, auditable de-authorization where the worker can
  never self-perpetuate. Longhouse's escrow function is built on exactly this split.
- **Title persistence: the office never dies, the holder is swappable.** A role is a
  durable addressable identity with attached state and obligations; the worker instance
  bound to it is interchangeable. This is routing stability + state continuity, and it's
  what lets a recalled or crashed agent be replaced without losing the thread.

---

## 2. The three coordination primitives, as Ocean mechanics

These are the load-bearing constructs. Everything else is wiring. Each is defined as a
concrete daemon mechanic, not a metaphor.

### 2.1 Stigmergy — the blackboard as a decaying signal field

**Principle (Briefs 1+2):** agents coordinate through a shared *medium* they read/write,
never by addressing each other. Termites build by reacting to the structure, not to other
termites (Grassé). Digital pheromones (Parunak) aggregate, propagate, and **evaporate**.
Hearsay-II knowledge sources watch a blackboard and fire on triggers. Honeybees signal site
quality and **commit on a quorum threshold**, not on command.

**Ocean mechanic.** A **blackboard** is a dedicated daemon-owned session
(`kind = "blackboard"`) per *convened topic*. It is *not* a chat transcript an LLM curates —
it is an append-only, daemon-maintained store of **typed marks**:

```rust
// ocean-longhouse crate
pub struct Mark {
    pub id: Uuid,
    pub board_id: SessionId,          // the blackboard session this rides
    pub author: AgentSessionId,       // signed: which working session emitted it
    pub kind: MarkKind,               // proposal | endorse | inhibit | evidence | note | converged
    pub target: Option<Uuid>,         // proposal a vote/inhibit refers to
    pub weight: f32,                  // current strength (decays)
    pub body: Value,                  // claim, patch ref, rationale, etc.
    pub credential: CredentialRef,    // §6 — what gives this author quorum weight
    pub at_ms: i64,
    pub ttl_ms: i64,                  // decay horizon; 0 = non-decaying (proposals)
}
```

Three stigmergic properties, all **daemon-computed** so no LLM is trusted to do arithmetic:

- **Aggregate.** The daemon sums `endorse` weights per proposal across all authors, minus
  `inhibit` weights aimed at it (cross-inhibition, §5). This running tally is the
  "pheromone level" of each proposal.
- **Decay.** Every mark's effective weight is `weight * 2^(-Δt / ttl_ms)`. A signal nobody
  re-asserts fades to zero. This is the built-in GC for stale coordination state — and the
  *direct* fix for the documented Ocean token-burn problem (`MEMORY.md`: quadratic context
  resend): old marks stop being projected into prompts because they've decayed out.
- **Project, don't dump.** Agents never ingest the raw board. The daemon renders a **bounded
  projection** — top-K live proposals by net weight, recent evidence, current quorum
  distance — injected as the convened agent's next turn. Bounded by construction, so the
  context-explosion failure of mesh topologies (council doc's rejected design) cannot recur.

Agents write marks with one new tool, `board_post` (§4), which is a thin wrapper over the
existing `ToolSideEffect` emit path — the same mechanism `component_render` already uses to
push events onto the bus. They read by being convened (turn-injected with the projection).

### 2.2 Subsidiarity — domain-routed convening; most things never convene

**Principle (Brief 1):** the Grand Council has narrow enumerated jurisdiction — war, peace,
treaty, inter-nation disputes. Villages decide local matters with no escalation. Positions
pre-aggregate bottom-up before anything reaches the central fire. **Most matters never
escalate.** This is load-shedding, and Brief 1 flags it as "highly translatable and
underused."

**Ocean mechanic.** A turn does **not** convene anything by default. A single session
handles its own work, exactly as today. Convening is gated by an explicit, **daemon-checked
escalation predicate**:

```
convene(topic, domain) is permitted to escalate to a blackboard ONLY if the triggering
action is any of:
  (a) shared-state:   it writes state ≥2 sessions read (e.g. a shared schema/types file),
  (b) irreversible:   it is externally visible / not cheaply undoable (deploy, spend,
                      destructive bash, prod data, sending a message), or
  (c) cross-boundary: it crosses a trust boundary (different operator/workspace; §6).
Otherwise the action stays local. No council. No blackboard. No quorum cost.
```

`(b)` reuses machinery that already exists: Ocean tools already declare
`requires_permission()` (bash/write/edit = true). Irreversibility ≈ that bit plus a small
allow-list (deploy/spend/delete verbs). `(a)` is a path-overlap check against other live
sessions' write sets. `(c)` is workspace-root mismatch (sessions are already
workspace-bucketed). So the predicate is computed from data the daemon already has.

This caps coordination cost hard: the expensive quorum path runs only for the rare
high-stakes, multi-agent, irreversible decisions — which is also exactly where Brief 3 says
the slow/inclusive method earns its keep ("reversible/cheap → speed; irreversible/
consequential → consensus"). Cheap reversible work never pays the tax.

**Domain routing replaces the chair's "who speaks next."** A topic carries a `domain`
(e.g. `schema`, `frontend`, `security`, `deploy`). Convening notifies sessions whose
declared **competence tags** intersect the domain — not a chair's choice, a routed match.
Competence tags come from the same place a session's toolset comes from (the skill/role it
was spawned under). Nobody is summoned by a manager; the domain summons the competent.

### 2.3 Authority-in-escrow — grant / exercise / revoke split three ways

**Principle (Briefs 1+3):** the constitutional core. Three principals over one office: clan
mother *owns* the title, sachem *exercises* it (and provably cannot self-renew or name a
successor), War Chief *executes* deposition. Termination of a mandate is a **separate
function from the work**, routed back to the title-holder (fail-safe, never into a vacuum).
Brief 3 confirms this is what *every surviving* flat system smuggles back in: Haudenosaunee
recall, Apache's justified binding veto held only by committers, the elected-dictator
theorem. "Emergent coordination for exploration, escrowed authority for termination" *is*
this primitive plus the bee algorithm.

**Ocean mechanic.** Three distinct capability principals, none of which is a working agent,
each emitting a distinct event type (§4):

- **Title Registry (`owns`)** — a daemon component (not an LLM) that issues, holds, and
  reclaims **role capability tokens**. A token binds a *title* (`firekeeper@topic-x`,
  `validator@topic-x`) to a *worker session* for the duration of a convening. The registry
  is the "bunch of shell strings": authority lives here at rest. Emits `RoleGranted`.
- **Working agents (`exercise`)** — the convened sessions. They hold a token, do the work,
  emit marks. They **cannot** mint, transfer, or renew their own token (enforced: the
  `board_post`/quorum tools check the token came from the registry, not from a peer). This
  is the sachem who cannot name his successor.
- **Revoker (`revoke`)** — a **separate** daemon function (the "War Chief"), distinct from
  both the registry and every working agent, that executes `RoleRevoked`: it cancels the
  session's in-flight turn (existing `CancellationToken`), pulls the token, and returns the
  title to the registry (escrow), never to a vacuum. Critically, *the decision to revoke*
  and *the execution of revoke* are separated exactly as clan-mother-decides /
  War-Chief-executes: the trigger (quorum of `recall` marks, or a hard policy breach) is
  computed by the quorum engine; the Revoker merely executes when the daemon-checked
  condition is met. No single agent both decides and executes.

The **firekeeper** is a *title*, not a standing chair. It is bound to a working session only
for one convening, holds no power to direct other agents, and exists to do exactly one
thing: emit the single binding `Converged`/`Aborted` mark when the daemon reports quorum
met or timeout fired. It is the elected-dictator-for-termination the theorem requires — one
signed, logged actor owns the *call* — but it has zero authority during exploration. When
the topic closes, the title returns to escrow. The next topic gets a different firekeeper
(or none, if it auto-resolved on quorum without a tie; see §5).

---

## 3. Roles without a chair — titles, not persons

There is **no standing manager**. There are **titles** (durable addressable identities,
Brief 1's "office never dies") that get **bound to worker sessions** for the life of a
convening, then released. A title is just a string in the Title Registry plus its attached
state; the session bound to it is swappable (crash → rebind a successor → inherit the
board projection; the "requickening" skeleton, minus the ceremony).

| Title | Replaces (council) | What it is | Powers | Standing? |
|---|---|---|---|---|
| **Worker** | Member | A convened session with competence tags in the topic's domain. Posts proposals/endorse/inhibit/evidence. | Exercise its own toolset; write marks. **Cannot** convene-close, revoke, or self-renew. | No — bound per convening. |
| **Firekeeper** | Chair (gutted) | A worker session additionally granted the `firekeeper@topic` token *for one topic*. | Emit the single binding `Converged`/`Aborted` mark **only when the daemon says quorum/timeout is satisfied**. No turn-ordering, no curation, no input gatekeeping. | No — one topic, then released. |
| **Validator** | (new; Brief 1 "Listener-Validator" / Hononwiretonh) | A session that takes **no position on content**. Verifies *process*: did required-competence workers actually weigh in? was quorum genuinely met (not faked)? are credentials valid? | Emit a procedural `Veto` (process-only) or a confirm. Cannot vote on the answer. | No. |
| **Title Registry** | — | Daemon component, no LLM. Issues/holds/reclaims tokens. | `RoleGranted`; reclaim on release/revoke. | **Yes — but it's code, not an agent.** Owns authority at rest. |
| **Revoker** | — | Daemon function, no LLM, distinct from registry. Executes recall. | `RoleRevoked`: cancel turn, pull token, return to escrow. | **Yes — code.** |
| **Convener** | (transient) | *Any* session that trips the subsidiarity predicate. Opens a topic + blackboard. Becomes an ordinary Worker immediately after. | `TopicConvened`. No lasting authority. | No — role evaporates on the next event (Contract-Net "manager is per-task"). |

The thing that replaces the chair is **the split itself**: routing is done by *domain* (not
a person), convergence is computed by *the daemon* (not declared by a person), termination
is owned by *a per-topic firekeeper title bound to an ordinary worker* (not a standing
manager), and authority at rest lives in *code* (registry + revoker), which no agent can
capture because no agent *is* it.

Two human mechanics from Brief 1 explicitly **dropped** here:
- **The Condolence/requickening emotional payload** — agents don't grieve; only the state
  handoff survives, and that's just "rebind the title, re-project the board."
- **The three-warning reputation gradient as a trust device** — compressed to a hard policy
  counter (§5): soft strikes accrue and demote; hard breaches revoke instantly. The warnings
  were a human relationship ritual; agents need a threshold, not a relationship.

---

## 4. Event-bus mechanics — riding the existing rails

Everything rides the **existing dual bus**: `OceanEvent`/`EventEnvelope` (control plane,
carries `session_id`/`request_id`) and `AgentTurnEvent` (high-fidelity product stream that
`/v1/agent/events` ships). Longhouse adds **one new event family**, tagged by `session_id`
exactly like every other event, and reuses the `Extension { extension, payload }` variant
that **already exists** in `AgentTurnEvent` as the wire carrier — so **no breaking change to
the SSE contract**. Clients that don't understand Longhouse already ignore unknown
extensions (the SDK contract says so).

### 4.1 New event type — `LonghouseEvent`, carried inside `AgentTurnEvent::Extension`

```rust
// ocean-longhouse crate; serialized as the payload of
// AgentTurnEvent::Extension { extension: "longhouse", payload: <this> }
#[derive(Serialize, Deserialize)]
#[serde(tag = "lh_type", rename_all = "snake_case")]
pub enum LonghouseEvent {
    TopicConvened   { topic_id: Uuid, board_id: SessionId, domain: String,
                      convener: AgentSessionId, reason: EscalationReason, deadline_ms: i64 },
    Convened        { topic_id: Uuid, workers: Vec<AgentSessionId> },   // who got routed in
    MarkPosted      { mark: Mark },                                     // proposal/endorse/inhibit/evidence/note
    QuorumUpdated   { topic_id: Uuid, tallies: Vec<ProposalTally>,      // daemon-computed
                      leader: Option<Uuid>, distance_to_quorum: f32 },
    RoleGranted     { title: String, session_id: AgentSessionId, topic_id: Uuid },
    RoleRevoked     { title: String, session_id: AgentSessionId, reason: RevokeReason },
    Warned          { session_id: AgentSessionId, strike: u8, reason: String },
    Veto            { topic_id: Uuid, by: AgentSessionId, kind: VetoKind, // process | safety
                      justification: Value },                            // §5: machine-checkable or discarded
    Converged       { topic_id: Uuid, decision: Uuid, by: AgentSessionId }, // firekeeper, signed
    Aborted         { topic_id: Uuid, reason: AbortReason },            // timeout/split/recall
    TopicClosed     { topic_id: Uuid, outcome: Outcome },               // titles returned to escrow
}
```

Why piggyback on `Extension` rather than add native `AgentTurnEvent` variants: the SDK
already promises forward-compat on unknown extensions, the monitor/8-bit deck and the
surface keep working untouched, and we avoid editing the canonical product enum until the
shape stabilizes. Promotion to native variants is a later, mechanical migration.

### 4.2 How each phase rides existing primitives

- **Convening** = a `board_post`-class tool side-effect emits `TopicConvened` + creates the
  blackboard session (a normal daemon session, `kind="blackboard"`, via existing session
  create). `Convened` lists the domain-routed workers.
- **Routing a worker in** = **turn-injection, unchanged.** The daemon posts a turn into each
  matched worker's session (`PromptRequest` with that `session_id`, the existing strict-resume
  path) whose prompt **is the bounded board projection** + "post your proposal/endorse/inhibit
  via `board_post`." This is identical to how the council "spoke to a worker," except the
  *daemon* (domain router) injects, not a chair.
- **Quorum / convergence** = the daemon's quorum engine recomputes tallies on every
  `MarkPosted`, emits `QuorumUpdated`. **No agent computes quorum.** When a proposal's net
  weight crosses threshold, the daemon notifies the firekeeper (turn-injection) to emit the
  binding `Converged` — or, in the no-tie fast path, the daemon emits `Converged` itself and
  the firekeeper title is never even bound (most topics auto-resolve).
- **Escalation** = subsidiarity predicate (§2.2), checked in the daemon at the moment a
  tool call would be irreversible/shared/cross-boundary.
- **Veto** = a privileged title (validator, or any token-holder for `safety`) emits `Veto`
  with a **machine-checkable justification**; the daemon **discards a Veto whose
  justification doesn't validate** (Apache rule: a bare veto has no weight). Process vetoes
  block close; safety vetoes block the action.

### 4.3 New daemon endpoints / tools — minimal

`GET /v1/sessions` and `session::list()` **already exist** (and already support workspace
scoping). Reused as-is for roster. **New surface:**

**Endpoints (2):**
```
GET  /v1/longhouse/topics                  -> live topics: [{ topic_id, domain, board_id,
                                              workers, tallies, leader, deadline_ms, state }]
    Read model over the in-memory topic registry. The monitor/command-deck subscribes here.

GET  /v1/longhouse/topics/{id}             -> one topic + its board projection (the same
                                              bounded projection workers receive), for UI.
```
Convening, posting, and closing do **not** need their own endpoints — they happen through
**tools** (below) during normal turns, and their effects already stream on
`/v1/agent/events`. Recall/veto by a *human* operator reuses the existing
`POST /v1/requests/{id}/cancel` (the Revoker is the same cancellation machinery) plus a thin
`POST /v1/longhouse/topics/{id}/recall` if operator-initiated recall is wanted — optional,
flagged as a later add.

**Tools (registered as a `CapabilityProvider`, §7):**
```
convene(topic: string, domain: string, reason: enum) -> { topic_id, board_id }
    Permitted only if the subsidiarity predicate passes (daemon-checked). Opens a topic +
    blackboard. Caller becomes an ordinary worker.

board_post(board_id, kind, target?, body, weight?, ttl_ms?) -> { mark_id }
    Append a typed mark. Thin wrapper over the existing ToolSideEffect emit path. The daemon
    stamps author = caller's session_id and validates the credential; an agent cannot forge
    another's authorship (the session_id is the daemon's, not the LLM's claim).

board_read(board_id) -> { projection }
    Return the bounded projection (top-K live proposals, tallies, evidence). Read-only.
    (Usually unnecessary — workers receive the projection in their injected turn — but
    available for a worker that wants a refresh mid-turn.)

claim_outcome(topic_id, decision_mark_id) -> { ok }
    Firekeeper-only. Emits the binding Converged. The daemon REJECTS the call unless the
    caller holds firekeeper@topic AND the daemon's own quorum state says converged-or-timeout.
    This is the LLM-untrusted gate: the firekeeper can only ratify what the daemon already
    computed, never override it.
```

Four tools, two read endpoints, zero changes to the agent loop or the SSE envelope. That's
the whole new surface.

---

## 5. Termination & anti-deadlock — daemon-enforced, not LLM-trusted

This is the section the council doc got most wrong (it left termination to the chair's
prompt). Every mechanism here is **computed and enforced by the daemon**, because Brief 3 is
unambiguous: a pure consensus-threshold can hang forever (Buridan), and the accountable
terminator must be a single signed actor (elected-dictator theorem). LLMs are not trusted to
count, to break ties, or to stop themselves.

**T1 — Hard deadline on every topic (anti-hang).** `convene` stamps `deadline_ms`. A daemon
timer fires regardless of vote state. On fire, the daemon forces resolution: if a leader is
clear, `Converged` on it; if a true tie, seeded-random tie-break among leaders **or**
escalate to the firekeeper for a forced `Aborted`. **A topic can never block solely on
"reach consensus."** This is the Buridan escape: a timeout that fires independent of the
field.

**T2 — Cross-inhibition with size-scaled weight (anti-symmetric-deadlock).** When workers
back competing proposals, a backer of A emits `inhibit` against B with weight scaled to A's
current tally (Brief 3, Seeley et al. 2012). Quorum counts **net** support = endorsements −
inhibition. Two near-equal proposals actively suppress each other until one breaks
symmetry — the daemon does this arithmetic, not an LLM. Without this, two equally-good plans
dither indefinitely (the single most overlooked primitive in naive voting designs).

**T3 — Decay / give-up (liveness).** Every mark decays (§2.1). An agent that keeps re-asserting
the same proposal with no new evidence sees its signal fade ("expiration of dissent,"
Seeley 2003). No agent can hold the field hostage by shouting. Enthusiasm decays → the system
is biased toward convergence over time.

**T4 — Quorum threshold, not unanimity (speed).** Convergence fires when net weight crosses a
configurable quorum (a steep sigmoid in support count — Couzin's quorum response; sharper =
more decisive, flatter = faster/noisier). Unanimity (every agent a veto) is reserved *only*
for `safety`-class topics (irreversible/destructive), where any validator may block —
safety-over-liveness exactly where Brief 1 says staged unanimity belongs. Cheap topics use a
low quorum and resolve fast.

**T5 — Single signed terminator (accountability).** The binding `Converged`/`Aborted` is
emitted by **one** actor (firekeeper title, or the daemon itself on the fast path) and logged
to one `AgentSessionId`. The elected-dictator theorem says this is the only diffusion-free
shape. The decision log (§6, T7) makes it auditable: replay exactly who closed what.

**T6 — Graduated-then-hard recall (bounded, reversible mandate).** Two paths, both
daemon-enforced:
- *Soft:* a worker producing degraded output / repeated tool errors / ignoring the projection
  accrues `Warned` strikes (a counter, not a relationship). At N strikes, the Revoker
  demotes it (pull token, return to escrow, rebind a successor session to the title).
- *Hard:* a policy breach / unsafe tool call / attempted self-renewal triggers **immediate**
  `RoleRevoked`, no warnings (Brief 1's "crime in office bypasses the gradient"). The
  "lineage forfeiture" maps to quarantining a whole agent **template/role**, not just one
  instance, when the failure is systemic.

**T7 — Token budget ceiling (cost fail-safe).** A per-topic ceiling across all workers +
firekeeper, enforced by the daemon (it already meters usage per turn — `TokenUsage` flows
through `record_prompt_result`). On ceiling, the daemon forces `Aborted` with whatever the
leader is. This is the council doc's budget concern, but enforced in code instead of trusted
to a prompt.

**Anti-deadlock summary:** T1 (timeout) + T2 (cross-inhibition) + T3 (decay) together
guarantee liveness against the three documented hang modes — symmetric ties, stubborn
holdouts, and pure-threshold stalls. T5 + T7 guarantee a topic always terminates with a
signed outcome and a bounded cost. None of it trusts an LLM to do the right thing.

---

## 6. Anti-capture / Sybil resistance — what stops a fake quorum

Brief 3's sharpest warning: a quorum/stigmergic field is **trivially flooded** unless
influence is gated by a scarce credential, and *agents can fork themselves* — the cheapest
Sybil attack in existence. If termination is downstream of a flooded field, capturing
exploration captures termination (the escrow "rubber-stamps a rigged signal"). Defenses,
in order:

**C1 — Quorum counts credentials, not messages (the core fix).** Every `Mark` carries a
`CredentialRef`. The daemon's quorum engine sums **credential-weighted** net support, not raw
mark volume. A chatty agent posting 100 endorsements still counts as **one** credential. This
is the swarm-robotics token-economy idea (Strobel/Dorigo) minus the blockchain: inside one
trusted runtime the daemon *is* the scarce-credential authority — it knows exactly which
sessions are real because it minted them.

**C2 — Credentials are minted only by the daemon, scoped per real spawn.** A worker's
credential is issued by the Title Registry **at convening time**, one per routed session, and
**cannot be cloned by the LLM** — the session_id is the daemon's record, not a value the model
supplies. `spawn_worker` (a capability, §7) mints a fresh credential per child and the
**parent's quorum weight is split, not duplicated** across its children (a duplication-style
attack — fork 50 workers to fake quorum — yields the *same total weight* as one, because the
budget is conserved across the lineage). This directly defeats "spawn a clone army to
manufacture consensus."

**C3 — Self-renewal is structurally impossible.** A worker cannot mint, transfer, or renew its
own token (the grant/exercise/revoke split, §2.3, enforced: `claim_outcome` and the recall
path check the token's issuer is the registry). The sachem cannot name his successor. So an
agent cannot bootstrap itself into more authority.

**C4 — Process validator gates the close (Hononwiretonh).** Before any `Converged` binds, the
validator title verifies — taking **no position on content** — that required-competence workers
actually participated, that quorum was met on **credentialed** weight (not flooded), and that
credentials are registry-issued. A failed check is a procedural `Veto` that blocks the close.
This decouples "is the answer good" from "was the process clean," and catches a captured field
*before* it reaches the terminator.

**C5 — Vote-synchrony anomaly detection (the residual risk).** Brief 3 is honest that
cost-gating stops *cheap duplication* but **not funded/coordinated collusion** — a small clique
of legitimately-credentialed agents synchronizing votes. C1-C4 don't catch this. Mitigation: the
daemon flags suspiciously correlated endorsement timing/content across a credential cluster and
surfaces it as a validator signal (and to the operator). This is detection, not prevention —
documented as a known limit, not a solved problem.

**C6 — Cross-boundary is a different regime (Kaswentha).** Everything above assumes one trusted
runtime where the daemon mints every credential. The moment agents span **operators/orgs**
(different daemons), the legitimacy-manufacturing layer we dropped in §1 **comes back** — you
can no longer trust that a peer's "credential" is scarce. There, Brief 1's two-row wampum
applies: a **non-interference interop contract** — shared mark schema + SLAs, but neither
runtime mutates the other's control plane or issues commands into it; **only marks cross the
boundary**, and cross-boundary marks get a *separate, capped* quorum weight (you never let an
unverifiable external field dominate a local decision). This is the anti-corruption-layer
pattern. **Inside one daemon it is unnecessary** and explicitly not built in v1 — flagged for
federation later.

---

## 7. Riding the CapabilityRegistry — convening as capabilities

The whole Longhouse tool surface plugs into the **shipped** `CapabilityRegistry` seam
(`ocean-runtime::capability`) as **one new provider**, exactly as `ocean-mcp` does. The agent
loop is **not touched** — it already calls `capabilities.tools_for_session(&ctx)` once per
turn, and `SessionContext` already carries `session_id` (the hint a provider needs to decide
whether a session is convened and what titles it holds).

```
ocean-runtime  (capability.rs)         ── the seam (unchanged): CapabilityProvider, SessionContext
      ▲
      │ depends up (never down)
ocean-longhouse  (NEW crate)           ── implements CapabilityProvider ("longhouse")
      │   - convene / board_post / board_read / claim_outcome tools
      │   - Blackboard store + Mark types + decay
      │   - QuorumEngine (credential-weighted net support, cross-inhibition)
      │   - TitleRegistry + Revoker (grant/exercise/revoke principals)
      │   - SubsidiarityPredicate (escalation gate)
      │   - LonghouseEvent (wire payload)
      ▲
      │
ocean-agent    (config.rs)             ── registers LonghouseProvider alongside Builtin + MCP
      ▲
      │
ocean-daemon                           ── 2 read endpoints; routes turn-injection for convening;
                                          owns the topic registry + deadline timers; the Revoker
                                          reuses the existing CancellationToken per request
```

`spawn_worker` is **a capability, not a chair power.** The council doc made it a chair-only
tool; here it's available to any convened worker (subject to the credential-split rule C2) so
sub-convening is recursive (Contract-Net / holonic: a worker can convene a sub-topic, then
revert to worker — authority is contextual, not standing). It mints a child session (existing
session create), records `parent = caller`, applies a role/competence prompt + toolset from the
registry, and **splits the parent's quorum credential** across children. Returns the child
`session_id`. The recall/cancel of a parent cascades to children via the existing
`CancellationToken` (delegation-cascade tear-down, Brief 1's EROS reference).

Why this is clean: caching is the provider's job (registry contract), the provider serves a
per-turn snapshot, built-ins-first dedup means Longhouse tools can never shadow `bash`/`write`,
and a malformed/absent Longhouse config leaves the daemon running built-ins-only (zero
behavior change) — identical to how MCP degrades. Longhouse is **opt-in per workspace** via
`ocean.toml`.

---

## 8. Build order + open decisions

**Build order** (each step independently shippable; nothing breaks if you stop early):

1. **`ocean-longhouse` crate skeleton + `LonghouseEvent` over `Extension`.** Types: `Mark`,
   `MarkKind`, `LonghouseEvent`. Serialize as `AgentTurnEvent::Extension{extension:"longhouse"}`.
   No behavior yet — just the wire vocabulary + a roundtrip test. Risk: near zero.
2. **Blackboard store + `board_post`/`board_read` tools as a `CapabilityProvider`.** Stigmergy
   without quorum: marks land, decay, project. Register the provider in `ocean-agent`. At this
   point agents can share a decaying scratchpad — already useful, already token-safe (decay
   fixes stale-context resend).
3. **Subsidiarity predicate + `convene`.** Wire the escalation gate (reuse `requires_permission`
   + path-overlap + workspace mismatch). Domain routing = turn-inject the projection into
   competence-matched sessions. Now topics open and workers get pulled in by *domain*, no chair.
4. **QuorumEngine (daemon-computed) + `QuorumUpdated` + cross-inhibition + decay tallies.** The
   convergence math, in code. Emit `Converged` on the fast path (no firekeeper needed when no
   tie). This is the heart; most of the correctness work lives here.
5. **Termination guardrails:** deadline timer (T1), token ceiling (T7), seeded tie-break.
   Daemon-enforced. After this, a topic provably always terminates.
6. **Escrow trio:** TitleRegistry + Revoker + firekeeper/validator titles + `claim_outcome`
   gate + graduated/hard recall (T6). Now authority is split three ways and revocation is
   fail-safe.
7. **Sybil hardening:** credential-weighted quorum (C1), credential split on `spawn_worker`
   (C2), self-renewal block (C3), validator process-veto (C4). Capture defenses.
8. **2 read endpoints + monitor/command-deck.** `GET /v1/longhouse/topics[/{id}]`; the 8-bit
   deck subscribes and animates topics/quorum live. Operator recall via existing cancel.
9. *(Later, not v1)* Vote-synchrony anomaly detection (C5); cross-daemon Kaswentha federation
   (C6); promote `LonghouseEvent` from `Extension` to native `AgentTurnEvent` variants.

Steps 1-2 deliver value alone (shared decaying blackboard). 1-5 deliver leaderless,
deadlock-proof convergence. 1-7 deliver the full escrow + anti-capture model. The monitor is
last because it's pure observation.

**Open decisions** (call before building the dependent step):

- **D1 — Firekeeper: always-bound, or only-on-tie?** Leaning **only-on-tie**: the daemon emits
  `Converged` itself on a clear quorum (cheaper, fewer turns), and a firekeeper title is bound
  only when there's a genuine split needing a single signed call. Decide before step 4/6.
- **D2 — Quorum threshold + sigmoid sharpness: global, per-domain, or per-topic?** The single
  speed/accuracy knob (Couzin). Leaning **per-domain defaults, per-topic override**; `safety`
  domain = unanimity. Decide in step 4.
- **D3 — Credential weight model.** Flat-per-session, or weighted by competence-match strength?
  Flat is Sybil-simplest; competence-weighting is smarter but adds a capture surface (game the
  competence tags). Leaning **flat in v1**, competence-weight later behind the validator.
- **D4 — Blackboard substrate.** Dedicated session (gets persistence + bus for free, consistent
  with "everything is a session") vs. a lighter in-memory topic store flushed to a session on
  close. Leaning **dedicated session** for durability/replay; revisit if it's too heavy.
- **D5 — Does a worker ever see the raw bus, or only the bounded projection?** Default **only
  the projection** (bounds context, prevents the mesh blow-up). Opt-in "open floor" raw read
  later behind a flag. (Inherited from the council doc; the answer is firmer now: projection.)
- **D6 — Recall of a *human-convened* topic.** Is operator recall a first-class
  `POST /v1/longhouse/topics/{id}/recall`, or just "cancel the firekeeper's request"? Leaning
  **reuse cancel** in v1; add the explicit endpoint only if the deck needs it.
- **D7 — Persistence of the decision log (T7 audit trail).** Marks decay for *coordination*,
  but the **outcome + who-signed-what** must persist immutably for accountability. Decide where:
  a per-topic JSON alongside the blackboard session, append-only. (The visible-contribution
  ledger from Brief 3 — non-negotiable for the accountability theorem to hold; just deciding
  the file shape.)

---

## Appendix — primitive → research → Ocean mechanic (traceability)

| Longhouse mechanic | Research primitive (brief) | Ocean substrate it rides |
|---|---|---|
| Blackboard of decaying typed marks | Stigmergy / digital pheromones / Hearsay-II (1,2) | new `ocean-longhouse`; `ToolSideEffect` emit; session store |
| Bounded projection (not raw dump) | BB1 control blackboard; load-shedding (1,2) | turn-injection (`PromptRequest`) |
| Subsidiarity escalation predicate | Grand Council narrow jurisdiction (1) | `requires_permission` bit + workspace bucketing |
| Domain routing (no chair) | Contract-Net per-task roles; heterarchy (2) | competence tags from spawn role; turn-injection |
| Credential-weighted quorum | Honeybee quorum-sensing; token economy (2,3) | daemon QuorumEngine; daemon-minted session ids |
| Cross-inhibition (size-scaled) | Stop-signals / cross-inhibition (2,3) | `inhibit` marks; net-support tally |
| Decay / give-up | Pheromone evaporation; expiration of dissent (2,3) | per-mark TTL; daemon recompute |
| Hard deadline + tie-break | Buridan's Principle (3) | daemon timer; seeded RNG |
| Single signed terminator | Elected-dictatorship theorem (3) | firekeeper title; signed `Converged`; decision log |
| Grant/exercise/revoke split | Clan mother / sachem / War Chief; EROS revoker (1) | TitleRegistry + Revoker (daemon) + `CancellationToken` |
| Title persistence, swappable holder | "the office never dies" (1) | title→session binding; rebind on crash/recall |
| Graduated-then-hard recall | Three warnings vs. crime-in-office (1,3) | strike counter + immediate `RoleRevoked` |
| Justified binding veto | Apache `-1` with reason (3) | `Veto` with machine-checkable justification |
| Process validator (no content vote) | Hononwiretonh listener-validator (1) | validator title; procedural `Veto` |
| Credential split on spawn | Sybil cost-gating (3) | `spawn_worker` conserves parent weight |
| Cross-daemon non-interference | Two-row wampum / Kaswentha (1) | *(federation, not v1)* capped external mark weight |

**Dropped as human-only** (named so the next reader doesn't re-import them): the
Condolence/requickening emotional ritual (agents don't grieve — keep only the state handoff);
the legitimacy/kinship compliance layer (the daemon gates actions directly — unnecessary inside
one runtime); the three-warning relationship gradient *as trust-building* (compressed to a hard
counter); standing central authority of any kind (replaced by per-topic titles + daemon-owned
escrow).
