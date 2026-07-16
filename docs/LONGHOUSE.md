# Ocean Longhouse

Ocean Longhouse is the **local-first agentic operations hub** for Ocean OS: the place agents go before they act.

Longhouse is not a separate "Hive" product. **Longhouse is the hive.** It is the shared coordination layer that centralizes how agents discover context, select routines, convene peers, and decide what should happen next.

## Responsibility split

Ocean has two load-bearing pieces:

- **`ocean-daemon` = local runtime/body**
  - Owns HTTP/SSE composition and local execution authority.
  - Streams client events over the Ocean HTTP/SSE API.
  - Executes local filesystem, shell, tool, MCP, and destructive actions through `ocean-runtime`.
  - Enforces local permission gates through `ocean-runtime`.
  - Leaves product session/history persistence to `ocean-agent`.

- **`ocean-longhouse` = hive brain / coordination layer**
  - Owns advisory preparation, SOP/workflow coordination, tool/MCP discovery, and quorum/council logic. Local typed memory belongs to `ocean-memory`; shared knowledge belongs to Ocean Bedrock.
  - Coordinates and recommends; it does not bypass daemon permissions.
  - May run bounded council model calls for quorum workflows. General subagent definitions, dispatch, lifecycle, and orchestration belong to extensions, whose local side effects still route through daemon execution authority.

This separation lets Ocean become team-aware without turning a remote service into a root shell on every coworker laptop.

## What Longhouse centralizes

Longhouse is the canonical place for:

1. **SOPs** — company/project operating procedures, guardrails, review rules, escalation policies.
2. **Routines/workflows** — repeatable preflight/planning/check/review sequences.
3. **Tools and MCP discovery** — what tools exist, when to use them, and which daemon can execute them.
4. **Skills** — compact, task-specific capability briefs that can be injected into the main Ocean agent.
5. **Data/memory coordination** — discovers and stages relevant local `ocean-memory` and shared Bedrock knowledge without taking storage ownership.
6. **Advisory preparation/spec assembly** — compact recommendations that extensions may consume; Longhouse does not spawn or manage general subagents.
7. **Quorum/council workflows** — bounded multi-model propose → endorse/inhibit → converge flows with deterministic quorum arithmetic.

## Deployment modes

Longhouse should support the same API shape across four modes:

- **`disabled`** — no Longhouse integration. Ocean behaves with built-ins/MCP only.
- **`embedded`** — daemon exposes Longhouse routes in-process. This exists today for demo/convene routes.
- **`local`** — Longhouse runs as a separate local Rust service, usually:

  ```bash
  cargo run -p ocean-longhouse --bin ocean-longhouse -- serve
  ```

  Default bind target: `127.0.0.1:4781`.

- **`remote`** — local daemons connect to a team Longhouse over HTTPS with an `OCEAN_TOKEN`. The daemon still owns local execution authority.

## Configuration shape

The intended daemon-facing configuration model is:

```text
mode: disabled | embedded | local | remote
url: optional service URL
token_env: environment variable name for bearer token
timeout_ms: request timeout
```

Environment variables:

- `OCEAN_LONGHOUSE_MODE` — one of `disabled`, `embedded`, `local`, `remote`.
- `OCEAN_LONGHOUSE_URL` — service URL, e.g. `http://127.0.0.1:4781`.
- `OCEAN_LONGHOUSE_TOKEN_ENV` — token env var name; defaults to `OCEAN_TOKEN`.
- `OCEAN_TOKEN` — bearer token for remote Longhouse.
- `OCEAN_LONGHOUSE_TIMEOUT_MS` — HTTP timeout budget.

Defaults should be conservative: disabled or embedded-only unless explicitly configured.

## API direction

Existing embedded daemon routes (live in `crates/ocean-daemon/src/main.rs`):

- `POST /v1/longhouse/demo`
- `POST /v1/longhouse/convene`
- `POST /v1/council/convene` — alias of `/v1/longhouse/convene` (same handler).
  "council" is the governance-facing name for the convene/quorum flow; both
  paths dispatch identically so a client can use either name (OCEAN-227).
- `GET /v1/longhouse/topics` — list every topic's observable state. Returns
  `{ "ok": true, "topics": [...] }` from the in-process topic registry.
- `GET /v1/longhouse/topics/{topic_id}` — one topic's full observable state by
  id. Returns `{ "ok": true, "topic": {...} }`; `400` with a typed `{ ok, error }`
  body when `topic_id` is not a valid UUID, `404` when the id is unknown — never
  a panic.
- `POST /v1/longhouse/prepare` — read-only pre-turn skill/SOP preparation.
- `POST /v1/longhouse/inspect` — read-only inspection of that exact preparation
  ranking. It accepts the same brief and `top_n`, uses the same cwd-scoped cached
  indexes, scorer, floor, de-duplication, and tie-breaks, and returns indexed and
  candidate counts plus the selected compact briefs, scores, deterministic
  contributing prompt terms, the `exact_name_phrase` bonus flag, and a
  path-redacted projection of the exact ordinary `prep`. The response reports
  whether the automatic consult is enabled without altering it. It never returns
  the raw prompt, session id, cwd, source paths, or full bodies, and it never runs
  a turn, grants capabilities, or invokes a model.
- `POST /v1/longhouse/claim` — daemon-held `claim_outcome` gate (OCEAN-272).
- `POST /v1/longhouse/board` — `board_post` append note/evidence (OCEAN-272).
- `POST /v1/longhouse/revoke` — hard recall of a persisted title (OCEAN-272).
- `POST /v1/longhouse/recall` — no-confidence vote against a seated firekeeper (OCEAN-272).
- `POST /v1/longhouse/breach` — breach-of-conduct report (OCEAN-272).
- `POST /v1/skills/query` — skill-librarian prefilter (OCEAN-281).
- `POST /v1/skills/fetch` — fetch one skill's full body by id (OCEAN-281).
- `POST /v1/subagents/spec` — compatibility endpoint that assembles an advisory spec from skills + defaults (OCEAN-282); it does not spawn a worker and is pending a separately approved extension migration.
- `POST /v1/workflows/prepare` — read-only workflow-brief preparation step (OCEAN-340).

Local/remote service shape now starts with:

- `GET /health`
- `POST /v1/council/convene` — HTTP wrapper around the existing `convene` flow.
  Already served by the embedded daemon as an alias of `/v1/longhouse/convene`
  (see the embedded route list above); a standalone Longhouse service should
  expose the same path so clients can target either deployment unchanged.

The standalone `ocean-longhouse serve` binary currently exposes only `/health`
and `/v1/council/convene`. Keep the embedded routes stable if that standalone
surface expands; the two deployment modes are not yet API-equivalent.

## First safe integration slice

The smallest useful dynamic integration is a **read-only preparation step**:

1. Daemon receives a user turn.
2. If Longhouse is enabled, daemon sends a compact task/session brief to Longhouse.
3. Longhouse returns compact active briefs:
   - relevant skills,
   - SOP reminders,
   - workflow/routine suggestions,
   - optional extension/council recommendation.
4. Daemon injects those briefs into the main agent prompt/context.
5. Main agent still uses daemon tools and permission gates for all local execution.

This creates value without allowing Longhouse to perform local side effects.

## Skill Librarian

Longhouse indexes and queries (implemented — `ocean-longhouse/src/prepare.rs`):

- `~/.config/ocean-rs/skills/**` — Ocean's native user skill pack library
  (either format below; `OCEAN_SKILLS_DIR` overrides the location)
- `~/.spawner/skills/**/skill.yaml`
- `~/.codex/skills/**/SKILL.md`
- repo-local `./skills/**`

Selection path:

1. Deterministically index compact skill/workflow names and descriptions; bodies
   do not participate in relevance.
2. Normalize the prompt into distinct alphanumeric terms. Match only exact
   metadata-token boundaries (never substrings), retain the closed short-domain
   allowlist `ai`, `ci`, `db`, `os`, `pr`, `qa`, `ui`, and `ux`, and drop common
   request boilerplate.
3. Weight name hits above description hits, reward distinct coverage, add one
   explicit bonus when the complete retained multiword name occurs in prompt
   order, then apply the existing relevance floor and stable score/name/path
   tie-break. Exact matching intentionally does not stem `deploy` into
   `deployment` or singulars into plurals.
4. Return up to `top_n` compact briefs (default: five); daemon injects them into
   the main Ocean agent under the existing advisory framing.

Model-based reranking is future work; the shipped preparation path makes no LLM call.
The explained selection can be inspected through `POST /v1/longhouse/inspect`;
inspection is a projection of this same scorer rather than a second debug ranker.

## Extension-owned subagent boundary

General subagents are extension-owned. Extensions own role definitions, objectives, prompts, model/tool policy, skill selection, budgets, spawn/join lifecycle, and orchestration. Here an extension is a separately shipped/runtime-loaded capability provider over Ocean's plugin, MCP, subprocess, or future WASM seams—not another named module compiled into the daemon. Core crates must not add a daemon-native `task`, `spawn_worker`, fleet scheduler, or named-subagent runtime.

Core remains responsible only for generic permission-gated turns, cancellation, capability-provider registration, and extension event/tool transport. The existing `POST /v1/subagents/spec` assembler is read-only compatibility behavior: it returns an advisory description and never spawns a worker. Moving or removing that route and the folder-agent `subagents` metadata requires a separate migration; neither is precedent for new core orchestration.

## Current implementation notes

`crates/ocean-longhouse` is a Rust crate. It already contains:

- deterministic quorum logic in `quorum.rs`,
- cheap-model single-turn workers using the existing Ocean provider stack in `agent.rs`,
- real council flow in `convene.rs`,
- replay/tuning support in `replay.rs` and `lh-tune`.

`ocean-daemon` mounts the full `longhouse_routes` group and registers the
permission-gated `LonghouseProvider`. The separate `ocean-longhouse serve`
binary already runs a minimal local service on `127.0.0.1:4781` by default.

### Council model-selection behavior

For embedded HTTP `POST /v1/longhouse/convene`, a non-empty explicit `models`
array must contain ready IDs from the daemon's live `GET /v1/models` registry.
The handler rejects the whole explicit roster with `invalid_models` when any ID
is not ready; it does not silently substitute a model.

That rejection rule is not universal. An omitted/empty HTTP roster uses
`ConveneRequest` defaults. The `longhouse__convene` capability tool and the
standalone service also pass aliases directly into `convene`. In those paths,
aliases that fail resolution or lack credentials are warned and skipped, so a
partial council can run. See `LONGHOUSE_ORCHESTRATION.md` for the exact entry-path
boundary.

## Built vs unbuilt — current quorum and governance status

The `QuorumEngine` and real council path are shipped and unit-tested:
per-worker-deduplicated endorse−inhibit tallies, time-decay, cross-inhibition,
configurable threshold, margin-gated convergence, and seeded tie-break. Shipped
execution bounds are the council deadline, maximum deliberation rounds, a
45-second timeout per model call, and a 512-token output cap per model call.
There is no aggregate per-topic token accounting or token ceiling yet.
`convene.rs` staffs a real mixed-model council, grants a firekeeper to the
winning proposer, and emits `Converged`/`Aborted`.

Later governance work is mixed. Use the explicit **BUILT** and unbuilt labels
below:

- **Unforgeable `claim_outcome` gate.** BUILT (OCEAN-229). When
  `convene` seats the firekeeper on the winning proposer it mints a
  [`FirekeeperTitle`](../crates/ocean-longhouse/src/convene.rs): the public
  `agent_id` paired with a **secret token** drawn server-side from the OCEAN-185
  decision-token primitive (`mint_decision_token`, ~244 bits of OS-CSPRNG
  entropy). The token is the proof-of-title; it never appears on any emitted
  `LonghouseEvent` (`RoleGranted`/`Converged` carry only the `agent_id`).
  `claim_outcome` now requires the claimant to present a matching `(agent_id,
  token)` pair and verifies the token in **constant time**
  (`decision_token_matches`) *before* consulting the engine — so a forged
  firekeeper that only learned the public id off the event stream is rejected
  with `ClaimError::ForgedFirekeeper` even when the quorum genuinely converged.
  The original accountability brake still holds on top: a legitimately-titled
  firekeeper may only ratify the engine's own decision (`NotConverged` /
  `WrongDecision` otherwise). This is the same trust-boundary discipline as
  OCEAN-185 (public id, secret token) and OCEAN-220 (the right is server-decided
  and minted, not claimant-asserted).
- **Escrow trio.** BUILT (OCEAN-246), in
  [`escrow.rs`](../crates/ocean-longhouse/src/escrow.rs). The
  grant/exercise/revoke principals now exist as durable, daemon-held code:
  - **`SqliteTitleRegistry` — authority at rest.** A `rusqlite`-backed store
    (the same bundled-SQLite pattern as `ocean-store`'s `SqliteRoomStore`) that
    issues/holds/reclaims titles. Crucially the title now **survives across
    turns**: a title minted when a council converges is looked up and verified in
    a *later* turn, which is exactly what lets `claim_outcome` become a
    daemon-held op (the limitation `longhouse_provider.rs` flagged). Persistence
    is **secret-free**: the registry stores a *verifier* — a per-title random salt
    plus `SHA-256(salt || token)` — and **never the raw token**, so a full DB dump
    cannot recover the token or forge a claim (password-storage discipline).
  - **`Revoker` — the War Chief who executes deposition.** A separate principal
    (distinct from the registry and from every worker) that runs graduated recall:
    `warn()` accrues `Warned` strikes, `revoke()` hard-pulls the title
    (`RoleRevoked`). A revoked title fails `claim_outcome_persisted` **even with
    the correct token**. Revocation is itself unforgeable — the Revoker holds a
    server-minted capability key, so a forged recall (no/wrong key) is refused;
    *decide ≠ execute* is preserved (the trigger is daemon-computed; the Revoker
    only executes).
  - **Validator escrow.** An escrow ledger where validators stake a bond per
    topic; the bond is **held**, **released** on a successful firekeeper claim
    (`claim_outcome_persisted` releases the topic's escrow), or **forfeited** on
    abort/veto.
  - **`claim_outcome_persisted`** is the daemon-held analog of the in-frame gate:
    it verifies the persisted title (constant-time, rejecting revoked/released
    titles) **then** the engine's agreement, then releases escrow.

  **Scope-noted follow-ups (deliberately not in OCEAN-246):** (1) *daemon wiring*
  — completed in OCEAN-272 (commit 668aa70): `SqliteTitleRegistry` is wired
  through the `AppState::titles` field, while `longhouse_routes`,
  `longhouse_claim`, `longhouse_board_post`, and the rest of the governance
  route set are live embedded daemon handlers (see the route list above).
  (2) *staking economics* —
  the escrow data structure + release/forfeit hooks are built, but where bond
  amounts come from, slashing curves, and sybil-cost calibration are policy and
  remain future work.
- **Validator process-veto** and the **subsidiarity escalation predicate** (most
  things should never convene a council at all) — still stubbed.
- **Sybil hardening.** Credential conservation for extension-owned workers,
  generalized self-renewal prevention, and validator veto are not built.
  Longhouse does not own worker spawning.

The convergence engine, the unforgeable `claim_outcome` gate (OCEAN-229), and
the persisted escrow trio (OCEAN-246) are real code. The remaining anti-capture
predicates, staking economics, and aggregate per-topic token accounting are
follow-ups; the embedded daemon wiring described above is live.
