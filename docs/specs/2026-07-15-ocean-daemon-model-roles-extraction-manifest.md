# Ocean Daemon Model Roles Extraction Manifest

**Date:** 2026-07-15
**Status:** Characterization complete and independently reviewed; production extraction pending
**Owner:** Ocean OS
**Rollback point:** `2c326bd`

## Purpose

Characterize and then move the daemon's immutable startup model-role loading and pure turn/advisor role resolution into one private binary module without changing configuration precedence, fail-open behavior, alias strings, explicit-model precedence, unknown-role fallback/warning signals, advisor activation, or agent-turn orchestration.

This module is a control-plane adapter over `ocean_agent::DaemonConfig`; it does not own model catalog/routing, credential readiness, provider resolution, persistence, turn execution, or advisor provider calls. `AppState` continues to hold the once-loaded role map, and parent composition continues to decide when to warn, invoke a turn, and spawn the post-turn advisor.

## Characterization and seam before extraction

Introduce one behavior-neutral helper in `main.rs`:

- `load_model_roles(config_dir: &Path) -> HashMap<String, String>`

Mechanically move the existing startup `DaemonConfig::load` match into the helper, then call it once with `ocean_agent::config_dir_from_env()` at the same startup position. Preserve the same informational/warning logs and exact success/error branches.

Add focused coverage in `main.rs`:

- `model_roles_load_missing_and_malformed_config_fail_open`
- `model_roles_load_preserves_aliases_verbatim`
- `model_role_resolution_is_exact_and_does_not_trim_inputs`

Keep and rely on existing:

- `resolve_advisor_alias_precedence`
- `role_resolution_known_unknown_and_model_id_precedence`

The combined tests freeze missing-config and malformed/invalid-whole-config fallback, verbatim key/value retention, exact/case-sensitive role lookup, blank alias behavior, explicit model precedence, unknown-role signaling, advisor override precedence, and blank per-turn advisor override fallback.

## Exact symbols to move intact

After characterization passes, move from `crates/ocean-daemon/src/main.rs` to new `crates/ocean-daemon/src/model_roles.rs`:

- `load_model_roles`
- `resolve_advisor_alias`
- `resolve_effective_model_id`

Move each symbol's role-specific documentation with it. Keep the pre-existing advisor-completion normalization documentation in `main.rs` with `advisor_note_if_actionable` when removing the intervening role resolver. The only permitted code changes are minimal `pub(super)` visibility, that documentation reattachment, and module-local imports/formatting.

## Files in scope

- `crates/ocean-daemon/src/main.rs`
- new `crates/ocean-daemon/src/model_roles.rs`
- `crates/ocean-daemon/AGENTS.md`
- `docs/DAEMON_REFACTOR_MISSION.md`
- `docs/specs/2026-07-12-ocean-code-health-and-agent-readiness-plan.md`
- this manifest
- root `events.md`

## Dependencies and visibility

The new module depends only on:

- `std::{collections::HashMap, path::Path}`;
- `ocean_agent::DaemonConfig`;
- `ocean_agent_sdk::AdvisorControl`;
- existing `tracing` macros.

All three functions are `pub(super)` only because parent startup/turn composition and retained parent tests call them. No item becomes public outside the daemon binary. `AppState.roles` remains `Arc<HashMap<String, String>>`; no state split or role service is introduced.

## Frozen startup and resolution invariants

- Roles load once during daemon startup, after durable stores are opened and before `AppState` assembly.
- The config path remains `ocean_agent::config_dir_from_env()/ocean.toml` through `DaemonConfig::load`.
- A missing config produces an empty role map.
- Any `DaemonConfig::load` failure—including parse, read, or whole-config validation failure—logs the same warning and disables all roles without aborting this startup step.
- A non-empty role map logs the same role count, whether `advisor` is present, and message; an empty map emits no role-loaded info line.
- Role keys and aliases remain verbatim, case-sensitive, and untrimmed; blank aliases remain present rather than being filtered.
- An explicit `model_id` always wins over a simultaneous role, including blank or otherwise unvalidated strings at this pure resolution seam.
- With no explicit model, a known role returns its cloned alias and `unknown=false`.
- An unknown exact role returns `(None, true)` so parent composition emits the existing warning and falls back to the runtime's global model.
- No role and no explicit model returns `(None, false)`.
- Advisor `enabled:false` suppresses both override and global advisor role.
- Advisor `enabled:true` uses a non-blank override model, otherwise falls back to the global `advisor` role.
- With no per-turn advisor override, the global `advisor` role controls whether the observer is enabled.
- Blank per-turn advisor models are treated as absent; configured global advisor aliases are returned verbatim, including blank aliases.

## Composition anchors and exclusions

This move does not:

- move or change `AppState`, startup ordering, `agent_turn`, warning call sites, request destructuring, runtime invocation, provider calls, background task spawning, or advisor event emission;
- change `DaemonConfig`, config parsing/validation, environment/config-dir precedence, model catalog/routing/readiness, or persisted model selection;
- validate, trim, normalize, resolve, or eagerly check any configured alias;
- change explicit-model/role/advisor precedence or unknown-role fallback;
- move advisor prompt/note/severity helpers, role logging at turn call sites, tests, router code, or HTTP/SSE contracts;
- introduce a public API, daemon library, service trait, substate, cache refresh, file watcher, dependency, or opportunistic cleanup.

Any startup order, error posture, logging branch, string normalization, precedence, warning signal, provider-routing, advisor activation, or turn behavior change stops this extraction and requires a separate decision.

## Validation

Characterization gate:

```bash
cargo test -p ocean-daemon model_roles_ -- --nocapture
cargo test -p ocean-daemon role_resolution_ -- --nocapture
cargo test -p ocean-daemon resolve_advisor_alias_precedence -- --nocapture
cargo test -p ocean-agent config::tests:: -- --nocapture
cargo test -p ocean-daemon router_contract -- --nocapture
cargo test -p ocean-daemon
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

Extraction completion gate:

```bash
cargo test -p ocean-daemon model_roles_ -- --nocapture
cargo test -p ocean-daemon role_resolution_ -- --nocapture
cargo test -p ocean-daemon resolve_advisor_alias_precedence -- --nocapture
cargo test -p ocean-agent config::tests:: -- --nocapture
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

A fresh reviewer must compare the moved definitions against the characterization commit, verify startup order and fail-open/logging branches, inspect exact role/advisor precedence and string handling, and report any unresolved medium-or-higher issue.

## Characterization result

A behavior-neutral `load_model_roles` helper now holds the exact startup match on post-rebase baseline `2c326bd`, with its call at the same startup position. Three new tests freeze missing/malformed/invalid-whole-config fail-open behavior, verbatim aliases and keys, exact/case-sensitive lookup, blank alias behavior, and explicit-model precedence; existing advisor and role-precedence tests remain authoritative. Two loader tests, two role-resolution tests, advisor precedence, ten `ocean-agent` config tests, all five router contracts, all 313 daemon tests, formatting, documentation, and diff checks pass in a dedicated target directory. Independent review found no unresolved medium-or-higher issue.

## Result

Pending extraction, completion validation, final review, publication, and deployment.

## Rollback

Revert the bounded extraction after the characterization point; if reverting the helper seam too, restore the startup match block at its original position. There is no data migration, wire-version handling, or compatibility cleanup.
