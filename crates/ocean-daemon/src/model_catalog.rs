use super::AppState;
use axum::{extract::State, Json};
use serde_json::json;

#[derive(Debug, serde::Deserialize)]
pub(super) struct ModelSetRequest {
    pub(super) model: String,
}

pub(super) async fn model_get(State(state): State<AppState>) -> Json<serde_json::Value> {
    let (provider, model) = state.runtime.current_model();
    Json(json!({"ok": true, "provider": provider, "model": model}))
}

/// List the models the daemon can route to, plus the currently selected one,
/// for a client model picker.
pub(super) async fn models_list(State(state): State<AppState>) -> Json<serde_json::Value> {
    let (provider, model) = state.runtime.current_model();
    // Per-model readiness (credential visible to THIS daemon process) so a
    // picker can tell the menu apart from what's actually selectable. Additive:
    // entries keep id/provider/label top-level and gain ready/credential_source.
    // Auth-file reads are blocking I/O, so they ride spawn_blocking.
    let models = tokio::task::spawn_blocking(|| {
        let env = ocean_agent::ProviderEnv::from_process();
        ocean_agent::known_models_with_readiness(&env)
    })
    .await
    .unwrap_or_default();
    Json(json!({
        "ok": true,
        "current": { "provider": provider, "model": model },
        "models": models,
    }))
}

pub(super) async fn model_set(
    State(state): State<AppState>,
    Json(req): Json<ModelSetRequest>,
) -> Json<serde_json::Value> {
    match state.runtime.set_model(&req.model) {
        Ok((provider, model)) => {
            tracing::info!(provider, model, "model swapped");
            Json(json!({"ok": true, "provider": provider, "model": model}))
        }
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}
