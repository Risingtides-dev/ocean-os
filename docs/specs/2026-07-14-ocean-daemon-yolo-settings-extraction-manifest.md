# Ocean Daemon YOLO Settings Extraction Manifest

**Date:** 2026-07-14
**Status:** Complete; characterization, security-doc clarification, focused/full/feature validation, and independent review passed
**Owner:** Ocean OS
**Rollback point:** `529e0ed`

## Purpose

Characterize and then move the daemon's security-sensitive YOLO preference/effective-policy helpers and GET/POST settings adapters into one private binary module without changing permission authority. Keep router registration, per-turn call sites, permission decision-token binding, voice fail-fast behavior, and the single shared process-environment test lock in parent composition.

This is the settings half of the catalog/settings mission wave. Model catalog adapters were completed separately so permission posture has its own focused review and rollback.

## Security clarification before extraction

Correct the stale `effective_yolo` documentation that still claims a request wire flag can opt into YOLO. Current and required behavior is the opposite: `resolve_request_yolo` deliberately discards `PromptRequest.yolo`, and all effective posture comes from recognized operator env → persisted preference → safe default off. This documentation-only correction accompanies characterization, not the mechanical move.

## Characterization before extraction

Add two direct-handler tests in `main.rs` before ownership moves:

- `yolo_settings_get_reports_persisted_effective_and_env_override`
- `yolo_settings_set_persists_before_resolving_effective_and_reports_mask`

They freeze exact GET/POST keys and values, nullable env override, persistence-before-effective resolution, and an explicit env-off mask over persisted true. They acquire the existing `YOLO_ENV_LOCK` then `AUTO_CONVENE_ENV_LOCK`, matching the established global lock order, and remain in parent composition after extraction.

Existing parent tests continue to pin:

- `ocean_yolo_env_defaults_off_and_opts_in_explicitly`
- `yolo_pref_persists_and_roundtrips`
- `effective_yolo_precedence_env_over_persisted_over_off`
- `resolve_request_yolo_ignores_wire_flag`
- voice token/yolo truth table and live fail-fast behavior
- permission gating default and explicit YOLO bypass

## Exact symbols to move intact

After characterization is green, move from `crates/ocean-daemon/src/main.rs` to new `crates/ocean-daemon/src/yolo_settings.rs`:

- test-only `yolo_enabled`
- `yolo_env_pref`
- `effective_yolo`
- `resolve_request_yolo`
- `YoloSetRequest`
- `yolo_setting_get`
- `yolo_setting_set`

## Files in scope

- `crates/ocean-daemon/src/main.rs`
- new `crates/ocean-daemon/src/yolo_settings.rs`
- `crates/ocean-daemon/AGENTS.md`
- `docs/DAEMON_REFACTOR_MISSION.md`
- `docs/specs/2026-07-12-ocean-code-health-and-agent-readiness-plan.md`
- this manifest
- root `events.md`

## Dependencies and visibility

The new module depends only on:

- `std::env`;
- `axum::Json`;
- `serde_json::json`;
- existing `ocean_agent::{config_dir_from_env, load_yolo_pref, persist_yolo_pref}` paths;
- tracing through the existing fully-qualified macro.

- `effective_yolo`, `resolve_request_yolo`, `yolo_setting_get`, and `yolo_setting_set` are `pub(super)` for unchanged parent router/orchestration callers.
- `YoloSetRequest` and its field are `pub(super)` only for retained parent handler characterization.
- test-only `yolo_enabled` is `pub(super)` under `cfg(test)` for the retained parsing test.
- `yolo_env_pref` remains private because both settings handlers move with it.

The process-global `YOLO_ENV_LOCK`, sync/async guard helpers, `AUTO_CONVENE_ENV_LOCK`, and all parent integration tests stay in `main.rs`; no lock is duplicated or reordered.

## Frozen invariants

- Recognized env-on spellings remain trimmed/case-insensitive `1`, `true`, `yes`, and `on`.
- Recognized env-off spellings remain trimmed/case-insensitive `0`, `false`, `no`, and `off`.
- Unset or unrecognized env falls through to the persisted preference.
- Precedence remains recognized env → persisted preference → false; false is the safety default.
- Explicit env false overrides persisted true; explicit env true overrides persisted false.
- `PromptRequest.yolo` remains inert and cannot escalate permission posture.
- `GET /v1/settings/yolo` remains HTTP 200 with exactly `{ok, persisted, effective, env_override}`; absent persisted/env values serialize as null.
- `POST /v1/settings/yolo` persists the submitted bool before recomputing effective posture, logs the same fields/message, and returns the same exact keys.
- Persisted preference path/format and restart behavior remain owned by `ocean-agent`.
- Product turns, legacy prompt/request handlers, room/Longhouse turns, calls, and voice use the same effective posture at the same points.
- Permission gates and decision-token binding remain authoritative and orthogonal.
- Shared environment-lock order remains YOLO then auto-convene; no duplicate mutex is introduced.

## Composition anchors and exclusions

This move does not:

- move or change router registrations, method/path discovery, handlers outside YOLO settings, or middleware;
- move per-turn call sites, `build_prompt_control`, permission policy, waiters, decision endpoints/tokens, or voice fail-fast logic;
- trust or remove the legacy wire `yolo` field;
- move tests or shared environment locks into the leaf;
- change truthy/falsy grammar, precedence, config path, persistence format, JSON/status shape, log text, or async behavior;
- touch model catalog/routing/readiness, Longhouse prep enablement, AppState, sessions, cwd, routes, or SSE;
- introduce a daemon library, public API, dependency, settings service, trait, substate, or opportunistic cleanup.

Any permission, precedence, wire, persistence, response, logging, lock, or caller change stops this extraction and requires a separate decision.

## Validation

Characterization gate:

```bash
cargo test -p ocean-daemon yolo_settings_ -- --nocapture
cargo test -p ocean-daemon effective_yolo_precedence_env_over_persisted_over_off -- --nocapture
cargo test -p ocean-daemon resolve_request_yolo_ignores_wire_flag -- --nocapture
cargo test -p ocean-daemon voice_turn_ -- --nocapture
cargo test -p ocean-daemon router_contract -- --nocapture
```

Extraction completion gate:

```bash
cargo test -p ocean-daemon yolo_settings_ -- --nocapture
cargo test -p ocean-daemon effective_yolo_precedence_env_over_persisted_over_off -- --nocapture
cargo test -p ocean-daemon resolve_request_yolo_ignores_wire_flag -- --nocapture
cargo test -p ocean-daemon voice_turn_ -- --nocapture
cargo test -p ocean-agent
cargo test -p ocean-runtime
cargo test -p ocean-daemon router_contract -- --nocapture
cargo test -p ocean-daemon
cargo check --workspace --tests
cargo check -p ocean-daemon --features livekit-tap
cargo check -p ocean-daemon --features deepgram-stt
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

An independent security-focused reviewer must compare all seven moved definitions against the characterization commit, verify every composition/lock anchor above, and confirm no unresolved medium-or-higher issue.

## Characterization result

Two direct-handler tests now freeze exact GET/POST response values, env masking, nullable override fields, and persistence-before-effective resolution. The security documentation now matches the existing inert-wire behavior. Focused settings/precedence/wire/voice tests, all five router contracts, all 294 daemon tests, formatting, documentation, and diff checks pass at the characterization point.

## Result

A private `yolo_settings.rs` now owns only env/persisted effective-policy resolution and the GET/POST settings adapters. All seven moved definitions are unchanged from characterization commit `529e0ed` except for minimal `pub(super)` visibility. Parent composition retains every caller, permission and decision-token boundary, voice fail-fast path, router registration, both shared environment locks, and all characterization/integration tests. Focused settings, precedence, inert-wire, voice, agent, runtime, router, daemon, workspace-test compilation, both supported-feature, formatting, documentation, and diff checks passed. Fresh security-focused review found no unresolved medium-or-higher issue.

## Rollback

Revert the bounded extraction commit after the characterization point. There is no data migration, wire-version handling, or compatibility cleanup.
