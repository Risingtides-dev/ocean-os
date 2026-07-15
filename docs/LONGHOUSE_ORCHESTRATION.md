# Longhouse orchestration boundaries

**Status:** Current implementation reference plus explicitly labeled target boundaries

**Updated:** 2026-07-14

**Primary code:** `crates/ocean-longhouse`, `crates/ocean-daemon/src/main.rs`

## Purpose

Longhouse provides bounded multi-model council deliberation, observable topic state,
advisory preparation, and persisted governance titles. It does not own general
subagent spawning. General worker definitions, prompts, model/tool policy, budgets,
spawn/join lifecycle, and result orchestration belong to extensions.

Source owns all wire shapes and route behavior. This document names stable symbols
rather than copying complete Rust enums or the daemon's executable route inventory.
Use [`LONGHOUSE.md`](LONGHOUSE.md) for the subsystem overview and the runtime operator
guide for the current HTTP quick reference.

## Authority boundary

| Layer | Shipped responsibility |
| --- | --- |
| `ocean-longhouse` | Council model calls, quorum arithmetic, in-memory topic projection, replay/tuning, advisory preparation, title/escrow primitives, and two permission-gated capability tools |
| `ocean-daemon` | HTTP composition, event-bus publication, shared Longhouse registry, persisted title storage, board/claim/revoke/recall/breach handlers, and model-readiness validation on the explicit HTTP convene roster |
| `ocean-runtime` | Generic capability execution, permission gates, cancellation, and tool events |
| Extensions | General subagent roles, prompts, worker policy, dispatch, budgets, lifecycle, joins, and orchestration |

Longhouse council calls never bypass runtime permissions for local side effects.
The council workers themselves are bounded provider calls and receive no local toolset.

## Shipped council entry paths

### Embedded HTTP convene

`ocean-daemon::longhouse_convene` handles both `POST /v1/longhouse/convene`
and its `/v1/council/convene` alias.

- A non-empty explicit `models` array is checked against ready IDs from the daemon's
  live model registry. Any invalid ID rejects the whole request with
  `invalid_models`; the handler does not silently substitute a model.
- An omitted or empty roster uses `ConveneRequest` defaults. Resolution then occurs
  inside `ocean_longhouse::convene`, where aliases that fail resolution or lack a
  credential are warned and skipped.
- Each emitted `LonghouseEvent` is folded into the shared `LonghouseRegistry` before
  being published on the daemon event bus.
- The HTTP path uses `LonghouseEvent::into_turn_event()`, which produces an
  `AgentTurnEvent::Extension` with `scope: None`. These council-wide events are not
  implicitly attributed to an `AgentSessionId`.
- On convergence, the daemon grants a separate persisted firekeeper title, binds it
  to the decision, and returns `title_id` plus the raw claim token only in the direct
  HTTP response. The token is not stored raw and is never emitted on SSE.

### Capability-provider convene

`LonghouseProvider` exposes exactly two namespaced tools:

- `longhouse__convene`
- `longhouse__board_read`

Both report `requires_permission() == true`. `longhouse__convene` accepts model aliases
directly; `convene` skips aliases that cannot resolve or lack credentials, so this path
may run a partial council. It folds events into the shared in-memory registry but does
not publish them to the daemon event bus. Its result includes topic, proposal, tally,
and decision data; it does not grant or return a persisted firekeeper title/token.

`board_post`, `claim_outcome`, revocation, recall, and breach are daemon HTTP handlers,
not `LonghouseProvider` tools.

### Standalone service

The `ocean-longhouse` binary already supports:

```bash
cargo run -p ocean-longhouse --bin ocean-longhouse -- serve \
  --bind 127.0.0.1:4781
```

It exposes `/health` and `POST /v1/council/convene`. Its council route accepts aliases
directly and returns collected events in the response; it does not reproduce the
embedded daemon's full governance, registry, SSE, or readiness-validation surface.

## Identity and state

### Council workers are not Ocean sessions

For every model alias that resolves with a credential, `convene` creates a fresh UUID
for that model worker. That UUID appears as `LonghouseMember.agent_id` and mark author.
It is not an `AgentSessionId`, does not identify a tool-using Ocean session, and is not
a registry-issued scarce credential.

`QuorumEngine` keeps at most one live stance per worker UUID across the field. This
deduplicates repeated messages from the same worker ID, but it does not prevent an
extension or caller from introducing additional identities. Conserved child credentials
remain future work.

### Topic projection is in memory

`LonghouseRegistry` is a daemon-held in-memory map that folds `LonghouseEvent`s into
`TopicSnapshot`s. It targets a 256-topic bound by evicting the oldest closed
snapshots when over capacity, but never evicts a live topic; more than 256
simultaneously live topics can therefore exceed the target. It survives client
refresh while the daemon process remains alive and is not persisted across restart.

Persisted governance titles are separate. `SqliteTitleRegistry` stores title state and
salted token verifiers so later HTTP claim/revoke operations can survive turns without
storing the raw secret.

### Source-owned event and mark shapes

Current wire shapes live in `ocean-agent-sdk`:

- `Mark`
- `LonghouseMember`
- `LonghouseEvent`
- `LonghouseEvent::into_turn_event`
- `LonghouseEvent::into_turn_event_scoped`

Do not infer fields such as `SessionId`, `AgentSessionId`, `CredentialRef`, or `Veto`
from historical design sketches; they are not fields/variants in the current Longhouse
wire types.

## Deliberation and termination bounds

`ocean_longhouse::convene` runs one proposal round followed by bounded
endorse/inhibit rounds.

- Later-round prompts receive a stable numbered list of proposal texts, with each text
  truncated to 220 characters. They do not receive a raw transcript or a complete
  evidence/quorum-distance projection.
- Endorse and inhibit calls currently use unit weight. Cross-inhibition subtracts that
  unit stance from a proposal's net support; it is not size-scaled.
- A worker's newer stance replaces its prior stance and refreshes its timestamp.
  Unreasserted stances decay; repeatedly reasserted stances do not fade automatically.
- `max_rounds` bounds the number of deliberation rounds.
- Each model call has a 45-second timeout and a 512-token output cap.
- `deadline_ms` is checked before later rounds and while returned votes are consumed.
  It is not an independent timer and does not preempt a model call already in progress.
- After the loop, pending quorum is force-resolved using the configured deterministic
  tie behavior or emits an abort when resolution fails.

There is no aggregate per-topic token accounting or token-cost ceiling. The shipped
controls bound round count, individual call duration/output, and the council control
flow; they do not prove a hard wall-clock deadline or aggregate spend limit.

## Governance and cancellation

The shipped title boundary includes server-minted firekeeper proofs, persisted title
verification, warnings/strikes, manual revoke, recall voting, breach-triggered warning or
revocation, and later decision claim.

Revocation changes persisted title state. It does **not** cancel an in-flight request,
turn, session, or model call. Runtime request cancellation is a separate mechanism.
The shipped path also does not rebind a successor session, quarantine an agent template,
or transfer a board projection after a crash.

Validator process veto, safety unanimity, and machine-validated `Veto` events are not
implemented.

## Target work that must remain explicit

The following are target concepts, not current behavior:

- extension-owned worker dispatch, joins, and cancellation cascades;
- domain/competence routing of existing sessions;
- a daemon-checked subsidiarity predicate over shared, irreversible, or
  cross-boundary work;
- conserved parent/child quorum credentials;
- validator/process-veto and safety-unanimity gates;
- aggregate per-topic token accounting;
- successor-session rebinding and template-level quarantine;
- vote-synchrony detection;
- cross-daemon/federated quorum weighting;
- coupling title recall to request/turn cancellation.

Any of these requires its own approved design, source changes, tests, and migration.
None should be inferred from the existing `/v1/subagents/spec` compatibility assembler
or folder-agent `subagents` metadata.

## Verification

For documentation-only corrections:

```bash
cargo xtask docs-check
git diff --check
```

For Longhouse behavior changes:

```bash
cargo test -p ocean-longhouse
cargo test -p ocean-daemon
cargo check --workspace --tests
```

Stable source anchors:

- `crates/ocean-longhouse/src/agent.rs`
- `crates/ocean-longhouse/src/convene.rs`
- `crates/ocean-longhouse/src/quorum.rs`
- `crates/ocean-longhouse/src/registry.rs`
- `crates/ocean-longhouse/src/longhouse_provider.rs`
- `crates/ocean-longhouse/src/escrow.rs`
- `crates/ocean-agent-sdk/src/lib.rs`
- `crates/ocean-daemon/src/main.rs::{longhouse_routes,longhouse_convene,longhouse_claim,longhouse_revoke,longhouse_recall,longhouse_breach,longhouse_board_post}`
