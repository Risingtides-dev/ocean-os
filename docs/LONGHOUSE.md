# Ocean Longhouse

Ocean Longhouse is the **local-first agentic operations hub** for Ocean OS: the place agents go before they act.

Longhouse is not a separate "Hive" product. **Longhouse is the hive.** It is the shared coordination layer that centralizes how agents discover context, select routines, convene peers, and decide what should happen next.

## Responsibility split

Ocean has two load-bearing pieces:

- **`ocean-daemon` = local runtime/body**
  - Owns sessions and transcript persistence.
  - Streams client events over the Ocean HTTP/SSE API.
  - Executes local filesystem, shell, tool, MCP, and destructive actions.
  - Enforces local permission gates.
  - Remains the authority for actions on a user machine.

- **`ocean-longhouse` = hive brain / coordination layer**
  - Owns skills, SOPs, routines, workflows, tool/MCP discovery, shared memory/knowledge, subagent specs, and quorum/council logic.
  - Coordinates and recommends; it does not bypass daemon permissions.
  - May host model subagents and council workflows, but local file edits/bash/destructive actions must route back through the daemon execution authority.

This separation lets Ocean become team-aware without turning a remote service into a root shell on every coworker laptop.

## What Longhouse centralizes

Longhouse is the canonical place for:

1. **SOPs** — company/project operating procedures, guardrails, review rules, escalation policies.
2. **Routines/workflows** — repeatable preflight/planning/check/review sequences.
3. **Tools and MCP discovery** — what tools exist, when to use them, and which daemon can execute them.
4. **Skills** — compact, task-specific capability briefs that can be injected into the main Ocean agent.
5. **Data/memory layers** — company memory, project memory, per-agent notes, run records, and knowledge retrieval.
6. **Subagent specs/runtimes** — role, objective, model policy, allowed tools, memory namespace, output schema, max turns, and budget.
7. **Quorum/council workflows** — multi-agent propose → endorse/inhibit → converge flows with deterministic quorum arithmetic.

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
- `POST /v1/longhouse/claim` — daemon-held `claim_outcome` gate (OCEAN-272).
- `POST /v1/longhouse/board` — `board_post` append note/evidence (OCEAN-272).
- `POST /v1/longhouse/revoke` — hard recall of a persisted title (OCEAN-272).
- `POST /v1/longhouse/recall` — no-confidence vote against a seated firekeeper (OCEAN-272).
- `POST /v1/longhouse/breach` — breach-of-conduct report (OCEAN-272).
- `POST /v1/skills/query` — skill-librarian prefilter (OCEAN-281).
- `POST /v1/skills/fetch` — fetch one skill's full body by id (OCEAN-281).
- `POST /v1/subagents/spec` — assemble a subagent spec from skills + defaults (OCEAN-282).
- `POST /v1/workflows/prepare` — read-only workflow-brief preparation step (OCEAN-340).

Local/remote service shape now starts with:

- `GET /health`
- `POST /v1/council/convene` — HTTP wrapper around the existing `convene` flow.
  Already served by the embedded daemon as an alias of `/v1/longhouse/convene`
  (see the embedded route list above); a standalone Longhouse service should
  expose the same path so clients can target either deployment unchanged.

Keep the daemon-side embedded routes working while adding the standalone service; clients and daemons can bridge versions during migration.

## First safe integration slice

The smallest useful dynamic integration is a **read-only preparation step**:

1. Daemon receives a user turn.
2. If Longhouse is enabled, daemon sends a compact task/session brief to Longhouse.
3. Longhouse returns compact active briefs:
   - relevant skills,
   - SOP reminders,
   - workflow/routine suggestions,
   - optional subagent/council recommendation.
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

1. Deterministic prefilter from task/session brief.
2. Cheap fast model reranker.
3. Return 3–7 compact skill briefs.
4. Daemon injects those briefs into the main Ocean agent.

## Subagent future

Longhouse should assemble subagent specs from skills + routines + token scopes + memory + model/tool policy.

A subagent spec should include:

- role,
- objective,
- model policy,
- skill ids,
- allowed tools,
- memory namespace,
- output schema,
- max turns,
- budget.

Subagents can be orchestrated by Longhouse, but local side effects still return through daemon permission gates.

## Current implementation notes

`crates/ocean-longhouse` is a Rust crate. It already contains:

- deterministic quorum logic in `quorum.rs`,
- cheap-model single-turn workers using the existing Ocean provider stack in `agent.rs`,
- real council flow in `convene.rs`,
- replay/tuning support in `replay.rs` and `lh-tune`.

`ocean-daemon` currently embeds Longhouse routes for demo and convene flows. The next step is to let `ocean-longhouse` also run as a local service on `127.0.0.1:4781`, then teach the daemon to consult it dynamically in read-only preparation mode.

## Built vs unbuilt — quorum steps 1–5 are real, steps 6+ are not

Per the build order in `docs/LONGHOUSE_ORCHESTRATION.md` § 8, the `QuorumEngine`
is shipped through **step 5** and unit-tested (11 passing tests): credential-weighted
endorse−inhibit tallies, time-decay, cross-inhibition, configurable threshold,
margin-gated convergence, seeded tie-break, and the termination guardrails
(deadline timer, token ceiling) that make a topic provably terminate. `convene.rs`
staffs a real mixed-model council, grants a firekeeper to the winning proposer,
and emits `Converged`/`Aborted`.

**Steps 6 and beyond are future / unbuilt.** Treat the following as not-yet-real:

- **Unforgeable `claim_outcome` gate (step 6).** BUILT (OCEAN-229). When
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
- **Escrow trio (step 6).** BUILT (OCEAN-246), in
  [`escrow.rs`](../crates/ocean-longhouse/src/escrow.rs). The three principals of
  §2.3 now exist as durable, daemon-held code:
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
  — completed in OCEAN-272 (commit 668aa70): `SqliteTitleRegistry` is now wired
  into `AppState` (main.rs line 98, 448, 1498–1510), and `claim_outcome`/`board_post`
  and the full governance route set are live embedded daemon routes (see the
  embedded route list above). (2) *staking economics* —
  the escrow data structure + release/forfeit hooks are built, but where bond
  amounts come from, slashing curves, and sybil-cost calibration are policy and
  remain future work.
- **Validator process-veto** and the **subsidiarity escalation predicate** (most
  things should never convene a council at all) — still stubbed.
- **Sybil hardening (step 7).** Credential-split on `spawn_worker`,
  self-renewal block, validator veto — not built.

The convergence engine, the unforgeable `claim_outcome` gate (OCEAN-229), and now
the persisted escrow trio (OCEAN-246) are real code. The remaining anti-capture
predicates and the daemon wiring are the documented follow-ups.
