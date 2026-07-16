use std::sync::{Arc, LazyLock};

use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use ocean_agent::{
    AgentRuntime, HistorySearchCapacityError, HistorySearchHit, MAX_HISTORY_SEARCH_QUERY_CHARS,
    MAX_HISTORY_SEARCH_STORE_BYTES,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use super::AppState;

/// A typeahead client can supersede a request before its blocking filesystem
/// scan notices. Bound concurrent scans globally so abandoned/parallel queries
/// cannot flood Tokio's blocking pool or multiply full-store I/O without limit.
const MAX_CONCURRENT_HISTORY_SEARCHES: usize = 2;
static HISTORY_SEARCH_SLOTS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_CONCURRENT_HISTORY_SEARCHES)));

#[derive(Debug, Default, Deserialize)]
pub(super) struct HistorySearchQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub(super) struct HistorySearchResponse {
    ok: bool,
    query: String,
    hits: Vec<HistorySearchHit>,
    error: Option<String>,
}

/// `GET /v1/agent/history/search` searches only ocean-agent's persisted,
/// display-ready transcript projection. The blocking file scan runs off the
/// async executor and cannot invoke a provider or embedding service.
pub(super) async fn history_search(
    State(state): State<AppState>,
    Query(params): Query<HistorySearchQuery>,
) -> (StatusCode, Json<HistorySearchResponse>) {
    let query = params.q.unwrap_or_default();
    if query.trim().is_empty() {
        return response(
            StatusCode::BAD_REQUEST,
            query,
            Vec::new(),
            Some("query parameter q must not be empty".into()),
        );
    }
    if query.trim().chars().count() > MAX_HISTORY_SEARCH_QUERY_CHARS {
        return response(
            StatusCode::BAD_REQUEST,
            query,
            Vec::new(),
            Some(format!(
                "query exceeds {MAX_HISTORY_SEARCH_QUERY_CHARS} characters"
            )),
        );
    }
    let permit = match HISTORY_SEARCH_SLOTS.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return response(
                StatusCode::TOO_MANY_REQUESTS,
                query,
                Vec::new(),
                Some("too many transcript history searches are already running".into()),
            );
        }
    };

    let runtime: Arc<AgentRuntime> = state.runtime.clone();
    let search_query = query.clone();
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        runtime.search_history(&search_query, params.limit)
    })
    .await
    {
        Ok(Ok(hits)) => response(StatusCode::OK, query, hits, None),
        Ok(Err(error)) => {
            tracing::warn!(error = %error, "history transcript search failed");
            search_error_response(query, &error)
        }
        Err(error) => {
            tracing::warn!(error = %error, "history transcript search task failed");
            response(
                StatusCode::INTERNAL_SERVER_ERROR,
                query,
                Vec::new(),
                Some("transcript history could not be searched".into()),
            )
        }
    }
}

fn search_error_response(
    query: String,
    error: &anyhow::Error,
) -> (StatusCode, Json<HistorySearchResponse>) {
    if error.downcast_ref::<HistorySearchCapacityError>().is_some() {
        response(
            StatusCode::SERVICE_UNAVAILABLE,
            query,
            Vec::new(),
            Some(format!(
                "transcript history exceeds the {} MiB search capacity",
                MAX_HISTORY_SEARCH_STORE_BYTES / (1024 * 1024)
            )),
        )
    } else {
        response(
            StatusCode::INTERNAL_SERVER_ERROR,
            query,
            Vec::new(),
            Some("transcript history could not be searched".into()),
        )
    }
}

fn response(
    status: StatusCode,
    query: String,
    hits: Vec<HistorySearchHit>,
    error: Option<String>,
) -> (StatusCode, Json<HistorySearchResponse>) {
    (
        status,
        Json(HistorySearchResponse {
            ok: status.is_success(),
            query,
            hits,
            error,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_errors_map_to_an_explicit_service_response() {
        let error: anyhow::Error = HistorySearchCapacityError {
            observed_bytes: MAX_HISTORY_SEARCH_STORE_BYTES + 1,
            max_bytes: MAX_HISTORY_SEARCH_STORE_BYTES,
        }
        .into();
        let (status, Json(body)) = search_error_response("ocean".into(), &error);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(!body.ok);
        assert_eq!(body.query, "ocean");
        assert!(body.hits.is_empty());
        assert!(body
            .error
            .as_deref()
            .unwrap()
            .contains("64 MiB search capacity"));
    }
}
