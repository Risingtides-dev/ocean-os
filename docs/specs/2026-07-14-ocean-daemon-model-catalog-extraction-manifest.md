# Ocean Daemon Model Catalog Extraction Manifest

**Date:** 2026-07-14
**Status:** Characterization complete; extraction, full validation, and review pending
**Owner:** Ocean OS
**Rollback point:** `6d13d82`

## Purpose

Characterize and then move the daemon's three model catalog/selection HTTP adapters out of `src/main.rs` into one private binary module. Keep provider routing, credential/readiness resolution, persistence, operational readiness, Longhouse filtering, role/advisor selection, per-turn model overrides, router registration, and all YOLO/settings policy unchanged in composition or their owning crates.

This is the low-risk catalog half of the mission's catalog/settings wave. The security-sensitive YOLO settings half is a separate reviewed checkpoint because its tests share process-global environment locks with voice, permission, and room integration tests.

## Characterization before extraction

Add four focused direct-handler tests in `main.rs` before the mechanical move:

- `model_catalog_get_reports_current_selection`
- `model_catalog_list_preserves_picker_shape_and_readiness_fields`
- `model_catalog_set_reports_success_and_updates_current_selection`
- `model_catalog_set_rejects_invalid_selection_without_mutation`

These tests freeze the exact top-level response keys, current provider/model projection, flat per-model readiness fields, success mutation shape, invalid-selection error shape, and no-mutation-on-error behavior. They remain in parent composition after extraction because they reuse the broad existing `AppState` fixture; this checkpoint does not manufacture a test-only state abstraction merely to relocate them.

## Exact symbols to move intact

After characterization is green, move from `crates/ocean-daemon/src/main.rs` to new `crates/ocean-daemon/src/model_catalog.rs`:

- `ModelSetRequest`
- `model_get`
- `models_list`
- `model_set`

## Files in scope

- `crates/ocean-daemon/src/main.rs`
- new `crates/ocean-daemon/src/model_catalog.rs`
- `crates/ocean-daemon/AGENTS.md`
- `docs/DAEMON_REFACTOR_MISSION.md`
- `docs/specs/2026-07-12-ocean-code-health-and-agent-readiness-plan.md`
- this manifest
- root `events.md`

## Dependencies and visibility

The new module depends only on:

- parent-private `AppState`;
- `axum::{extract::State, Json}`;
- `serde_json::{json, Value}`;
- the existing `ocean_agent::{ProviderEnv, known_models_with_readiness}` facade;
- Tokio `spawn_blocking`;
- tracing through its existing fully-qualified macro call.

`model_get`, `models_list`, and `model_set` are `pub(super)` because the parent router registers them. `ModelSetRequest` is `pub(super)` only because the retained parent characterization tests construct it directly; its field remains parent-visible only as required by that test seam. No symbol becomes public outside the daemon binary.

## Frozen invariants

- `GET /v1/model` returns HTTP 200 with exactly `{ok, provider, model}` from `AgentRuntime::current_model`.
- `GET /v1/models` returns HTTP 200 with exactly `{ok, current, models}`; `current` remains `{provider, model}`.
- Picker entries remain the `ocean_agent::known_models_with_readiness` order and flat serde shape: `id`, `provider`, `label`, `ready`, and optional `credential_source`.
- Process credential/auth-file discovery remains in `ProviderEnv::from_process` and the owner implementation; the daemon does not duplicate provider logic.
- Readiness computation stays in `spawn_blocking`; join failure still degrades to an empty model list via `unwrap_or_default`.
- `POST /v1/model` continues calling only `AgentRuntime::set_model`, preserving canonical alias/provider resolution, credential validation, atomic runtime mutation, and last-model persistence.
- Success remains HTTP 200 `{ok:true, provider, model}` and logs the same `model swapped` fields/message.
- Failure remains HTTP 200 `{ok:false, error}` and leaves the current runtime selection unchanged.
- Routes, method/path parity, middleware, `AppState`, operational `/ready`, turn behavior, and model IDs/order remain unchanged.

## Composition anchors and exclusions

This move does not:

- move or change `/ready`, `/health`, build provenance, fallback-provider reporting, or startup readiness;
- move router registration, discovery/banner entries, or operator-guide entries;
- change `ocean-providers`, `ocean-agent`, known-model ownership, aliases, credential lookup, readiness, or persistence;
- move Longhouse requested-model validation, role/advisor resolution, turn-level overrides, or global/current model reads in turn orchestration;
- touch `yolo_enabled`, `yolo_env_pref`, `effective_yolo`, `resolve_request_yolo`, YOLO settings handlers, permission policy, or their shared environment locks;
- change async boundaries, JSON/status shapes, error/log strings, route behavior, state, or dependencies;
- introduce a daemon library, public API, service trait, substate, generic settings service, generated routing, or opportunistic cleanup.

Any model ID, order, provider, readiness, persistence, response, status, logging, async, or permission change stops this extraction and requires a separate decision.

## Validation

Characterization gate before extraction:

```bash
cargo test -p ocean-daemon model_catalog_ -- --nocapture
cargo test -p ocean-daemon router_contract -- --nocapture
```

Extraction completion gate:

```bash
cargo test -p ocean-daemon model_catalog_ -- --nocapture
cargo test -p ocean-providers
cargo test -p ocean-agent
cargo test -p ocean-daemon router_contract -- --nocapture
cargo test -p ocean-daemon
cargo check --workspace --tests
cargo check -p ocean-daemon --features livekit-tap
cargo check -p ocean-daemon --features deepgram-stt
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

An independent reviewer must compare all four moved production bodies against the characterization commit, verify the retained tests and every composition anchor above, and confirm focused/full/feature gates with no unresolved medium-or-higher issue.

## Characterization result

Four direct-handler tests now freeze the get/list/set response contracts before ownership moves. The focused catalog tests, all five router-contract tests, all 292 daemon tests, formatting, documentation, and diff checks pass at the characterization point.

## Planned result

A private `model_catalog.rs` owns only the model get/list/set HTTP adapters. The parent keeps full-router characterization tests and composition; provider owners keep routing/readiness/persistence authority; YOLO settings remain a separate security-sensitive checkpoint.

## Rollback

Revert the bounded extraction commit after the characterization point. There is no data migration, wire-version handling, or compatibility cleanup.
