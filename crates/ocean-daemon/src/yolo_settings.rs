use axum::Json;
use serde_json::json;
use std::env;

/// OCEAN-51: whether the daemon runs the product agent-turn path
/// (`POST /v1/agent/turns`, and the voice wrapper) in "yolo" mode — every tool
/// auto-approved, no per-tool permission gating.
///
/// Default is `false`: tool calls are gated by `DaemonPermissionPolicy` exactly
/// as the permission machinery was designed, and a mutating tool will emit a
/// `PermissionRequest` event and block until an operator decision arrives via
/// `POST /v1/permissions/{id}/decision`.
///
/// Set `OCEAN_YOLO=1` (or `true`/`yes`/`on`) to restore the previous
/// fire-and-forget behavior for trusted automation. This is the documented,
/// explicit operator opt-in — the bypass is NEVER the silent default.
///
/// Read fresh on each turn (not cached) so an operator can flip it by restarting
/// with a different env without code changes, and so tests can scope it.
///
/// This is ONLY the env layer. The effective per-turn posture is resolved by
/// [`effective_yolo`], which layers the persisted operator default (OCEAN-YOLO)
/// underneath the env — every live call site now uses `effective_yolo`, so this
/// remains as the focused env-layer assertion target for tests.
#[cfg(test)]
pub(super) fn yolo_enabled() -> bool {
    yolo_env_pref().unwrap_or(false)
}

/// Parse the `OCEAN_YOLO` env var into an explicit preference: `Some(true)` /
/// `Some(false)` for a recognized spelling, `None` when unset or unrecognized
/// (so the caller falls through to the persisted setting). Recognizing the
/// "off" spellings explicitly (not just "absent") is what lets `OCEAN_YOLO=0`
/// OVERRIDE a persisted `true` for a session — the documented precedence.
fn yolo_env_pref() -> Option<bool> {
    match env::var("OCEAN_YOLO")
        .ok()?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Resolve the effective YOLO posture for a turn, in precedence order:
///
///   1. `OCEAN_YOLO` env, if set to a recognized value (operator/CI override),
///   2. the persisted operator default (OCEAN-YOLO — set once via
///      `POST /v1/settings/yolo`, survives restarts),
///   3. the built-in default: **off** (permission gating ON).
///
/// The legacy per-request `req.yolo` wire flag is deliberately ignored by
/// [`resolve_request_yolo`]: an untrusted client cannot opt itself into the
/// permission bypass. Every live call site resolves from operator policy only:
/// env → persisted → off.
///
/// Default-off is the safety invariant: nothing configured ⇒ gated. This
/// function only decides whether tools auto-approve; it does NOT touch the
/// permission decision-token binding (OCEAN-185), which stays orthogonal.
pub(super) fn effective_yolo() -> bool {
    if let Some(env_pref) = yolo_env_pref() {
        return env_pref;
    }
    ocean_agent::load_yolo_pref(&ocean_agent::config_dir_from_env()).unwrap_or(false)
}

/// Resolve the YOLO posture for a turn arriving on a wire `PromptRequest`
/// (`POST /v1/prompt`, `POST /v1/requests`), DELIBERATELY IGNORING the
/// client-supplied `wire_yolo` flag (OCEAN-160, P0).
///
/// History: the legacy handlers used to compute `req.yolo || effective_yolo()`.
/// Because `PromptRequest.yolo` deserializes straight off the request JSON, any
/// client could POST `{"yolo": true, ...}` and force the bypass on — every tool
/// auto-approved, the entire `DaemonPermissionPolicy` gate skipped — even when
/// the operator had NOT opted in. That is an auth-bypass: a per-request wire
/// flag must never be able to escalate past the operator's policy.
///
/// The fix matches the modern product path (`POST /v1/agent/turns`, see
/// `agent_turn`), whose `AgentTurnRequest` carries no yolo field at all and
/// resolves the posture purely from `effective_yolo()` (OCEAN_YOLO env →
/// persisted operator default → off). It also matches the established
/// epic-E7 pattern: OCEAN-162 documented that "the daemon ignores the wire
/// `yolo` flag and gates mutating tools on its own `OCEAN_YOLO`" and patched
/// the CLI to stop sending it — this closes the daemon side of that contract so
/// the field is truly inert, regardless of which client sends it.
///
/// A legitimate operator who relies on the persisted/env yolo default is
/// unaffected: that path runs through `effective_yolo()` exactly as before. The
/// `wire_yolo` parameter is accepted (and ignored) only so the inert flag is
/// explicit at the call site and the security intent is greppable.
pub(super) fn resolve_request_yolo(wire_yolo: bool) -> bool {
    // The wire flag is intentionally discarded; see the doc comment above.
    let _ = wire_yolo;
    effective_yolo()
}

#[derive(Debug, serde::Deserialize)]
pub(super) struct YoloSetRequest {
    /// The new persisted default. `true` opts into the permission-gating bypass
    /// (tools auto-approve); `false` restores gated/safe.
    pub(super) enabled: bool,
}

/// `GET /v1/settings/yolo` — report the operator's persisted YOLO default and
/// the *effective* posture (after env override), so a client can show both
/// "your saved default" and "what's actually in force right now".
///
/// Mirrors `model_get`'s shape: `{ ok, persisted, effective, env_override }`.
/// `persisted` is the saved personal default (null on first run); `effective`
/// is what a turn would actually use via [`effective_yolo`]; `env_override`
/// flags when `OCEAN_YOLO` is masking the persisted value.
pub(super) async fn yolo_setting_get() -> Json<serde_json::Value> {
    let persisted = ocean_agent::load_yolo_pref(&ocean_agent::config_dir_from_env());
    let env_override = yolo_env_pref();
    Json(json!({
        "ok": true,
        "persisted": persisted,
        "effective": effective_yolo(),
        "env_override": env_override,
    }))
}

/// `POST /v1/settings/yolo` — set + persist the operator's YOLO default. Writes
/// the preference under the config dir (same mechanism as the persisted model
/// selection) so it survives restarts. Mirrors `model_set`'s response shape and
/// returns the freshly resolved `effective` value so the caller sees whether an
/// env override is still masking their new default.
///
/// Persisting `enabled` does NOT weaken the permission decision-token binding
/// (OCEAN-185); it only sets the default for whether tools auto-approve.
pub(super) async fn yolo_setting_set(Json(req): Json<YoloSetRequest>) -> Json<serde_json::Value> {
    let config_dir = ocean_agent::config_dir_from_env();
    ocean_agent::persist_yolo_pref(&config_dir, req.enabled);
    let env_override = yolo_env_pref();
    tracing::info!(
        persisted = req.enabled,
        ?env_override,
        "yolo default persisted"
    );
    Json(json!({
        "ok": true,
        "persisted": req.enabled,
        "effective": effective_yolo(),
        "env_override": env_override,
    }))
}
