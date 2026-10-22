# Ocean Daemon Longhouse Convene/Title Grant-Bind Extraction Manifest

**Date:** 2026-10-22
**Status:** Characterized and extracted; behavior-neutral mechanical move of the real convene boundary
**Owner:** Ocean OS
**Source baseline:** current `origin/main` `1bd1bc37`
**Rollback points:** characterization commit on `factory/fq-008`; after movement, rollback is one revert of the extraction commit

## Purpose

Extract the daemon's real Longhouse convene flow — ready-model admission, awaited council orchestration over the production provider wire, registry-first event projection, durable firekeeper title grant plus decision bind, and one-time raw-token response delivery — from `crates/ocean-daemon/src/main.rs` into one private binary module, `crates/ocean-daemon/src/longhouse_convene.rs`, without changing behavior, wire contracts, authority, persistence, or security posture.

This is the wave the governance-control manifest deliberately deferred: its stop rule kept real convene in `main.rs` because "deterministic successful real-convene characterization is not available without an unauthorized production seam." That premise is now false in one narrow, production-seam-only way, so the five coupled behaviors can finally move together.

The result remains a private module of the `ocean-daemon` binary. It is not a daemon library, service interface, state subobject, routing authority, or new trust boundary.

## Why the deferral lifted: characterization without a new seam

The previous stop rule held because `ocean_longhouse::convene` resolves its council workers through `ModelHandle::resolve` — the real `ocean_providers` env/auth-file resolution — and drives them over `ocean_protocol::stream_simple`. The keyless `fake-ok` provider cannot staff a council: `ProviderId::Fake` resolves without a credential by construction (`resolve_credential` short-circuits `Ok(None)` for providers that do not require one), and `ModelHandle::ask` drops credential-less workers, so a fake council aborts before any worker answers. The only previously imagined alternatives — exposing ocean-longhouse's private scripted resolver, adding an `AppState` resolver substate, or teaching ocean-protocol a fake wire — are all forbidden new seams.

No seam is needed. The production provider configuration surface already accepts a loopback council:

- `OCEAN_GLM_BASE_URL` — the existing operator override that redirects every catalog GLM model's wire away from `https://open.bigmodel.cn` (exactly the proxy an operator deploys);
- `OCEAN_GLM_API_KEY` — the existing env credential for the GLM provider.

With those two process variables set, the three catalog aliases `glm-5.3`, `glm-5.3-flash`, and `glm-4.5-flash` are `ready` in `known_models_with_readiness`, resolve distinct correlation groups (`glm:<model>`), carry a credential, and stream from any OpenAI-compatible endpoint. The characterization test binds a loopback SSE endpoint serving a scripted council dialogue; the REAL handler then runs its entire production path — ready-model validation, council staffing, the awaited two-round blackboard over the real wire, quorum convergence, projection, grant/bind, and token delivery. The pattern follows `ocean-protocol`'s own tests, which serve hand-written SSE responses to the real `OpenAiProvider` over a loopback `TcpListener`.

Nothing in any product crate changes to make this testable. No trait, substate, test seam, or public API is introduced.

## Exact candidate inventory

Move the following three existing definitions together with their attached comments, exactly as they stand at the baseline:

1. `LonghouseConveneRequest` (request DTO; gains only the `pub(super)` visibility its presence in the handler signature requires; fields stay private);
2. `parse_federation` (stays module-private; only the convene handler calls it);
3. `longhouse_convene` (the handler; becomes `pub(super)`).

The two route mounts in `longhouse_routes()` — `/v1/longhouse/convene` and its canonical-doc alias `/v1/council/convene` — stay in `main.rs` and remain textually identical; the handler name resolves through a `use longhouse_convene::longhouse_convene;` import, the same pattern `longhouse_governance_control` and `longhouse_topics` handlers already use.

## Required owner dependencies

The private owner may import only existing dependencies required by the moved body:

- parent-private `AppState` (`super::AppState`);
- Axum `extract::State` and `Json`;
- `ocean_agent_sdk::{AgentRole, Federation}`;
- `ocean_longhouse::{convene, ConveneRequest, SystemClock}`;
- `ocean_providers::{known_models_with_readiness, ProviderEnv}`;
- `ocean_protocol::now_ms`;
- `serde::Deserialize`, `serde_json::{json, Value}` (the `Value` only in the handler's return type);
- `uuid::Uuid`;
- the existing `tracing` macros the body already uses.

No new crate dependency, handle, wrapper, trait, clock abstraction, registry accessor, test-only production seam, or failure-injection API is permitted.

## Parent-owned authority that must not move

The following remain in `main.rs`:

- `AppState` and every Longhouse/title/revoker/recall field;
- `longhouse_routes()` and the canonical router/middleware composition (both convene mounts included);
- startup construction of the shared `LonghouseRegistry`, the durable `SqliteTitleRegistry`, the daemon-held `Revoker`, and the recall registry;
- `titles_db_path` and all startup DB open/migration;
- the AgentEventBus (`state.agent_events`) and all SSE filtering, replay, framing, lag, keepalive, and shutdown policy;
- the already-published owners: `longhouse_topics.rs`, `longhouse_turn_preparation.rs`, `longhouse_preparation.rs`, `longhouse_governance_control.rs`;
- parent fixtures (`fake_convene_state`, `AUTO_CONVENE_ENV_LOCK`, `TestEnvRestore`) and all extraction-aware characterization tests;
- every `ocean-longhouse` algorithm: council orchestration, quorum/evidence engine, title/escrow cryptography, SQLite schema.

## Existing behavior to preserve

### Provider readiness (audit-grade roster admission)

- Supplied `models` are validated against the live ready-model registry (`known_models_with_readiness` on the process env) before any worker exists; unknown or unready aliases are rejected with the exact `200 { ok: false, error, invalid_models, ready_models }` body (the route answers JSON, not a status error). Silent provider fallback remains prohibited.
- Omitted/empty `models` keeps the `ConveneRequest::new` default roster.

### Awaited council orchestration

- The handler clones the bus, the shared registry, and the durable title registry, then awaits `ocean_longhouse::convene(...)` with the `SystemClock` to completion before answering; the response is not an acknowledgment of a background task.
- Worker resolution failures still drop workers with warnings; an unstaffed council still aborts cleanly through the same event stream.

### Projection before publication

- The emit closure folds each `LonghouseEvent` into the in-memory read-side registry (`registry.lock()` + `ingest`) before publishing `bus.emit(ev.into_turn_event())`; the std Mutex guard is dropped before any await, and lock contention may still block a worker (preserved behavior, not a lock-type change).

### Durable title grant/bind

- On a converged outcome the handler reads the firekeeper from the registry snapshot, grants a `Firekeeper` title into the durable registry at `now_ms`, and binds the engine's exact decision via `bind_decision`; a bind failure logs a warning and still delivers the token; a grant failure maps to non-convergence (`converged: false`, no `title_id`/`token`); only the salted verifier persists — the raw token is never stored or logged.

### Raw-token response delivery

- `title_id` and `token` appear only in the direct HTTP response of a converged council; no bus frame, event, or projection carries either value. The response body key order/shape (`ok`, `question`, `federation` lowercased debug, `streaming_on`, `converged`, `convergence_basis`, then `title_id`/`token`) is frozen by the characterization test and existing tests.

### Preserved risks, not endorsed

- Convene remains unauthenticated like its Longhouse sibling routes (local-route trust posture; CORS is not authentication).
- `converged` reflects grant success, not engine convergence alone: a converged council whose grant fails reports `converged: false` with basis — preserved honestly.
- The std-Mutex registry lock inside the emit closure can block a council worker under contention.
- Grant-success/bind-failure still delivers the token (bind is advisory to the claim path's `NotConverged` refusal).

## Characterization

Parent-owned, in `main.rs` beside the existing convene tests (they need `longhouse_routes()`, `AppState` fixtures, the env lock, and the bus):

- existing `convene_rejects_aliases_missing_from_live_ready_registry` freezes ready-model rejection (unchanged);
- existing `council_convene_is_a_live_alias_of_longhouse_convene` freezes the alias mount and the unstaffed-council abort envelope (unchanged);
- new `convene_converged_council_grants_binds_and_delivers_the_raw_token` freezes the deterministic successful path end to end through the loopback configuration seam: exact response envelope with `converged: true`, valid `title_id`, non-empty `token`, basis string, federation echo; ready-alias staffing of the exact requested model set on the wire with the env credential as the bearer; the converged topic projected into the read-side registry with firekeeper and decision; the durable title present in `SqliteTitleRegistry`; a later engine-free `POST /v1/longhouse/claim` with the delivered token ratifying the bound decision over HTTP; a wrong token refused 403; and the token plus durable title id absent from every published bus frame while the topic id itself remains public bus data.

Five consecutive runs of the new test passed in 0.9–1.7 s each before extraction; the council is deterministic because every provider answer is scripted and the quorum engine is pure.

## Validation matrix

```bash
cargo test -p ocean-daemon convene_
cargo test -p ocean-daemon -- --test-threads=1
cargo clippy -p ocean-daemon --all-targets -- -D warnings
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

The owning-crate gate `cargo test -p ocean-daemon` must pass before push.

## Stop rules

Stop and request a concrete design decision if:

- the moved body would need a trait, clock parameter, resolver injection, `AppState` substate, public API, or any test-only production seam;
- the wire response, route mounts, alias behavior, ready-model rejection envelope, event ordering, or grant/bind/token flow would change in any observable way;
- the loopback characterization proves flaky (then the wave returns to deferred status and this manifest is withdrawn);
- any fresh reviewer reports an unresolved medium-or-higher correctness, security, architecture, or lifecycle issue.

## Rollback

Rollback is one revert of the extraction commit: restore the three definitions to `main.rs`, remove the module/import wiring and the test-marker updates, and retain the characterization test.
