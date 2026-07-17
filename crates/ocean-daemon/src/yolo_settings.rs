use axum::Json;
use ocean_core::{PermissionMode, PermissionSettingsRequest, PermissionSettingsResponse};
use serde_json::json;
use std::env;

/// Legacy env switch for the permission bypass. The three-state setting keeps
/// this compatibility layer: true forces `skip_all`; false prohibits
/// `skip_all` while preserving a saved `manual` versus `automatic` choice.
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

/// Focused env-layer assertion target retained for the legacy YOLO tests.
#[cfg(test)]
pub(super) fn yolo_enabled() -> bool {
    yolo_env_pref().unwrap_or(false)
}

fn persisted_permission_mode() -> Option<PermissionMode> {
    ocean_agent::load_permission_mode(&ocean_agent::config_dir_from_env())
}

/// Resolve the daemon-owned approval policy for a newly-started turn.
///
/// Precedence:
/// 1. `OCEAN_YOLO=1` forces `skip_all`.
/// 2. `OCEAN_YOLO=0` prevents `skip_all`, but does not collapse an explicit
///    `manual` choice into `automatic`.
/// 3. The persisted three-state choice (with legacy `yolo_pref` migration).
/// 4. `automatic`, the safe autonomous default: read-only tools run while
///    mutating/side-effecting tools keep their permission gate.
pub(super) fn effective_permission_mode() -> PermissionMode {
    let persisted = persisted_permission_mode().unwrap_or_default();
    match yolo_env_pref() {
        Some(true) => PermissionMode::SkipAll,
        Some(false) if persisted == PermissionMode::SkipAll => PermissionMode::Automatic,
        Some(false) | None => persisted,
    }
}

pub(super) fn permission_env_override() -> Option<PermissionMode> {
    let persisted = persisted_permission_mode().unwrap_or_default();
    match yolo_env_pref() {
        Some(true) if persisted != PermissionMode::SkipAll => Some(PermissionMode::SkipAll),
        Some(false) if persisted == PermissionMode::SkipAll => Some(PermissionMode::Automatic),
        Some(true) | Some(false) | None => None,
    }
}

/// Legacy boolean view used by voice dead-end checks and compatibility APIs.
pub(super) fn effective_yolo() -> bool {
    effective_permission_mode() == PermissionMode::SkipAll
}

/// Resolve an inert legacy request `yolo` flag into the operator's three-state
/// policy. A client-supplied flag can never escalate daemon authority.
pub(super) fn resolve_request_permission_mode(wire_yolo: bool) -> PermissionMode {
    let _ = wire_yolo;
    effective_permission_mode()
}

/// Boolean compatibility view retained as a focused inert-wire test target.
#[cfg(test)]
pub(super) fn resolve_request_yolo(wire_yolo: bool) -> bool {
    resolve_request_permission_mode(wire_yolo) == PermissionMode::SkipAll
}

#[derive(Debug, serde::Deserialize)]
pub(super) struct YoloSetRequest {
    /// `true` maps to `skip_all`; `false` maps to `automatic`.
    pub(super) enabled: bool,
}

/// `GET /v1/settings/yolo` — retained compatibility shape.
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

/// `POST /v1/settings/yolo` — retained compatibility adapter. Writing the bool
/// also updates the new three-state preference so old and new clients cannot
/// leave conflicting settings behind.
pub(super) async fn yolo_setting_set(Json(req): Json<YoloSetRequest>) -> Json<serde_json::Value> {
    let config_dir = ocean_agent::config_dir_from_env();
    let mode = if req.enabled {
        PermissionMode::SkipAll
    } else {
        PermissionMode::Automatic
    };
    if let Err(error) = ocean_agent::persist_permission_mode(&config_dir, mode) {
        tracing::warn!(%error, "failed to persist yolo default");
        return Json(json!({
            "ok": false,
            "persisted": ocean_agent::load_yolo_pref(&config_dir),
            "effective": effective_yolo(),
            "env_override": yolo_env_pref(),
            "error": error.to_string(),
        }));
    }
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

/// `GET /v1/settings/permissions` — report both the saved choice and the mode a
/// newly-started turn will actually use after the legacy env override.
pub(super) async fn permission_settings_get() -> Json<PermissionSettingsResponse> {
    Json(PermissionSettingsResponse {
        ok: true,
        error: None,
        persisted: persisted_permission_mode(),
        effective: effective_permission_mode(),
        env_override: permission_env_override(),
    })
}

/// `POST /v1/settings/permissions` — persist the operator's three-state global
/// approval policy. The daemon resolves it at turn start, so in-flight turns
/// keep their captured policy and subsequent turns use the new choice.
pub(super) async fn permission_settings_set(
    Json(req): Json<PermissionSettingsRequest>,
) -> Json<PermissionSettingsResponse> {
    let config_dir = ocean_agent::config_dir_from_env();
    if let Err(error) = ocean_agent::persist_permission_mode(&config_dir, req.mode) {
        tracing::warn!(%error, requested = ?req.mode, "failed to persist permission mode");
        return Json(PermissionSettingsResponse {
            ok: false,
            error: Some(error.to_string()),
            persisted: persisted_permission_mode(),
            effective: effective_permission_mode(),
            env_override: permission_env_override(),
        });
    }
    let effective = effective_permission_mode();
    let env_override = permission_env_override();
    tracing::info!(persisted = ?req.mode, ?effective, ?env_override, "permission mode persisted");
    Json(PermissionSettingsResponse {
        ok: true,
        error: None,
        persisted: Some(req.mode),
        effective,
        env_override,
    })
}
