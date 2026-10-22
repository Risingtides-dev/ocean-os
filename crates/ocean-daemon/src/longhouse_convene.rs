//! Real Longhouse convene: ready-model admission, awaited council
//! orchestration over the production provider wire, registry-first event
//! projection, durable firekeeper title grant/bind, and one-time raw-token
//! response delivery.
//!
//! Composition retains routes, state/startup authority, the shared registry
//! and durable title store construction, event-bus/SSE policy, and tests.
//! The council algorithms (workers, planner, quorum/evidence engine, title
//! cryptography) remain in `ocean-longhouse`.

use axum::{extract::State, Json};
use ocean_agent_sdk::{AgentRole, Federation};
use serde_json::json;
use uuid::Uuid;

use super::AppState;

/// Request body for `POST /v1/longhouse/convene`.
#[derive(Debug, serde::Deserialize)]
pub(super) struct LonghouseConveneRequest {
    /// The question / task the council deliberates.
    question: String,
    /// Which department room hosts it: dev | sales | content | campaign |
    /// commons. Defaults to `commons` if omitted or unrecognized.
    #[serde(default)]
    federation: Option<String>,
    /// Optional model alias override; one worker per alias. Every supplied alias
    /// is validated against the daemon's live ready-model registry before the
    /// council starts; unknown or unready aliases are rejected (never silently
    /// resolved as a fallback).
    #[serde(default)]
    models: Option<Vec<String>>,
}

fn parse_federation(s: Option<&str>) -> Federation {
    match s.map(|v| v.trim().to_lowercase()).as_deref() {
        Some("dev") => Federation::Dev,
        Some("sales") => Federation::Sales,
        Some("content") => Federation::Content,
        Some("campaign") => Federation::Campaign,
        _ => Federation::Commons,
    }
}

/// Convene a **real** longhouse council: run cheap-model LLM workers through the
/// propose → endorse/inhibit rounds, let the daemon-side `QuorumEngine` decide
/// convergence, and stream the resulting `LonghouseEvent`s onto the existing
/// agent event bus — exactly like `longhouse_demo`, but driven by real agents
/// and a real quorum engine instead of a scripted timer. The deck renders it
/// with zero changes.
///
/// Blocks until the council finishes, then returns `200 { ok, question,
/// federation, streaming_on, title_id?, token? }`. `title_id` and `token` are
/// present only when the council converged; they are the firekeeper's durable
/// cross-turn claim credential. The token is delivered **only** in this direct
/// HTTP response — it is never emitted on the SSE/bus (OCEAN-229 discipline:
/// the bus carries public event data only, never secrets; an SSE sniff would
/// otherwise let any bus subscriber forge a firekeeper claim).
pub(super) async fn longhouse_convene(
    State(state): State<AppState>,
    Json(req): Json<LonghouseConveneRequest>,
) -> Json<serde_json::Value> {
    let bus = state.agent_events.clone();
    let registry = state.longhouse.clone();
    // The persisted title registry (OCEAN-272): the convened council mints its
    // firekeeper title into THIS durable store on convergence, so the title —
    // and the right to `claim_outcome` against it — survives the turn.
    let titles = state.titles.clone();
    let federation = parse_federation(req.federation.as_deref());

    let mut convene_req = ocean_longhouse::ConveneRequest::new(req.question.clone(), federation);
    if let Some(models) = req.models {
        if !models.is_empty() {
            let ready: std::collections::HashSet<_> = ocean_providers::known_models_with_readiness(
                &ocean_providers::ProviderEnv::from_process(),
            )
            .into_iter()
            .filter(|model| model.ready)
            .map(|model| model.model.id)
            .collect();
            let invalid: Vec<_> = models
                .iter()
                .filter(|model| !ready.contains(model.as_str()))
                .cloned()
                .collect();
            if !invalid.is_empty() {
                return Json(json!({
                    "ok": false,
                    "error": "longhouse council requires ready model ids from GET /v1/models",
                    "invalid_models": invalid,
                    "ready_models": ready,
                }));
            }
            convene_req.models = models;
        }
    }

    let topic_hint = convene_req.question.clone();
    let clock = ocean_longhouse::SystemClock;
    // Emit each longhouse event onto the agent bus, exactly as the demo does
    // (`bus.emit(ev.into_turn_event())`), so existing SSE clients render it —
    // AND tee it into the in-memory read-side registry so the topic survives a
    // client refresh during this process lifetime (OCEAN-58). The bus is the live
    // feed; the registry is not restart durability.
    //
    // OCEAN-229/339: the bus closure carries only `LonghouseEvent` variants
    // (TopicConvened, RoleGranted, Converged, …) — none of which carry the
    // secret token. The token lives only in the `FirekeeperTitle` returned by
    // `grant()` below and is delivered solely in this HTTP response body.
    let outcome = ocean_longhouse::convene(convene_req, &clock, |ev| {
        // Fold into the observable topic store first, then publish to the bus.
        // The closure is synchronous and drops the std Mutex guard before any
        // await. Lock acquisition can still block this worker on contention; this
        // checkpoint preserves that behavior rather than changing lock types.
        if let Ok(mut reg) = registry.lock() {
            reg.ingest(&ev);
        }
        bus.emit(ev.into_turn_event());
    })
    .await;

    // OCEAN-272/339: persist the firekeeper title for a converged council into
    // the durable registry, bound to the engine's decision. The in-frame title
    // inside `convene()` already gated the binding `Converged` it emitted
    // (OCEAN-229); this is the *additional* durable authority that lets a
    // firekeeper ratify in a LATER turn via `POST /v1/longhouse/claim`.
    //
    // Security: `grant()` mints the secret server-side from the CSPRNG and the
    // registry persists only a salt+SHA-256 verifier — the raw token is NEVER
    // stored and NEVER emitted on any event (we log only the public title_id /
    // agent_id / decision). The raw `FirekeeperTitle` returned here is used to
    // extract the token for this response, then dropped. The durable *authority*
    // persists in the registry; the secret travels only in this HTTP response,
    // directly to the convening caller. It never touches the SSE bus.
    let grant_result: Option<(Uuid, String)> = if let Some(decision) = outcome.decision {
        // The winning proposal's author holds the firekeeper title (the same
        // binding `convene()` uses). A converged outcome always has a recorded
        // firekeeper on the snapshot.
        let firekeeper = registry
            .lock()
            .ok()
            .and_then(|reg| reg.topic(&outcome.topic_id).and_then(|t| t.firekeeper));
        if let Some(firekeeper) = firekeeper {
            let now = ocean_protocol::now_ms();
            let mut reg = titles
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match reg.grant(outcome.topic_id, firekeeper, AgentRole::Firekeeper, now) {
                Ok((persisted, secret)) => {
                    // Bind the durable title to the engine's decision so a
                    // later, engine-free `claim_bound_outcome` can ratify
                    // exactly this proposal.
                    if let Err(e) = reg.bind_decision(persisted.title_id, decision) {
                        tracing::warn!(error = %e, "failed to bind persisted firekeeper title to decision");
                    }
                    tracing::info!(
                        topic = %outcome.topic_id,
                        title = %persisted.title_id,
                        firekeeper = %firekeeper,
                        decision = %decision,
                        "persisted firekeeper title bound to converged decision (claimable across turns)"
                    );
                    // Capture title_id + token before `secret` is dropped.
                    // The token is never logged; only the title_id is public.
                    Some((persisted.title_id, secret.token().to_string()))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to persist firekeeper title for converged council");
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    tracing::info!(
        topic = %outcome.topic_id,
        converged = outcome.decision.is_some(),
        convergence_basis = outcome
            .convergence_basis
            .map(|basis| basis.as_str())
            .unwrap_or("none"),
        proposals = outcome.proposals.len(),
        "longhouse council finished"
    );

    // Build the response. `title_id` and `token` are included only on
    // convergence — a caller MUST check `converged` before using them.
    // The token is the cross-turn claim credential; it must be stored by
    // the caller and presented verbatim to `POST /v1/longhouse/claim`.
    let mut resp = json!({
        "ok": true,
        "question": topic_hint,
        "federation": format!("{federation:?}").to_lowercase(),
        "streaming_on": "/v1/agent/events",
        "converged": grant_result.is_some(),
        "convergence_basis": outcome.convergence_basis.map(|basis| basis.as_str()),
    });
    if let Some((title_id, token)) = grant_result {
        resp["title_id"] = json!(title_id.to_string());
        resp["token"] = json!(token);
    }
    Json(resp)
}
