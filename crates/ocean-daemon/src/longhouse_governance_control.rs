//! Longhouse persisted-title and read-projection mutation HTTP adapters.
//!
//! Composition retains routes, state/startup authority, real convene/title
//! grant-bind/token delivery, storage paths, event-bus/SSE policy, and tests.

use axum::{extract::State, http::StatusCode, Json};
use ocean_agent_sdk::{AgentRole, LonghouseEvent, Mark, MarkKind};
use serde_json::json;
use uuid::Uuid;

use super::{cast_recall_vote, remove_recall_tally, AppState};

// ---- OCEAN-272: persisted-escrow ops (claim_outcome / board_post) ----------
//
// These are the two ops `longhouse_provider.rs` deliberately deferred ("there is
// no persisted, daemon-held engine to … claim an outcome against between turns").
// OCEAN-246 shipped the durable `SqliteTitleRegistry`; OCEAN-272 holds it on
// `AppState` (so it survives the turn) and exposes these endpoints against it.
//
// Security posture (mirrors #185/#220/#229/#246):
//   * `claim` verifies the persisted title's secret in CONSTANT TIME and rejects a
//     revoked/released title even with the correct token; it ratifies only the
//     decision the daemon durably bound at convergence (the firekeeper signs the
//     engine's choice, never its own). Verified before any decision state is read,
//     so a forged/revoked caller learns nothing.
//   * Longhouse stays advisory/coordinating: a successful claim records the close
//     and releases validator escrow; it does NOT execute anything or bypass a
//     daemon permission gate. The agent-facing tool seam (`longhouse_provider.rs`)
//     keeps `requires_permission() == true`, so an agent claiming via a tool is
//     still gated like `bash`/`write` (post-OCEAN-54).

/// Run a closure with the locked persisted title registry, recovering a poisoned
/// lock the same way the room/longhouse handlers do (`into_inner`). Synchronous:
/// the guard is dropped before this returns, so no `await` is held across it.
pub(super) fn with_titles<T>(
    state: &AppState,
    f: impl FnOnce(&mut ocean_longhouse::SqliteTitleRegistry) -> T,
) -> T {
    let mut guard = match state.titles.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(&mut guard)
}

/// Request body for `POST /v1/longhouse/claim`.
#[derive(Debug, serde::Deserialize)]
pub(super) struct LonghouseClaimRequest {
    /// The persisted title's id (public handle; on its own it grants nothing).
    title_id: String,
    /// The public agent id that holds the title (the firekeeper).
    agent_id: String,
    /// The secret proof-of-title minted server-side at convene-grant. Constant-
    /// time-verified against the stored salt+hash verifier; never logged.
    token: String,
    /// The proposal the firekeeper claims as the converged outcome. Must equal the
    /// decision the registry durably bound at convergence, else `WrongDecision`.
    decision: String,
}

/// `POST /v1/longhouse/claim` — the daemon-held `claim_outcome` (OCEAN-272). A
/// firekeeper ratifies a converged outcome against the **persisted** title
/// registry, in a turn LATER than the one that minted the title. Verifies the
/// title's secret in constant time, rejects a revoked/released title even with
/// the correct token, and accepts only the durably-bound decision. On success,
/// the title is released and the topic's validator escrow is released.
///
/// Status mapping: 200 on a ratified claim; 403 for a forged/revoked title
/// (`ForgedFirekeeper`); 409 for a premature (`NotConverged`) or wrong-proposal
/// (`WrongDecision`) claim; 400 for a malformed UUID. The body is a typed
/// `{ ok, … }` shape, never a panic.
pub(super) async fn longhouse_claim(
    State(state): State<AppState>,
    Json(req): Json<LonghouseClaimRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let parse = |label: &str, raw: &str| {
        Uuid::parse_str(raw.trim()).map_err(|_| format!("`{label}` is not a valid UUID: {raw:?}"))
    };
    let (title_id, agent_id, decision) = match (
        parse("title_id", &req.title_id),
        parse("agent_id", &req.agent_id),
        parse("decision", &req.decision),
    ) {
        (Ok(t), Ok(a), Ok(d)) => (t, a, d),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "ok": false, "error": e })),
            );
        }
    };

    // A blank token can never authorize; reject it as a forged claim WITHOUT
    // touching the registry (uniform with a wrong token, leaks nothing).
    let token = req.token.trim();
    let presented = if token.is_empty() { None } else { Some(token) };

    let now = ocean_protocol::now_ms();
    let result = with_titles(&state, |reg| {
        ocean_longhouse::claim_bound_outcome(reg, title_id, agent_id, presented, decision, now)
    });

    match result {
        Ok(released) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "title_id": title_id,
                "decision": decision,
                "escrow_released": released,
            })),
        ),
        // Forged identity OR a revoked/released title — refused identically so the
        // verdict leaks neither the title's existence nor the bound decision.
        Err(ocean_longhouse::ClaimError::ForgedFirekeeper) => (
            StatusCode::FORBIDDEN,
            Json(json!({
                "ok": false,
                "error": "claim refused: title not proven, or it has been revoked/released",
            })),
        ),
        // Engine never bound a decision for this title (premature claim).
        Err(ocean_longhouse::ClaimError::NotConverged) => (
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "error": "claim refused: no converged decision is bound to this title yet",
            })),
        ),
        // Right title, wrong proposal — the firekeeper may only sign the engine's
        // own decision.
        Err(ocean_longhouse::ClaimError::WrongDecision {
            engine_decision,
            claimed,
        }) => (
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "error": format!(
                    "claim refused: the bound decision is {engine_decision}, not {claimed}"
                ),
                "engine_decision": engine_decision,
                "claimed": claimed,
            })),
        ),
    }
}

/// Request body for `POST /v1/longhouse/revoke`.
#[derive(Debug, serde::Deserialize)]
pub(super) struct LonghouseRevokeRequest {
    /// The persisted title to pull. After a successful revoke it can never ratify
    /// a claim again, even with the correct token.
    title_id: String,
    /// Human-facing reason recorded on the audit row (e.g. "unsafe tool call").
    #[serde(default)]
    reason: Option<String>,
}

/// `POST /v1/longhouse/revoke` — execute a hard recall of a persisted title via
/// the daemon's single [`ocean_longhouse::Revoker`] (OCEAN-246/272, the "War
/// Chief").
///
/// **Current trust boundary.** This handler has no caller-authentication extractor.
/// Any request that reaches the local route with a valid live `title_id` asks the
/// daemon to execute revocation. Loopback exposure and CORS restrictions are the
/// current deployment posture; CORS is not authentication. The daemon presents
/// its server-minted `RevokerKey`, which is never emitted on the wire. That key
/// authenticates the daemon to the title engine; it does not authenticate the HTTP
/// caller. The cryptographic execute capability remains daemon-held even though
/// caller admission is outside this handler.
///
/// 200 on a pulled title; 404 if unknown; 409 if the title was already
/// revoked/released (`NotLive`); 400 on a malformed UUID. (`Unauthorized` is
/// unreachable here — the daemon always presents its own key — but is mapped to
/// 403 for completeness.)
pub(super) async fn longhouse_revoke(
    State(state): State<AppState>,
    Json(req): Json<LonghouseRevokeRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let title_id = match Uuid::parse_str(req.title_id.trim()) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "error": format!("`title_id` is not a valid UUID: {:?}", req.title_id),
                })),
            );
        }
    };
    let detail = req
        .reason
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("operator-initiated recall")
        .to_string();

    let now = ocean_protocol::now_ms();
    // The daemon presents ITS OWN key (held on AppState, never on the wire) — the
    // execute side of decide≠execute. We pull a clone of the Arc'd Revoker out so
    // the title-registry lock is the only lock held across the call.
    let revoker = state.revoker.clone();
    let key = revoker.key();
    let result = with_titles(&state, |reg| {
        revoker.revoke(
            reg,
            Some(key.secret()),
            title_id,
            ocean_longhouse::RevokeAuthorization::PolicyBreach { detail },
            now,
        )
    });

    match result {
        Ok(revocation) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "title_id": revocation.title_id,
                "topic_id": revocation.topic_id,
                "agent_id": revocation.agent_id,
                "reason": revocation.reason,
            })),
        ),
        Err(ocean_longhouse::RevokeError::UnknownTitle(id)) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": format!("no title with id '{id}'") })),
        ),
        Err(ocean_longhouse::RevokeError::NotLive(id)) => (
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "error": format!("title '{id}' is not live (already revoked/released)"),
            })),
        ),
        Err(ocean_longhouse::RevokeError::Unauthorized) => (
            StatusCode::FORBIDDEN,
            Json(json!({ "ok": false, "error": "revoke refused: missing Revoker capability" })),
        ),
        Err(ocean_longhouse::RevokeError::Storage(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": format!("revoke storage error: {e}") })),
        ),
    }
}

// ---- OCEAN-302: Revoker triggers (recall tally + policy-breach report) -------
//
// The direct `POST /v1/longhouse/revoke` path and these two trigger routes have no
// caller-authentication extractor. Requests accepted by the local router can ask
// the daemon to exercise its single `Revoker`, whose server-minted `RevokerKey` is
// held on `AppState` and never sent on the wire. The key authenticates the daemon
// to the title engine; it does not authenticate the HTTP caller.
//
//   * **recall**: the daemon counts distinct caller-supplied `voter_id` UUIDs in a
//     pure `RecallVote`. The first request fixes the threshold; omitted or zero is
//     clamped to one, so one distinct caller can carry a threshold-one tally. A
//     carried tally asks the daemon-held Revoker to pull the title.
//   * **breach**: each accepted report accrues a graduated strike via `warn`; the
//     third accepted report reaches the fixed threshold and asks the daemon-held
//     Revoker to hard-pull the title.

/// The strike count at which the daemon escalates a graduated policy-breach to a
/// hard recall. Three strikes is the documented graduated default ("warn twice,
/// pull on the third"); a true zero-tolerance breach uses `revoke` directly.
const POLICY_BREACH_STRIKE_THRESHOLD: u8 = 3;

/// Request body for `POST /v1/longhouse/recall`.
#[derive(Debug, serde::Deserialize)]
pub(super) struct LonghouseRecallRequest {
    /// The topic whose seated firekeeper is under recall.
    topic_id: String,
    /// The firekeeper (public agent id) the council moves no confidence in. The
    /// daemon resolves this + `topic_id` to the live firekeeper title to pull.
    firekeeper_id: String,
    /// Caller-supplied public agent UUID used as the tally's distinct-voter key.
    /// Repeating the same UUID counts once; this field is not authenticated by the
    /// handler, so distinctness is not proof of distinct human or agent callers.
    voter_id: String,
    /// Distinct caller-supplied UUIDs required to carry the recall. Recorded when
    /// the recall is FIRST opened for a title and immutable thereafter — a later
    /// request cannot lower it. Absent/zero ⇒ clamped to 1 by the engine, so the
    /// first distinct UUID carries a threshold-one tally.
    #[serde(default)]
    threshold: Option<usize>,
}

/// `POST /v1/longhouse/recall` — submit a no-confidence vote UUID for a seated
/// firekeeper (OCEAN-302, quorum-of-recall). The handler does not authenticate the
/// caller-supplied `voter_id`; it only deduplicates equal UUIDs. The first request
/// fixes the threshold, and absent/zero is clamped to one. When the tally carries,
/// the daemon presents its own `RevokerKey` and hard-pulls the title via the same
/// `Revoker` the manual route uses. The key gates execution inside the title
/// engine but does not authenticate the route caller. A revoked title then fails
/// `claim_outcome` even with the correct token (#246/#272).
///
/// Status: 200 with `{ carried: false, votes, threshold }` while the recall is
/// still pending; 200 with `{ carried: true, revocation }` when it carries and
/// the title is pulled; 404 if no live firekeeper title exists for
/// `(topic_id, firekeeper_id)`; 400 on a malformed UUID.
pub(super) async fn longhouse_recall(
    State(state): State<AppState>,
    Json(req): Json<LonghouseRecallRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let parse = |label: &str, raw: &str| {
        Uuid::parse_str(raw.trim()).map_err(|_| format!("`{label}` is not a valid UUID: {raw:?}"))
    };
    let (topic_id, firekeeper_id, voter_id) = match (
        parse("topic_id", &req.topic_id),
        parse("firekeeper_id", &req.firekeeper_id),
        parse("voter_id", &req.voter_id),
    ) {
        (Ok(t), Ok(f), Ok(v)) => (t, f, v),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "ok": false, "error": e })),
            );
        }
    };

    // Resolve the firekeeper's LIVE title from public coordinates. No live title
    // (never seated, or already revoked/released) ⇒ nothing to recall. We do this
    // first so a recall against a non-existent/closed title is a clean 404 rather
    // than opening an orphan tally.
    let title = match with_titles(&state, |reg| {
        reg.find_live(topic_id, firekeeper_id, AgentRole::Firekeeper)
    }) {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "ok": false,
                    "error": format!(
                        "no live firekeeper title for topic '{topic_id}' held by '{firekeeper_id}'"
                    ),
                })),
            );
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "ok": false, "error": format!("title lookup failed: {e}") })),
            );
        }
    };

    // Cast the vote into the per-title tally (creating it on the first vote with
    // the threshold fixed there). The threshold on later requests is ignored — it
    // cannot be lowered to forge a quick carry.
    let threshold = req.threshold.unwrap_or(0); // RecallVote clamps 0 → 1
    let outcome = cast_recall_vote(&state.recalls, title.title_id, voter_id, threshold);

    // Pending → report the running count. Not carried: the title is untouched.
    if let ocean_longhouse::RecallOutcome::Pending { votes, threshold } = outcome {
        return (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "carried": false,
                "title_id": title.title_id,
                "votes": votes,
                "threshold": threshold,
            })),
        );
    }

    // Carried → the daemon (holding its key) executes the deposition. The pull is
    // still key-gated by the Revoker, so this is the only thing that can revoke,
    // and only on a genuinely-carried tally.
    let revoker = state.revoker.clone();
    let key = revoker.key();
    let now = ocean_protocol::now_ms();
    let result = with_titles(&state, |reg| {
        ocean_longhouse::recall_to_revocation(&revoker, reg, Some(key.secret()), &outcome, now)
    });

    match result {
        Ok(revocation) => {
            // Drop the now-spent tally so a re-opened recall on a fresh title is
            // not shadowed by a carried one.
            remove_recall_tally(&state.recalls, title.title_id);
            tracing::info!(
                topic = %topic_id,
                title = %revocation.title_id,
                firekeeper = %revocation.agent_id,
                "quorum-of-recall carried: firekeeper title revoked (OCEAN-302)"
            );
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "carried": true,
                    "title_id": revocation.title_id,
                    "topic_id": revocation.topic_id,
                    "agent_id": revocation.agent_id,
                    "reason": revocation.reason,
                })),
            )
        }
        // The tally carried but the title was already pulled (a race with another
        // trigger). Treat as a benign already-revoked outcome.
        Err(ocean_longhouse::TriggerRefused::Revoke(ocean_longhouse::RevokeError::NotLive(id))) => {
            (
                StatusCode::CONFLICT,
                Json(json!({
                    "ok": false,
                    "error": format!("title '{id}' is already revoked/released"),
                })),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": format!("recall execution failed: {e}") })),
        ),
    }
}

/// Request body for `POST /v1/longhouse/breach`.
#[derive(Debug, serde::Deserialize)]
pub(super) struct LonghouseBreachRequest {
    /// The seated title reported as having breached policy (persisted title id).
    title_id: String,
    /// Short human-facing description of the breach (recorded on the audit row,
    /// e.g. "acted outside bound decision", "claim failed verification N times").
    #[serde(default)]
    detail: Option<String>,
}

/// `POST /v1/longhouse/breach` — submit a policy-breach report against a seated
/// title (OCEAN-302, policy-breach trigger). The handler does not authenticate or
/// independently detect the report. Each accepted request asks the daemon-held
/// Revoker to accrue a graduated strike via `warn`; the third accepted report
/// reaches [`POLICY_BREACH_STRIKE_THRESHOLD`] and hard-revokes the title. The
/// Revoker key is held on `AppState` and never sent on the wire, but it authenticates
/// daemon execution rather than the HTTP caller. A revoked title then fails
/// `claim_outcome` even with the correct token (#246/#272).
///
/// Status: 200 with `{ revoked: false, strikes, threshold }` while below
/// threshold; 200 with `{ revoked: true, revocation }` when the breach tips the
/// gradient and the title is pulled; 404 if the title is unknown. The current
/// owner returns zero strikes for an already closed title, so a later report is
/// also 200 with `revoked: false, strikes: 0`; the mapped `NotLive` 409 branch is
/// not reached by that path. Malformed UUIDs return 400.
pub(super) async fn longhouse_breach(
    State(state): State<AppState>,
    Json(req): Json<LonghouseBreachRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let title_id = match Uuid::parse_str(req.title_id.trim()) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "error": format!("`title_id` is not a valid UUID: {:?}", req.title_id),
                })),
            );
        }
    };
    let detail = req
        .detail
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("policy breach")
        .to_string();

    let revoker = state.revoker.clone();
    let key = revoker.key();
    let now = ocean_protocol::now_ms();
    let ledger = ocean_longhouse::PolicyBreachLedger::new(POLICY_BREACH_STRIKE_THRESHOLD);
    let result = with_titles(&state, |reg| {
        ledger.report(&revoker, reg, Some(key.secret()), title_id, &detail, now)
    });

    match result {
        Ok(ocean_longhouse::BreachAction::Warned { strikes, threshold }) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "revoked": false,
                "title_id": title_id,
                "strikes": strikes,
                "threshold": threshold,
            })),
        ),
        Ok(ocean_longhouse::BreachAction::Revoked(revocation)) => {
            tracing::info!(
                title = %revocation.title_id,
                firekeeper = %revocation.agent_id,
                "policy-breach threshold reached: firekeeper title revoked (OCEAN-302)"
            );
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "revoked": true,
                    "title_id": revocation.title_id,
                    "topic_id": revocation.topic_id,
                    "agent_id": revocation.agent_id,
                    "reason": revocation.reason,
                })),
            )
        }
        Err(ocean_longhouse::RevokeError::UnknownTitle(id)) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": format!("no title with id '{id}'") })),
        ),
        Err(ocean_longhouse::RevokeError::NotLive(id)) => (
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "error": format!("title '{id}' is not live (already revoked/released)"),
            })),
        ),
        Err(ocean_longhouse::RevokeError::Unauthorized) => (
            StatusCode::FORBIDDEN,
            Json(json!({ "ok": false, "error": "breach refused: missing Revoker capability" })),
        ),
        Err(ocean_longhouse::RevokeError::Storage(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": format!("breach storage error: {e}") })),
        ),
    }
}

/// Request body for `POST /v1/longhouse/board`.
#[derive(Debug, serde::Deserialize)]
pub(super) struct LonghouseBoardPostRequest {
    /// The topic whose in-memory read-side projection receives the mark.
    topic_id: String,
    /// The agent posting the mark.
    author: String,
    /// Mark kind: `note` (default) or `evidence`. Proposal/endorse/inhibit are
    /// quorum-affecting and are produced by the council's workers inside
    /// `convene()`, never posted ad hoc here — this annotation is not a vote and
    /// never decides quorum.
    #[serde(default)]
    kind: Option<String>,
    /// Short human-facing summary of the mark (shown on the deck's blackboard).
    summary: String,
}

/// Map a board-post `kind` string to a non-quorum-affecting [`MarkKind`].
/// Anything other than an explicit `evidence` is a free-form `note`; the
/// quorum-affecting kinds (proposal/endorse/inhibit) are intentionally not
/// accepted here so a board post can never move convergence.
fn parse_board_mark_kind(s: Option<&str>) -> MarkKind {
    match s.map(|v| v.trim().to_lowercase()).as_deref() {
        Some("evidence") => MarkKind::Evidence,
        _ => MarkKind::Note,
    }
}

/// `POST /v1/longhouse/board` — `board_post` (OCEAN-272): append a note/evidence
/// mark to a tracked topic's daemon-held, in-memory `LonghouseRegistry` projection
/// and publish `MarkPosted` onto the agent bus so live decks render it. The mark
/// does not decide quorum; the council engine does. The projection is not restart
/// durability. 404 if the topic isn't tracked, 400 on a malformed UUID.
pub(super) async fn longhouse_board_post(
    State(state): State<AppState>,
    Json(req): Json<LonghouseBoardPostRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let topic_id = match Uuid::parse_str(req.topic_id.trim()) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "error": format!("`topic_id` is not a valid UUID: {:?}", req.topic_id),
                })),
            );
        }
    };
    let author = match Uuid::parse_str(req.author.trim()) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "error": format!("`author` is not a valid UUID: {:?}", req.author),
                })),
            );
        }
    };
    let summary = req.summary.trim();
    if summary.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "`summary` must be a non-empty string" })),
        );
    }

    // The topic must already be tracked — a board post annotates an existing
    // council's record, it does not create a topic.
    let exists = match state.longhouse.lock() {
        Ok(reg) => reg.topic(&topic_id).is_some(),
        Err(poisoned) => poisoned.into_inner().topic(&topic_id).is_some(),
    };
    if !exists {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "ok": false,
                "error": format!("no longhouse topic with id '{topic_id}'"),
            })),
        );
    }

    let mark_id = Uuid::new_v4();
    let event = LonghouseEvent::MarkPosted {
        topic_id,
        mark: Mark {
            mark_id,
            author,
            kind: parse_board_mark_kind(req.kind.as_deref()),
            target: None,
            summary: summary.to_string(),
        },
    };
    // On a healthy lock, fold into the in-memory read projection first, then
    // publish to the live bus; drop the std Mutex guard before emit. If the second
    // lock attempt is poisoned, current behavior skips projection but still
    // publishes and returns success.
    if let Ok(mut reg) = state.longhouse.lock() {
        reg.ingest(&event);
    }
    state.agent_events.emit(event.into_turn_event());

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "topic_id": topic_id,
            "mark_id": mark_id,
        })),
    )
}
