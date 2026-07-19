//! Public, read-only GitHub REST projections for registered Ocean projects.
//!
//! This module deliberately owns no credentials and exposes no aggregate or
//! mutation route. Repository identity is derived only from the registered
//! workspace's `origin`, then all upstream JSON is deserialized into minimal
//! typed inputs and projected into bounded outward structs.

use std::{
    future::Future,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    body::{Body, Bytes},
    extract::{rejection::PathRejection, rejection::QueryRejection, Path, Query, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Extension,
};
use moka::future::Cache;
use reqwest::Client;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    time::timeout,
};

use crate::AppState;

const GITHUB_ORIGIN: &str = "https://api.github.com";
const USER_AGENT: &str = "ocean-daemon/0.1";
const ACCEPT: &str = "application/vnd.github+json";
const API_VERSION: &str = "2022-11-28";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
const REMOTE_TIMEOUT: Duration = Duration::from_secs(5);
const CACHE_TTL: Duration = Duration::from_secs(60);
const CACHE_CAPACITY: u64 = 256;
const MAX_UPSTREAM_BYTES: usize = 2_097_152;
const MAX_REMOTE_BYTES: usize = 4_096;

const X_OCEAN_CACHE: HeaderName = HeaderName::from_static("x-ocean-cache");
const X_OCEAN_CACHE_AGE: HeaderName = HeaderName::from_static("x-ocean-cache-age");
const X_UPSTREAM_RATE_REMAINING: HeaderName = HeaderName::from_static("x-ratelimit-remaining");
const X_UPSTREAM_RATE_RESET: HeaderName = HeaderName::from_static("x-ratelimit-reset");
const X_OUTWARD_RATE_REMAINING: HeaderName =
    HeaderName::from_static("x-github-ratelimit-remaining");
const X_OUTWARD_RATE_RESET: HeaderName = HeaderName::from_static("x-github-ratelimit-reset");

#[derive(Clone)]
pub(super) struct GitHubService {
    client: Client,
    origin: Arc<str>,
    responses: Cache<CacheKey, CachedResponse>,
    remotes: Cache<PathBuf, RepoIdentity>,
    request_timeout: Duration,
    remote_timeout: Duration,
}

impl GitHubService {
    pub(super) fn new() -> anyhow::Result<Self> {
        Ok(Self {
            client: github_client()?,
            origin: Arc::from(GITHUB_ORIGIN),
            responses: response_cache(CACHE_TTL),
            remotes: remote_cache(CACHE_TTL),
            request_timeout: TOTAL_TIMEOUT,
            remote_timeout: REMOTE_TIMEOUT,
        })
    }

    #[cfg(test)]
    fn with_origin(origin: &str) -> Self {
        Self::with_test_config(origin, TOTAL_TIMEOUT, CACHE_TTL)
    }

    #[cfg(test)]
    fn with_test_config(origin: &str, request_timeout: Duration, cache_ttl: Duration) -> Self {
        Self {
            client: github_client().expect("loopback GitHub fixture client"),
            origin: Arc::from(origin.trim_end_matches('/')),
            responses: response_cache(cache_ttl),
            remotes: remote_cache(cache_ttl),
            request_timeout,
            remote_timeout: REMOTE_TIMEOUT,
        }
    }

    async fn repo_for_project(
        &self,
        state: &AppState,
        raw_project_id: &str,
    ) -> Result<RepoIdentity, ApiError> {
        let project_id = uuid::Uuid::parse_str(raw_project_id).map_err(|_| {
            ApiError::local(
                StatusCode::BAD_REQUEST,
                "invalid_project_id",
                "Invalid project id",
            )
        })?;
        let project = state
            .runtime
            .find_project(project_id)
            .map_err(|_| ApiError::internal())?
            .ok_or_else(ApiError::not_found)?;
        let root = PathBuf::from(project.workspace_root);
        self.remotes
            .try_get_with(
                root.clone(),
                resolve_workspace_remote(root, self.remote_timeout),
            )
            .await
            .map_err(|error| (*error).clone())
    }

    async fn cached<F, Fut>(
        &self,
        key: CacheKey,
        fetch: F,
    ) -> Result<(CachedResponse, CacheDisposition), ApiError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<CachedResponse, ApiError>>,
    {
        if let Some(value) = self.responses.get(&key).await {
            return Ok((value, CacheDisposition::Hit));
        }
        let value = self
            .responses
            .try_get_with(key, fetch())
            .await
            .map_err(|error| (*error).clone())?;
        Ok((value, CacheDisposition::Miss))
    }

    async fn request<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Upstream<T>, ApiError> {
        let request = self
            .client
            .get(format!("{}{path}", self.origin))
            .header(header::USER_AGENT, USER_AGENT)
            .header(header::ACCEPT, ACCEPT)
            .header("X-GitHub-Api-Version", API_VERSION)
            .query(query);
        let (status, headers, bytes) = timeout(self.request_timeout, async {
            let response = request.send().await.map_err(|error| {
                if error.is_timeout() {
                    ApiError::github_timeout()
                } else {
                    ApiError::github_error()
                }
            })?;
            let status = response.status();
            let headers = response.headers().clone();
            let bytes = read_bounded_response(response).await?;
            Ok::<_, ApiError>((status, headers, bytes))
        })
        .await
        .map_err(|_| ApiError::github_timeout())??;
        let rate = RateLimitMeta::from_headers(&headers);
        if status == StatusCode::NOT_FOUND {
            return Err(ApiError::upstream(
                StatusCode::NOT_FOUND,
                "not_found_or_private",
                "Repository resource not found or private",
                rate,
            ));
        }
        if !status.is_success() {
            return Err(ApiError::upstream(
                StatusCode::BAD_GATEWAY,
                "github_error",
                "GitHub request failed",
                rate,
            ));
        }
        let link = headers
            .get(header::LINK)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let value = serde_json::from_slice(&bytes).map_err(|_| {
            ApiError::upstream(
                StatusCode::BAD_GATEWAY,
                "github_error",
                "GitHub response could not be decoded",
                rate.clone(),
            )
        })?;
        Ok(Upstream { value, rate, link })
    }
}

fn github_client() -> Result<Client, reqwest::Error> {
    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(TOTAL_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

fn response_cache(ttl: Duration) -> Cache<CacheKey, CachedResponse> {
    Cache::builder()
        .max_capacity(CACHE_CAPACITY)
        .time_to_live(ttl)
        .build()
}

fn remote_cache(ttl: Duration) -> Cache<PathBuf, RepoIdentity> {
    Cache::builder()
        .max_capacity(CACHE_CAPACITY)
        .time_to_live(ttl)
        .build()
}

async fn read_bounded_response(mut response: reqwest::Response) -> Result<Vec<u8>, ApiError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        if error.is_timeout() {
            ApiError::github_timeout()
        } else {
            ApiError::github_error()
        }
    })? {
        if bytes.len().saturating_add(chunk.len()) > MAX_UPSTREAM_BYTES {
            return Err(ApiError::response_too_large());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct RepoIdentity {
    owner: String,
    repo: String,
}

async fn resolve_workspace_remote(
    root: PathBuf,
    remote_timeout: Duration,
) -> Result<RepoIdentity, ApiError> {
    let child = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["remote", "get-url", "origin"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| ApiError::no_origin())?;
    let bytes = collect_child_stdout(child, remote_timeout).await?;
    let raw = std::str::from_utf8(&bytes).map_err(|_| ApiError::not_github())?;
    parse_github_remote(raw.trim())
}

async fn collect_child_stdout(
    mut child: tokio::process::Child,
    child_timeout: Duration,
) -> Result<Vec<u8>, ApiError> {
    let result = timeout(child_timeout, async {
        let stdout = child.stdout.as_mut().ok_or_else(ApiError::no_origin)?;
        let bytes = read_remote_stdout(stdout).await?;
        // Read stdout to EOF before checking status so a valid bounded child
        // cannot deadlock on a full pipe and process success remains authoritative.
        let status = child.wait().await.map_err(|_| ApiError::no_origin())?;
        if !status.success() {
            return Err(ApiError::no_origin());
        }
        Ok(bytes)
    })
    .await;

    match result {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(error)) => {
            kill_and_reap(&mut child).await;
            Err(error)
        }
        Err(_) => {
            kill_and_reap(&mut child).await;
            Err(ApiError::no_origin())
        }
    }
}

async fn kill_and_reap(child: &mut tokio::process::Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

async fn read_remote_stdout(reader: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>, ApiError> {
    let mut bytes = Vec::with_capacity(256);
    let mut buffer = [0_u8; 512];
    loop {
        let remaining = MAX_REMOTE_BYTES + 1 - bytes.len();
        let read_capacity = remaining.min(buffer.len());
        let read = reader
            .read(&mut buffer[..read_capacity])
            .await
            .map_err(|_| ApiError::no_origin())?;
        if read == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() > MAX_REMOTE_BYTES {
            return Err(ApiError::not_github());
        }
    }
}

fn parse_github_remote(raw: &str) -> Result<RepoIdentity, ApiError> {
    if raw.is_empty()
        || raw
            .bytes()
            .any(|byte| matches!(byte, b'?' | b'#' | b'%' | b'\\'))
    {
        return Err(ApiError::not_github());
    }
    let path = if let Some(path) = raw.strip_prefix("https://github.com/") {
        if path.contains('@') {
            return Err(ApiError::not_github());
        }
        path
    } else if let Some(path) = raw.strip_prefix("git@github.com:") {
        path
    } else if let Some(path) = raw.strip_prefix("ssh://git@github.com/") {
        path
    } else {
        return Err(ApiError::not_github());
    };
    let mut segments = path.split('/');
    let owner = segments.next().ok_or_else(ApiError::not_github)?;
    let repo = segments.next().ok_or_else(ApiError::not_github)?;
    if segments.next().is_some() {
        return Err(ApiError::not_github());
    }
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    if !valid_owner(owner) || !valid_repo(repo) {
        return Err(ApiError::not_github());
    }
    Ok(RepoIdentity {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
    })
}

fn valid_owner(owner: &str) -> bool {
    !owner.is_empty()
        && owner.len() <= 39
        && owner.is_ascii()
        && owner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && owner
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && owner
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

fn valid_repo(repo: &str) -> bool {
    !repo.is_empty()
        && repo.len() <= 100
        && repo != "."
        && repo != ".."
        && repo.is_ascii()
        && repo
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
enum Endpoint {
    Pulls,
    Pull,
    Checks,
    Reviews,
    Commits,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    endpoint: Endpoint,
    repo_digest: [u8; 32],
    parameters_digest: [u8; 32],
}

impl CacheKey {
    fn new<T: Serialize>(endpoint: Endpoint, repo: &RepoIdentity, parameters: &T) -> Self {
        Self {
            endpoint,
            repo_digest: digest_json(repo),
            parameters_digest: digest_json(parameters),
        }
    }
}

fn digest_json<T: Serialize>(value: &T) -> [u8; 32] {
    let canonical = serde_json::to_vec(value).expect("cache-key inputs are serializable");
    Sha256::digest(canonical).into()
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
struct RateLimitMeta {
    remaining: u64,
    reset: u64,
}

impl RateLimitMeta {
    fn from_headers(headers: &HeaderMap) -> Self {
        Self {
            remaining: parse_rate(headers, &X_UPSTREAM_RATE_REMAINING),
            reset: parse_rate(headers, &X_UPSTREAM_RATE_RESET),
        }
    }
}

fn parse_rate(headers: &HeaderMap, name: &HeaderName) -> u64 {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

struct Upstream<T> {
    value: T,
    rate: RateLimitMeta,
    link: Option<String>,
}

#[derive(Debug, Clone)]
struct CachedResponse {
    bytes: Bytes,
    rate: RateLimitMeta,
    populated_at: Instant,
}

impl CachedResponse {
    fn json<T: Serialize>(value: &T, rate: RateLimitMeta) -> Result<Self, ApiError> {
        let bytes = serde_json::to_vec(value).map_err(|_| ApiError::internal())?;
        Ok(Self {
            bytes: Bytes::from(bytes),
            rate,
            populated_at: Instant::now(),
        })
    }

    fn into_response(self, disposition: CacheDisposition) -> Response {
        let age = self.populated_at.elapsed().as_secs();
        let mut response = Response::new(Body::from(self.bytes));
        *response.status_mut() = StatusCode::OK;
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        insert_contract_headers(
            response.headers_mut(),
            disposition.as_str(),
            age,
            &self.rate,
        );
        response
    }
}

#[derive(Debug, Clone, Copy)]
enum CacheDisposition {
    Hit,
    Miss,
}

impl CacheDisposition {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "HIT",
            Self::Miss => "MISS",
        }
    }
}

fn insert_contract_headers(
    headers: &mut HeaderMap,
    cache: &'static str,
    age: u64,
    rate: &RateLimitMeta,
) {
    headers.insert(X_OCEAN_CACHE, HeaderValue::from_static(cache));
    if let Ok(value) = HeaderValue::from_str(&age.to_string()) {
        headers.insert(X_OCEAN_CACHE_AGE, value);
    }
    if let Ok(value) = HeaderValue::from_str(&rate.remaining.to_string()) {
        headers.insert(X_OUTWARD_RATE_REMAINING, value);
    }
    if let Ok(value) = HeaderValue::from_str(&rate.reset.to_string()) {
        headers.insert(X_OUTWARD_RATE_RESET, value);
    }
}

#[derive(Debug, Clone)]
pub(super) struct ApiError {
    status: StatusCode,
    code: &'static str,
    surface_message: &'static str,
    rate: RateLimitMeta,
}

impl ApiError {
    fn local(status: StatusCode, code: &'static str, surface_message: &'static str) -> Self {
        Self {
            status,
            code,
            surface_message,
            rate: RateLimitMeta::default(),
        }
    }

    fn upstream(
        status: StatusCode,
        code: &'static str,
        surface_message: &'static str,
        rate: RateLimitMeta,
    ) -> Self {
        Self {
            status,
            code,
            surface_message,
            rate,
        }
    }

    fn invalid_request() -> Self {
        Self::local(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Invalid request",
        )
    }

    fn invalid_sha() -> Self {
        Self::local(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_sha",
            "SHA must be a full 40-character hexadecimal value",
        )
    }

    fn not_found() -> Self {
        Self::local(StatusCode::NOT_FOUND, "not_found", "Project not found")
    }

    fn no_origin() -> Self {
        Self::local(
            StatusCode::NOT_FOUND,
            "no_origin_remote",
            "Project has no readable origin remote",
        )
    }

    fn not_github() -> Self {
        Self::local(
            StatusCode::NOT_FOUND,
            "not_github_remote",
            "Project origin is not a supported GitHub remote",
        )
    }

    fn github_error() -> Self {
        Self::local(
            StatusCode::BAD_GATEWAY,
            "github_error",
            "GitHub request failed",
        )
    }

    fn response_too_large() -> Self {
        Self::local(
            StatusCode::BAD_GATEWAY,
            "github_error",
            "GitHub response too large",
        )
    }

    fn github_timeout() -> Self {
        Self::local(
            StatusCode::GATEWAY_TIMEOUT,
            "github_timeout",
            "GitHub request timed out",
        )
    }

    fn internal() -> Self {
        Self::local(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Internal server error",
        )
    }
}

#[derive(Serialize)]
struct ErrorEnvelope {
    ok: bool,
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    surface_message: &'static str,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::to_vec(&ErrorEnvelope {
            ok: false,
            error: ErrorBody {
                code: self.code,
                surface_message: self.surface_message,
            },
        })
        .unwrap_or_else(|_| b"{\"ok\":false,\"error\":{\"code\":\"internal_error\",\"surface_message\":\"Internal server error\"}}".to_vec());
        let mut response = Response::new(Body::from(body));
        *response.status_mut() = self.status;
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        insert_contract_headers(response.headers_mut(), "MISS", 0, &self.rate);
        response
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProjectPath {
    project_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PullPath {
    project_id: String,
    number: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CheckPath {
    project_id: String,
    sha: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PullsQuery {
    state: Option<String>,
    page: Option<String>,
    per_page: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PageQuery {
    page: Option<String>,
    per_page: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CommitsQuery {
    sha: Option<String>,
    page: Option<String>,
    per_page: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ListParameters {
    state: String,
    page: u32,
    per_page: u32,
}

#[derive(Debug, Clone, Serialize)]
struct PageParameters {
    page: u32,
    per_page: u32,
}

#[derive(Debug, Clone, Serialize)]
struct NumberParameters {
    number: u64,
}

#[derive(Debug, Clone, Serialize)]
struct CheckParameters {
    sha: String,
}

#[derive(Debug, Clone, Serialize)]
struct CommitParameters {
    sha: Option<String>,
    page: u32,
    per_page: u32,
}

fn parse_pulls_query(query: PullsQuery) -> Result<ListParameters, ApiError> {
    let state = query.state.unwrap_or_else(|| "open".to_owned());
    if !matches!(state.as_str(), "open" | "closed" | "all") {
        return Err(ApiError::invalid_request());
    }
    let page = positive(query.page, 1)?;
    let per_page = positive(query.per_page, 10)?.min(25);
    Ok(ListParameters {
        state,
        page,
        per_page,
    })
}

fn parse_page_query(query: PageQuery) -> Result<PageParameters, ApiError> {
    Ok(PageParameters {
        page: positive(query.page, 1)?,
        per_page: positive(query.per_page, 10)?.min(25),
    })
}

fn parse_commit_query(query: CommitsQuery) -> Result<CommitParameters, ApiError> {
    if query.sha.as_ref().is_some_and(String::is_empty) {
        return Err(ApiError::invalid_request());
    }
    Ok(CommitParameters {
        sha: query.sha,
        page: positive(query.page, 1)?,
        per_page: positive(query.per_page, 10)?.min(25),
    })
}

fn positive(raw: Option<String>, default: u32) -> Result<u32, ApiError> {
    match raw {
        None => Ok(default),
        Some(value) => value
            .parse::<u32>()
            .ok()
            .filter(|value| *value >= 1)
            .ok_or_else(ApiError::invalid_request),
    }
}

fn parse_number(raw: &str) -> Result<u64, ApiError> {
    raw.parse::<u64>()
        .ok()
        .filter(|number| *number > 0)
        .ok_or_else(ApiError::invalid_request)
}

fn valid_full_sha(sha: &str) -> bool {
    sha.len() == 40 && sha.is_ascii() && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn path_value<T>(path: Result<Path<T>, PathRejection>) -> Result<T, ApiError> {
    path.map(|Path(value)| value)
        .map_err(|_| ApiError::invalid_request())
}

fn query_value<T: Default>(query: Result<Query<T>, QueryRejection>) -> Result<T, ApiError> {
    query
        .map(|Query(value)| value)
        .map_err(|_| ApiError::invalid_request())
}

pub(super) async fn pulls(
    State(state): State<AppState>,
    Extension(service): Extension<GitHubService>,
    path: Result<Path<ProjectPath>, PathRejection>,
    query: Result<Query<PullsQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let path = path_value(path)?;
    let parameters = parse_pulls_query(query_value(query)?)?;
    let repo = service.repo_for_project(&state, &path.project_id).await?;
    let key = CacheKey::new(Endpoint::Pulls, &repo, &parameters);
    let fetch_service = service.clone();
    let fetch_repo = repo.clone();
    let fetch_parameters = parameters.clone();
    let (response, disposition) = service
        .cached(key, move || async move {
            let upstream: Upstream<Vec<UpstreamPull>> = fetch_service
                .request(
                    &format!("/repos/{}/{}/pulls", fetch_repo.owner, fetch_repo.repo),
                    &[
                        ("state", fetch_parameters.state.clone()),
                        ("page", fetch_parameters.page.to_string()),
                        ("per_page", fetch_parameters.per_page.to_string()),
                    ],
                )
                .await?;
            let (pulls, pulls_truncated) = capped_try_map(
                upstream.value,
                fetch_parameters.per_page as usize,
                PullListItem::try_from,
            )?;
            CachedResponse::json(
                &PullListEnvelope {
                    ok: true,
                    pulls,
                    pulls_truncated,
                    page: fetch_parameters.page,
                    per_page: fetch_parameters.per_page,
                    has_more: has_next(upstream.link.as_deref()),
                    rate: upstream.rate.clone(),
                },
                upstream.rate,
            )
        })
        .await?;
    Ok(response.into_response(disposition))
}

pub(super) async fn pull(
    State(state): State<AppState>,
    Extension(service): Extension<GitHubService>,
    path: Result<Path<PullPath>, PathRejection>,
) -> Result<Response, ApiError> {
    let path = path_value(path)?;
    let number = parse_number(&path.number)?;
    let repo = service.repo_for_project(&state, &path.project_id).await?;
    let parameters = NumberParameters { number };
    let key = CacheKey::new(Endpoint::Pull, &repo, &parameters);
    let fetch_service = service.clone();
    let fetch_repo = repo.clone();
    let (response, disposition) = service
        .cached(key, move || async move {
            let upstream: Upstream<UpstreamPull> = fetch_service
                .request(
                    &format!(
                        "/repos/{}/{}/pulls/{number}",
                        fetch_repo.owner, fetch_repo.repo
                    ),
                    &[],
                )
                .await?;
            CachedResponse::json(
                &PullDetailEnvelope {
                    ok: true,
                    pull: PullDetail::try_from(upstream.value)?,
                    rate: upstream.rate.clone(),
                },
                upstream.rate,
            )
        })
        .await?;
    Ok(response.into_response(disposition))
}

pub(super) async fn checks(
    State(state): State<AppState>,
    Extension(service): Extension<GitHubService>,
    path: Result<Path<CheckPath>, PathRejection>,
) -> Result<Response, ApiError> {
    let path = path_value(path)?;
    if !valid_full_sha(&path.sha) {
        return Err(ApiError::invalid_sha());
    }
    let repo = service.repo_for_project(&state, &path.project_id).await?;
    let parameters = CheckParameters {
        sha: path.sha.clone(),
    };
    let key = CacheKey::new(Endpoint::Checks, &repo, &parameters);
    let fetch_service = service.clone();
    let fetch_repo = repo.clone();
    let sha = path.sha;
    let (response, disposition) = service
        .cached(key, move || async move {
            let upstream: Upstream<UpstreamChecks> = fetch_service
                .request(
                    &format!(
                        "/repos/{}/{}/commits/{sha}/check-runs",
                        fetch_repo.owner, fetch_repo.repo
                    ),
                    &[("per_page", "100".to_owned())],
                )
                .await?;
            let (checks, checks_truncated) =
                capped_map(upstream.value.check_runs, 100, CheckRun::from);
            CachedResponse::json(
                &ChecksEnvelope {
                    ok: true,
                    checks,
                    checks_truncated,
                    rate: upstream.rate.clone(),
                },
                upstream.rate,
            )
        })
        .await?;
    Ok(response.into_response(disposition))
}

pub(super) async fn reviews(
    State(state): State<AppState>,
    Extension(service): Extension<GitHubService>,
    path: Result<Path<PullPath>, PathRejection>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let path = path_value(path)?;
    let number = parse_number(&path.number)?;
    let page = parse_page_query(query_value(query)?)?;
    let repo = service.repo_for_project(&state, &path.project_id).await?;
    let parameters = (NumberParameters { number }, page.clone());
    let key = CacheKey::new(Endpoint::Reviews, &repo, &parameters);
    let fetch_service = service.clone();
    let fetch_repo = repo.clone();
    let (response, disposition) = service
        .cached(key, move || async move {
            let upstream: Upstream<Vec<UpstreamReview>> = fetch_service
                .request(
                    &format!(
                        "/repos/{}/{}/pulls/{number}/reviews",
                        fetch_repo.owner, fetch_repo.repo
                    ),
                    &[
                        ("page", page.page.to_string()),
                        ("per_page", page.per_page.to_string()),
                    ],
                )
                .await?;
            let (reviews, reviews_truncated) =
                capped_map(upstream.value, page.per_page as usize, Review::from);
            CachedResponse::json(
                &ReviewsEnvelope {
                    ok: true,
                    reviews,
                    reviews_truncated,
                    page: page.page,
                    per_page: page.per_page,
                    has_more: has_next(upstream.link.as_deref()),
                    rate: upstream.rate.clone(),
                },
                upstream.rate,
            )
        })
        .await?;
    Ok(response.into_response(disposition))
}

pub(super) async fn commits(
    State(state): State<AppState>,
    Extension(service): Extension<GitHubService>,
    path: Result<Path<ProjectPath>, PathRejection>,
    query: Result<Query<CommitsQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let path = path_value(path)?;
    let parameters = parse_commit_query(query_value(query)?)?;
    let repo = service.repo_for_project(&state, &path.project_id).await?;
    let key = CacheKey::new(Endpoint::Commits, &repo, &parameters);
    let fetch_service = service.clone();
    let fetch_repo = repo.clone();
    let fetch_parameters = parameters.clone();
    let (response, disposition) = service
        .cached(key, move || async move {
            let mut query = Vec::with_capacity(3);
            if let Some(sha) = &fetch_parameters.sha {
                query.push(("sha", sha.clone()));
            }
            query.push(("page", fetch_parameters.page.to_string()));
            query.push(("per_page", fetch_parameters.per_page.to_string()));
            let upstream: Upstream<Vec<UpstreamCommit>> = fetch_service
                .request(
                    &format!("/repos/{}/{}/commits", fetch_repo.owner, fetch_repo.repo),
                    &query,
                )
                .await?;
            let commits_truncated = upstream.value.len() > fetch_parameters.per_page as usize;
            let commits = upstream
                .value
                .into_iter()
                .take(fetch_parameters.per_page as usize)
                .map(Commit::try_from)
                .collect::<Result<Vec<_>, _>>()?;
            CachedResponse::json(
                &CommitsEnvelope {
                    ok: true,
                    commits,
                    commits_truncated,
                    page: fetch_parameters.page,
                    per_page: fetch_parameters.per_page,
                    has_more: has_next(upstream.link.as_deref()),
                    rate: upstream.rate.clone(),
                },
                upstream.rate,
            )
        })
        .await?;
    Ok(response.into_response(disposition))
}

fn has_next(link: Option<&str>) -> bool {
    link.is_some_and(|header| {
        header.split(',').any(|link_value| {
            link_value.split(';').skip(1).any(|parameter| {
                parameter
                    .trim()
                    .strip_prefix("rel=")
                    .and_then(|value| value.strip_prefix('"')?.strip_suffix('"'))
                    .is_some_and(|relations| {
                        relations.split_ascii_whitespace().any(|rel| rel == "next")
                    })
            })
        })
    })
}

fn truncate_utf8(value: String, cap: usize) -> (String, bool) {
    if value.len() <= cap {
        return (value, false);
    }
    let mut boundary = cap;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let mut truncated = value[..boundary].to_owned();
    truncated.push('…');
    (truncated, true)
}

fn capped_map<T, U>(values: Vec<T>, cap: usize, map: impl FnMut(T) -> U) -> (Vec<U>, bool) {
    let truncated = values.len() > cap;
    (values.into_iter().take(cap).map(map).collect(), truncated)
}

fn capped_try_map<T, U>(
    values: Vec<T>,
    cap: usize,
    map: impl FnMut(T) -> Result<U, ApiError>,
) -> Result<(Vec<U>, bool), ApiError> {
    let truncated = values.len() > cap;
    let values = values
        .into_iter()
        .take(cap)
        .map(map)
        .collect::<Result<Vec<_>, _>>()?;
    Ok((values, truncated))
}

#[derive(Debug, Deserialize)]
struct UpstreamUser {
    login: String,
}

#[derive(Debug, Deserialize)]
struct UpstreamBranch {
    #[serde(rename = "ref")]
    branch: String,
    sha: String,
}

#[derive(Debug, Deserialize)]
struct UpstreamLabel {
    name: String,
    color: String,
}

#[derive(Debug, Deserialize)]
struct UpstreamPull {
    number: u64,
    title: String,
    user: Option<UpstreamUser>,
    head: UpstreamBranch,
    base: UpstreamBranch,
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    labels: Vec<UpstreamLabel>,
    state: String,
    mergeable: Option<bool>,
    #[serde(default)]
    requested_reviewers: Vec<UpstreamUser>,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Serialize)]
struct Label {
    name: String,
    color: String,
    #[serde(skip_serializing_if = "is_false")]
    name_truncated: bool,
}

impl From<UpstreamLabel> for Label {
    fn from(value: UpstreamLabel) -> Self {
        let (name, name_truncated) = truncate_utf8(value.name, 50);
        Self {
            name,
            color: value.color,
            name_truncated,
        }
    }
}

#[derive(Debug, Serialize)]
struct PullListItem {
    number: u64,
    title: String,
    #[serde(skip_serializing_if = "is_false")]
    title_truncated: bool,
    author: String,
    #[serde(skip_serializing_if = "is_false")]
    author_truncated: bool,
    branch: String,
    base_branch: String,
    head_sha: String,
    draft: bool,
    labels: Vec<Label>,
    #[serde(skip_serializing_if = "is_false")]
    labels_truncated: bool,
    state: String,
    created_at: String,
    updated_at: String,
}

impl TryFrom<UpstreamPull> for PullListItem {
    type Error = ApiError;

    fn try_from(value: UpstreamPull) -> Result<Self, Self::Error> {
        if !valid_full_sha(&value.head.sha) {
            return Err(ApiError::github_error());
        }
        let (title, title_truncated) = truncate_utf8(value.title, 256);
        let (author, author_truncated) =
            truncate_utf8(value.user.map(|user| user.login).unwrap_or_default(), 39);
        let (labels, labels_truncated) = capped_map(value.labels, 100, Label::from);
        Ok(Self {
            number: value.number,
            title,
            title_truncated,
            author,
            author_truncated,
            branch: value.head.branch,
            base_branch: value.base.branch,
            head_sha: value.head.sha,
            draft: value.draft,
            labels,
            labels_truncated,
            state: value.state,
            created_at: value.created_at,
            updated_at: value.updated_at,
        })
    }
}

#[derive(Debug, Serialize)]
struct PullDetail {
    number: u64,
    title: String,
    #[serde(skip_serializing_if = "is_false")]
    title_truncated: bool,
    author: String,
    #[serde(skip_serializing_if = "is_false")]
    author_truncated: bool,
    branch: String,
    base_branch: String,
    head_sha: String,
    body: String,
    body_truncated: bool,
    draft: bool,
    labels: Vec<Label>,
    #[serde(skip_serializing_if = "is_false")]
    labels_truncated: bool,
    state: String,
    mergeable: Option<bool>,
    requested_reviewers: Vec<String>,
    #[serde(skip_serializing_if = "is_false")]
    requested_reviewers_truncated: bool,
    created_at: String,
    updated_at: String,
}

impl TryFrom<UpstreamPull> for PullDetail {
    type Error = ApiError;

    fn try_from(value: UpstreamPull) -> Result<Self, Self::Error> {
        if !valid_full_sha(&value.head.sha) {
            return Err(ApiError::github_error());
        }
        let (title, title_truncated) = truncate_utf8(value.title, 256);
        let (author, author_truncated) =
            truncate_utf8(value.user.map(|user| user.login).unwrap_or_default(), 39);
        let (body, body_truncated) = truncate_utf8(value.body.unwrap_or_default(), 65_536);
        let (labels, labels_truncated) = capped_map(value.labels, 100, Label::from);
        let reviewer_vector_truncated = value.requested_reviewers.len() > 100;
        let mut reviewer_scalar_truncated = false;
        let requested_reviewers = value
            .requested_reviewers
            .into_iter()
            .take(100)
            .map(|reviewer| {
                let (login, truncated) = truncate_utf8(reviewer.login, 39);
                reviewer_scalar_truncated |= truncated;
                login
            })
            .collect();
        Ok(Self {
            number: value.number,
            title,
            title_truncated,
            author,
            author_truncated,
            branch: value.head.branch,
            base_branch: value.base.branch,
            head_sha: value.head.sha,
            body,
            body_truncated,
            draft: value.draft,
            labels,
            labels_truncated,
            state: value.state,
            mergeable: value.mergeable,
            requested_reviewers,
            requested_reviewers_truncated: reviewer_vector_truncated || reviewer_scalar_truncated,
            created_at: value.created_at,
            updated_at: value.updated_at,
        })
    }
}

#[derive(Debug, Deserialize)]
struct UpstreamChecks {
    #[serde(default)]
    check_runs: Vec<UpstreamCheck>,
}

#[derive(Debug, Deserialize)]
struct UpstreamCheck {
    id: u64,
    name: String,
    status: String,
    conclusion: Option<String>,
    details_url: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct CheckRun {
    id: u64,
    name: String,
    #[serde(skip_serializing_if = "is_false")]
    name_truncated: bool,
    status: String,
    conclusion: Option<String>,
    details_url: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
}

impl From<UpstreamCheck> for CheckRun {
    fn from(value: UpstreamCheck) -> Self {
        let (name, name_truncated) = truncate_utf8(value.name, 128);
        Self {
            id: value.id,
            name,
            name_truncated,
            status: value.status,
            conclusion: value.conclusion,
            details_url: value.details_url,
            started_at: value.started_at,
            completed_at: value.completed_at,
        }
    }
}

#[derive(Debug, Deserialize)]
struct UpstreamReview {
    user: Option<UpstreamUser>,
    state: String,
    body: Option<String>,
    submitted_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct Review {
    reviewer: String,
    #[serde(skip_serializing_if = "is_false")]
    reviewer_truncated: bool,
    state: String,
    body: String,
    body_truncated: bool,
    submitted_at: Option<String>,
}

impl From<UpstreamReview> for Review {
    fn from(value: UpstreamReview) -> Self {
        let (reviewer, reviewer_truncated) =
            truncate_utf8(value.user.map(|user| user.login).unwrap_or_default(), 39);
        let (body, body_truncated) = truncate_utf8(value.body.unwrap_or_default(), 16_384);
        Self {
            reviewer,
            reviewer_truncated,
            state: value.state.to_ascii_lowercase(),
            body,
            body_truncated,
            submitted_at: value.submitted_at,
        }
    }
}

#[derive(Debug, Deserialize)]
struct UpstreamCommit {
    sha: String,
    commit: UpstreamCommitDetail,
    author: Option<UpstreamUser>,
}

#[derive(Debug, Deserialize)]
struct UpstreamCommitDetail {
    message: String,
    author: Option<UpstreamCommitAuthor>,
}

#[derive(Debug, Deserialize)]
struct UpstreamCommitAuthor {
    name: String,
    date: String,
}

#[derive(Debug, Serialize)]
struct Commit {
    sha_short: String,
    sha: String,
    message: String,
    #[serde(skip_serializing_if = "is_false")]
    message_truncated: bool,
    author: String,
    #[serde(skip_serializing_if = "is_false")]
    author_truncated: bool,
    date: String,
}

impl TryFrom<UpstreamCommit> for Commit {
    type Error = ApiError;

    fn try_from(value: UpstreamCommit) -> Result<Self, Self::Error> {
        if !valid_full_sha(&value.sha) {
            return Err(ApiError::github_error());
        }
        let (message, message_truncated) = truncate_utf8(value.commit.message, 512);
        let fallback_author = value
            .commit
            .author
            .as_ref()
            .map(|author| author.name.clone());
        let (author, author_truncated) = truncate_utf8(
            value
                .author
                .map(|author| author.login)
                .or(fallback_author)
                .unwrap_or_default(),
            39,
        );
        let date = value
            .commit
            .author
            .map(|author| author.date)
            .unwrap_or_default();
        Ok(Self {
            sha_short: value.sha[..7].to_owned(),
            sha: value.sha,
            message,
            message_truncated,
            author,
            author_truncated,
            date,
        })
    }
}

const fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Serialize)]
struct PullListEnvelope {
    ok: bool,
    pulls: Vec<PullListItem>,
    #[serde(skip_serializing_if = "is_false")]
    pulls_truncated: bool,
    page: u32,
    per_page: u32,
    has_more: bool,
    rate: RateLimitMeta,
}

#[derive(Serialize)]
struct PullDetailEnvelope {
    ok: bool,
    pull: PullDetail,
    rate: RateLimitMeta,
}

#[derive(Serialize)]
struct ChecksEnvelope {
    ok: bool,
    checks: Vec<CheckRun>,
    #[serde(skip_serializing_if = "is_false")]
    checks_truncated: bool,
    rate: RateLimitMeta,
}

#[derive(Serialize)]
struct ReviewsEnvelope {
    ok: bool,
    reviews: Vec<Review>,
    #[serde(skip_serializing_if = "is_false")]
    reviews_truncated: bool,
    page: u32,
    per_page: u32,
    has_more: bool,
    rate: RateLimitMeta,
}

#[derive(Serialize)]
struct CommitsEnvelope {
    ok: bool,
    commits: Vec<Commit>,
    #[serde(skip_serializing_if = "is_false")]
    commits_truncated: bool,
    page: u32,
    per_page: u32,
    has_more: bool,
    rate: RateLimitMeta,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request, routing::any, Router};
    use http_body_util::BodyExt;
    use ocean_core::{Project, ProjectConfig};
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    #[test]
    fn exact_remote_forms_and_attack_rejections() {
        let expected = RepoIdentity {
            owner: "Risingtides-dev".into(),
            repo: "ocean_os.v1".into(),
        };
        for remote in [
            "https://github.com/Risingtides-dev/ocean_os.v1.git",
            "git@github.com:Risingtides-dev/ocean_os.v1.git",
            "ssh://git@github.com/Risingtides-dev/ocean_os.v1.git",
        ] {
            assert_eq!(parse_github_remote(remote).unwrap(), expected);
        }
        for remote in [
            "https://token@github.com/o/r",
            "https://github.com.evil/o/r",
            "https://github.com:443/o/r",
            "https://github.com/o/r?x=1",
            "https://github.com/o/r#frag",
            "https://github.com/o/%72",
            "https://github.com/o/../r",
            "https://github.com/o/r/extra",
            "git@gitlab.com:o/r",
            "ssh://root@github.com/o/r",
            "https://github.com/-owner/r",
            "https://github.com/o/..",
            "https://github.com/o/",
        ] {
            assert!(parse_github_remote(remote).is_err(), "accepted {remote}");
        }
    }

    #[tokio::test]
    async fn remote_stdout_is_incrementally_bounded() {
        let mut normal: &[u8] = b"git@github.com:o/r.git\n";
        assert_eq!(read_remote_stdout(&mut normal).await.unwrap().len(), 23);
        let oversized = vec![b'x'; MAX_REMOTE_BYTES + 1];
        let mut reader = oversized.as_slice();
        let error = read_remote_stdout(&mut reader).await.unwrap_err();
        assert_eq!(error.code, "not_github_remote");
    }

    #[cfg(unix)]
    fn assert_process_reaped(pid: u32) {
        let status = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "pid="])
            .status()
            .expect("ps is available on supported Unix");
        assert!(!status.success(), "child {pid} is still live or unreaped");
    }

    #[cfg(unix)]
    fn test_child(script: &str) -> (tokio::process::Child, u32) {
        let child = Command::new("sh")
            .args(["-c", script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        (child, pid)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_and_reaps_real_child() {
        let (child, pid) = test_child("sleep 30");
        let error = collect_child_stdout(child, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert_eq!(error.code, "no_origin_remote");
        assert_process_reaped(pid);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn oversized_producer_is_killed_and_reaped() {
        let (child, pid) = test_child("while :; do printf 0123456789abcdef; done");
        let error = collect_child_stdout(child, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert_eq!(error.code, "not_github_remote");
        assert_process_reaped(pid);
    }

    #[test]
    fn resolver_source_characterizes_safe_child_io() {
        let production = include_str!("github.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        assert!(production.contains(".kill_on_drop(true)"));
        assert!(production.contains("Stdio::piped()"));
        assert!(!production.contains("Command::output"));
        assert!(!production.contains("wait_with_output"));
        assert!(!production.contains("read_to_end"));
    }

    #[test]
    fn utf8_caps_are_bytes_and_append_ellipsis_only_when_truncated() {
        for cap in [39, 50, 128, 256, 512, 16_384, 65_536] {
            let below = "a".repeat(cap - 1);
            assert_eq!(truncate_utf8(below.clone(), cap), (below, false));
            let at = "a".repeat(cap);
            assert_eq!(truncate_utf8(at.clone(), cap), (at, false));
            let over = format!("{}é", "a".repeat(cap - 1));
            let (bounded, truncated) = truncate_utf8(over, cap);
            assert!(truncated);
            assert_eq!(bounded, format!("{}…", "a".repeat(cap - 1)));
        }
    }

    #[test]
    fn every_frozen_scalar_and_vector_cap_sets_only_its_owned_flag() {
        let mut raw_pull = pull_json();
        raw_pull["title"] = json!("t".repeat(257));
        raw_pull["user"]["login"] = json!("a".repeat(40));
        raw_pull["body"] = json!("b".repeat(65_537));
        raw_pull["labels"] = Value::Array(
            (0..101)
                .map(|index| {
                    json!({"name": if index == 0 { "l".repeat(51) } else { "label".into() }, "color":"0e8a16"})
                })
                .collect(),
        );
        raw_pull["requested_reviewers"] = Value::Array(
            (0..101)
                .map(|index| json!({"login": if index == 0 { "r".repeat(40) } else { "reviewer".into() }}))
                .collect(),
        );
        let detail =
            PullDetail::try_from(serde_json::from_value::<UpstreamPull>(raw_pull).unwrap())
                .unwrap();
        let detail = serde_json::to_value(detail).unwrap();
        assert_eq!(detail["title_truncated"], true);
        assert_eq!(detail["author_truncated"], true);
        assert_eq!(detail["body_truncated"], true);
        assert_eq!(detail["labels_truncated"], true);
        assert_eq!(detail["labels"].as_array().unwrap().len(), 100);
        assert_eq!(detail["labels"][0]["name_truncated"], true);
        assert_eq!(detail["requested_reviewers_truncated"], true);
        assert_eq!(detail["requested_reviewers"].as_array().unwrap().len(), 100);

        let review = Review::from(UpstreamReview {
            user: Some(UpstreamUser {
                login: "r".repeat(40),
            }),
            state: "APPROVED".into(),
            body: Some("b".repeat(16_385)),
            submitted_at: None,
        });
        let review = serde_json::to_value(review).unwrap();
        assert_eq!(review["reviewer_truncated"], true);
        assert_eq!(review["body_truncated"], true);

        let check = CheckRun::from(UpstreamCheck {
            id: 1,
            name: "c".repeat(129),
            status: "completed".into(),
            conclusion: None,
            details_url: None,
            started_at: None,
            completed_at: None,
        });
        assert_eq!(serde_json::to_value(check).unwrap()["name_truncated"], true);

        let commit = Commit::try_from(UpstreamCommit {
            sha: "a".repeat(40),
            commit: UpstreamCommitDetail {
                message: "m".repeat(513),
                author: None,
            },
            author: Some(UpstreamUser {
                login: "u".repeat(40),
            }),
        })
        .unwrap();
        let commit = serde_json::to_value(commit).unwrap();
        assert_eq!(commit["message_truncated"], true);
        assert_eq!(commit["author_truncated"], true);

        let (checks, checks_truncated) = capped_map(vec![(); 101], 100, |_| ());
        assert_eq!(checks.len(), 100);
        assert!(checks_truncated);
        let pulls = (0..26)
            .map(|_| serde_json::from_value::<UpstreamPull>(pull_json()).unwrap())
            .collect();
        let (pulls, pulls_truncated) = capped_try_map(pulls, 25, PullListItem::try_from).unwrap();
        assert_eq!(pulls.len(), 25);
        assert!(pulls_truncated);
    }

    #[test]
    fn link_next_requires_exact_relation_token() {
        assert!(has_next(Some(
            "<https://api.github.com/x?page=2>; rel=\"next\""
        )));
        assert!(has_next(Some("<x>; rel=\"prev next\", <y>; rel=\"last\"")));
        assert!(!has_next(Some("<x>; rel=\"next-page\"")));
        assert!(!has_next(Some("<x>; title=\"next\"")));
        assert!(!has_next(None));
    }

    #[test]
    fn cache_keys_use_full_sha256_and_all_parameters() {
        let repo = RepoIdentity {
            owner: "o".into(),
            repo: "r".into(),
        };
        let a = CacheKey::new(
            Endpoint::Pulls,
            &repo,
            &ListParameters {
                state: "open".into(),
                page: 1,
                per_page: 10,
            },
        );
        let b = CacheKey::new(
            Endpoint::Pulls,
            &repo,
            &ListParameters {
                state: "closed".into(),
                page: 1,
                per_page: 10,
            },
        );
        let other_repo = CacheKey::new(
            Endpoint::Pulls,
            &RepoIdentity {
                owner: "other".into(),
                repo: "r".into(),
            },
            &ListParameters {
                state: "open".into(),
                page: 1,
                per_page: 10,
            },
        );
        let page = CacheKey::new(
            Endpoint::Pulls,
            &repo,
            &ListParameters {
                state: "open".into(),
                page: 2,
                per_page: 10,
            },
        );
        let per_page = CacheKey::new(
            Endpoint::Pulls,
            &repo,
            &ListParameters {
                state: "open".into(),
                page: 1,
                per_page: 11,
            },
        );
        let number = CacheKey::new(Endpoint::Pull, &repo, &NumberParameters { number: 42 });
        let other_number = CacheKey::new(Endpoint::Pull, &repo, &NumberParameters { number: 43 });
        let sha = CacheKey::new(
            Endpoint::Checks,
            &repo,
            &CheckParameters {
                sha: "a".repeat(40),
            },
        );
        let other_sha = CacheKey::new(
            Endpoint::Checks,
            &repo,
            &CheckParameters {
                sha: "b".repeat(40),
            },
        );
        for distinct in [
            &b,
            &other_repo,
            &page,
            &per_page,
            &number,
            &other_number,
            &sha,
            &other_sha,
        ] {
            assert_ne!(&a, distinct);
        }
        assert_ne!(number, other_number);
        assert_ne!(sha, other_sha);
        assert_eq!(a.repo_digest.len(), 32);
        assert_eq!(a.parameters_digest.len(), 32);
    }

    type FixtureHandler = dyn Fn(&str) -> (StatusCode, Vec<u8>, Option<&'static str>) + Send + Sync;

    struct Fixture {
        origin: String,
        calls: Arc<AtomicUsize>,
        seen: Arc<std::sync::Mutex<Vec<(String, HeaderMap)>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn fixture(handler: Arc<FixtureHandler>) -> Fixture {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let fixture_calls = calls.clone();
        let fixture_seen = seen.clone();
        let app = Router::new().fallback(any(move |request: Request<Body>| {
            let handler = handler.clone();
            let calls = fixture_calls.clone();
            let seen = fixture_seen.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let uri = request.uri().to_string();
                seen.lock()
                    .unwrap()
                    .push((uri.clone(), request.headers().clone()));
                let (status, body, link) = handler(&uri);
                let mut response = Response::new(Body::from(body));
                *response.status_mut() = status;
                response
                    .headers_mut()
                    .insert(X_UPSTREAM_RATE_REMAINING, HeaderValue::from_static("57"));
                response.headers_mut().insert(
                    X_UPSTREAM_RATE_RESET,
                    HeaderValue::from_static("1720000000"),
                );
                if let Some(link) = link {
                    response
                        .headers_mut()
                        .insert(header::LINK, HeaderValue::from_static(link));
                }
                response
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Fixture {
            origin: format!("http://{address}"),
            calls,
            seen,
            task,
        }
    }

    async fn delayed_fixture(delay: Duration, body: Vec<u8>) -> Fixture {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let fixture_calls = calls.clone();
        let fixture_seen = seen.clone();
        let app = Router::new().fallback(any(move |request: Request<Body>| {
            let calls = fixture_calls.clone();
            let seen = fixture_seen.clone();
            let body = body.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                seen.lock()
                    .unwrap()
                    .push((request.uri().to_string(), request.headers().clone()));
                tokio::time::sleep(delay).await;
                let mut response = Response::new(Body::from(body));
                *response.status_mut() = StatusCode::OK;
                response
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Fixture {
            origin: format!("http://{address}"),
            calls,
            seen,
            task,
        }
    }

    fn init_git_project(state: &AppState, root: &std::path::Path) -> uuid::Uuid {
        std::fs::create_dir_all(root).unwrap();
        assert!(std::process::Command::new("git")
            .args(["-C", root.to_str().unwrap(), "init", "-q"])
            .status()
            .unwrap()
            .success());
        assert!(std::process::Command::new("git")
            .args([
                "-C",
                root.to_str().unwrap(),
                "remote",
                "add",
                "origin",
                "https://github.com/Risingtides-dev/ocean-os.git",
            ])
            .status()
            .unwrap()
            .success());
        let id = uuid::Uuid::new_v4();
        state
            .runtime
            .upsert_project(
                Project {
                    id,
                    name: "GitHub fixture".into(),
                    workspace_root: root.to_string_lossy().into_owned(),
                    config: ProjectConfig::default(),
                    created_ms: 1,
                    updated_ms: 1,
                },
                1,
            )
            .unwrap();
        id
    }

    #[tokio::test]
    async fn project_remote_resolution_maps_failures_and_caches_identity() {
        let temp = tempfile::tempdir().unwrap();
        let state = crate::tests::fake_convene_state(&temp);
        let root = temp.path().join("workspace");
        let project_id = init_git_project(&state, &root);
        let service = GitHubService::with_origin("http://127.0.0.1:1");
        let repo = service
            .repo_for_project(&state, &project_id.to_string())
            .await
            .unwrap();
        assert_eq!(repo.owner, "Risingtides-dev");
        assert!(std::process::Command::new("git")
            .args([
                "-C",
                root.to_str().unwrap(),
                "remote",
                "set-url",
                "origin",
                "https://gitlab.com/hostile/remote.git",
            ])
            .status()
            .unwrap()
            .success());
        assert_eq!(
            service
                .repo_for_project(&state, &project_id.to_string())
                .await
                .unwrap(),
            repo
        );
        let uncached = GitHubService::with_origin("http://127.0.0.1:1");
        assert_eq!(
            uncached
                .repo_for_project(&state, &project_id.to_string())
                .await
                .unwrap_err()
                .code,
            "not_github_remote"
        );
        let no_origin_root = temp.path().join("no-origin");
        std::fs::create_dir_all(&no_origin_root).unwrap();
        assert!(std::process::Command::new("git")
            .args(["-C", no_origin_root.to_str().unwrap(), "init", "-q"])
            .status()
            .unwrap()
            .success());
        let no_origin_id = uuid::Uuid::new_v4();
        state
            .runtime
            .upsert_project(
                Project {
                    id: no_origin_id,
                    name: "No origin".into(),
                    workspace_root: no_origin_root.to_string_lossy().into_owned(),
                    config: ProjectConfig::default(),
                    created_ms: 1,
                    updated_ms: 1,
                },
                1,
            )
            .unwrap();
        assert_eq!(
            service
                .repo_for_project(&state, &no_origin_id.to_string())
                .await
                .unwrap_err()
                .code,
            "no_origin_remote"
        );
        assert_eq!(
            service
                .repo_for_project(&state, &uuid::Uuid::new_v4().to_string())
                .await
                .unwrap_err()
                .code,
            "not_found"
        );
    }

    fn github_router(state: AppState, service: GitHubService) -> Router {
        crate::app_router(crate::cors_layer(Vec::new()))
            .with_state(state)
            .layer(Extension(service))
    }

    async fn request_json(app: &Router, uri: &str) -> (StatusCode, HeaderMap, Value) {
        request_json_request(app, Request::get(uri).body(Body::empty()).unwrap()).await
    }

    async fn request_json_request(
        app: &Router,
        request: Request<Body>,
    ) -> (StatusCode, HeaderMap, Value) {
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, headers, serde_json::from_slice(&bytes).unwrap())
    }

    fn pull_json() -> Value {
        json!({
            "number": 42,
            "title": "feat(desktop): B0 — Open Externally",
            "user": {"login":"Risingtides-dev"},
            "head": {"ref":"feat/b0-open-externally","sha":"a7a4883f4cbb81130123456789abcdef01234567"},
            "base": {"ref":"main","sha":"1111111111111111111111111111111111111111"},
            "body": "## Summary\n\n...",
            "draft": false,
            "labels": [{"name":"desktop","color":"0e8a16"}],
            "state":"open",
            "mergeable": null,
            "requested_reviewers":[{"login":"fable"},{"login":"codex"}],
            "created_at":"2026-07-17T07:55:00Z",
            "updated_at":"2026-07-17T08:04:00Z"
        })
    }

    #[tokio::test]
    async fn all_success_shapes_headers_queries_and_cache_are_frozen() {
        let handler = Arc::new(|uri: &str| {
            let body = if uri.contains("check-runs") {
                json!({"check_runs":[{"id":12345,"name":"cargo build (wasm)","status":"completed","conclusion":"success","details_url":"https://github.com/x/actions/runs/1","started_at":"2026-07-17T07:56:00Z","completed_at":"2026-07-17T07:58:30Z"}]})
            } else if uri.contains("/reviews") {
                json!([{"user":{"login":"fable"},"state":"APPROVED","body":"LGTM, ship it.","submitted_at":"2026-07-17T08:02:00Z"}])
            } else if uri.contains("/commits?") {
                json!([{"sha":"cbb8113f4a7a48830123456789abcdef01234567","commit":{"message":"fix(surface): fit the landing hero wordmark...","author":{"name":"Fallback","date":"2026-07-17T07:00:00Z"}},"author":{"login":"Risingtides-dev"}}])
            } else if uri.contains("/pulls/42") {
                pull_json()
            } else {
                json!([pull_json()])
            };
            (
                StatusCode::OK,
                serde_json::to_vec(&body).unwrap(),
                Some("<x?page=2>; rel=\"next\""),
            )
        });
        let upstream = fixture(handler).await;
        let temp = tempfile::tempdir().unwrap();
        let state = crate::tests::fake_convene_state(&temp);
        let project_id = init_git_project(&state, &temp.path().join("workspace"));
        let app = github_router(state, GitHubService::with_origin(&upstream.origin));
        let sha = "a7a4883f4cbb81130123456789abcdef01234567";

        let pulls_uri = format!("/v1/repo/github/{project_id}/pulls?state=open&page=1&per_page=99");
        let hostile_request = Request::get(&pulls_uri)
            .header(header::AUTHORIZATION, "Bearer caller-secret")
            .header("x-hostile-caller", "must-not-forward")
            .body(Body::empty())
            .unwrap();
        let (status, headers, pulls_body) = request_json_request(&app, hostile_request).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[&X_OCEAN_CACHE], "MISS");
        assert_eq!(headers[&X_OCEAN_CACHE_AGE], "0");
        assert_eq!(headers[&X_OUTWARD_RATE_REMAINING], "57");
        assert_eq!(headers[&X_OUTWARD_RATE_RESET], "1720000000");
        assert_eq!(
            pulls_body,
            json!({
                "ok":true,"pulls":[{"number":42,"title":"feat(desktop): B0 — Open Externally","author":"Risingtides-dev","branch":"feat/b0-open-externally","base_branch":"main","head_sha":sha,"draft":false,"labels":[{"name":"desktop","color":"0e8a16"}],"state":"open","created_at":"2026-07-17T07:55:00Z","updated_at":"2026-07-17T08:04:00Z"}],
                "page":1,"per_page":25,"has_more":true,"rate":{"remaining":57,"reset":1720000000}
            })
        );
        let (_, hit_headers, hit_body) = request_json(&app, &pulls_uri).await;
        assert_eq!(hit_headers[&X_OCEAN_CACHE], "HIT");
        assert_eq!(hit_body, pulls_body);

        let (_, _, detail) =
            request_json(&app, &format!("/v1/repo/github/{project_id}/pulls/42")).await;
        assert_eq!(
            detail,
            json!({"ok":true,"pull":{"number":42,"title":"feat(desktop): B0 — Open Externally","author":"Risingtides-dev","branch":"feat/b0-open-externally","base_branch":"main","head_sha":sha,"body":"## Summary\n\n...","body_truncated":false,"draft":false,"labels":[{"name":"desktop","color":"0e8a16"}],"state":"open","mergeable":null,"requested_reviewers":["fable","codex"],"created_at":"2026-07-17T07:55:00Z","updated_at":"2026-07-17T08:04:00Z"},"rate":{"remaining":57,"reset":1720000000}})
        );

        let (_, _, checks_body) = request_json(
            &app,
            &format!("/v1/repo/github/{project_id}/head-sha/{sha}/checks"),
        )
        .await;
        assert_eq!(
            checks_body,
            json!({"ok":true,"checks":[{"id":12345,"name":"cargo build (wasm)","status":"completed","conclusion":"success","details_url":"https://github.com/x/actions/runs/1","started_at":"2026-07-17T07:56:00Z","completed_at":"2026-07-17T07:58:30Z"}],"rate":{"remaining":57,"reset":1720000000}})
        );
        let (_, _, reviews_body) = request_json(
            &app,
            &format!("/v1/repo/github/{project_id}/pulls/42/reviews?page=2&per_page=10"),
        )
        .await;
        assert_eq!(
            reviews_body,
            json!({"ok":true,"reviews":[{"reviewer":"fable","state":"approved","body":"LGTM, ship it.","body_truncated":false,"submitted_at":"2026-07-17T08:02:00Z"}],"page":2,"per_page":10,"has_more":true,"rate":{"remaining":57,"reset":1720000000}})
        );
        let (_, _, commits_body) = request_json(
            &app,
            &format!("/v1/repo/github/{project_id}/commits?sha=feature%2Fdeck&page=1&per_page=10"),
        )
        .await;
        assert_eq!(
            commits_body,
            json!({"ok":true,"commits":[{"sha_short":"cbb8113","sha":"cbb8113f4a7a48830123456789abcdef01234567","message":"fix(surface): fit the landing hero wordmark...","author":"Risingtides-dev","date":"2026-07-17T07:00:00Z"}],"page":1,"per_page":10,"has_more":true,"rate":{"remaining":57,"reset":1720000000}})
        );

        assert_eq!(upstream.calls.load(Ordering::SeqCst), 5);
        for (_, headers) in upstream.seen.lock().unwrap().iter() {
            assert_eq!(headers[header::USER_AGENT], USER_AGENT);
            assert_eq!(headers[header::ACCEPT], ACCEPT);
            assert_eq!(headers["x-github-api-version"], API_VERSION);
            assert!(headers.get(header::AUTHORIZATION).is_none());
            assert!(headers.get(header::IF_NONE_MATCH).is_none());
            assert!(headers.get("x-hostile-caller").is_none());
        }
        let seen = upstream.seen.lock().unwrap();
        assert!(seen.iter().any(|(uri, _)| uri
            == "/repos/Risingtides-dev/ocean-os/commits?sha=feature%2Fdeck&page=1&per_page=10"));
        assert!(seen.iter().any(|(uri, _)| uri
            == &format!("/repos/Risingtides-dev/ocean-os/commits/{sha}/check-runs?per_page=100")));
    }

    #[tokio::test]
    async fn router_owns_all_extractor_and_admission_errors() {
        let upstream = fixture(Arc::new(|_| (StatusCode::OK, b"[]".to_vec(), None))).await;
        let temp = tempfile::tempdir().unwrap();
        let state = crate::tests::fake_convene_state(&temp);
        let project_id = init_git_project(&state, &temp.path().join("workspace"));
        let app = github_router(state, GitHubService::with_origin(&upstream.origin));
        for (uri, expected_status, code) in [
            (
                "/v1/repo/github/not-a-uuid/pulls",
                StatusCode::BAD_REQUEST,
                "invalid_project_id",
            ),
            (
                &format!("/v1/repo/github/{project_id}/pulls/0"),
                StatusCode::BAD_REQUEST,
                "invalid_request",
            ),
            (
                &format!("/v1/repo/github/{project_id}/pulls/not-a-number"),
                StatusCode::BAD_REQUEST,
                "invalid_request",
            ),
            (
                &format!("/v1/repo/github/{project_id}/pulls?state=merged"),
                StatusCode::BAD_REQUEST,
                "invalid_request",
            ),
            (
                &format!("/v1/repo/github/{project_id}/pulls?state=%FF"),
                StatusCode::BAD_REQUEST,
                "invalid_request",
            ),
            (
                &format!("/v1/repo/github/{project_id}/pulls/%FF"),
                StatusCode::BAD_REQUEST,
                "invalid_request",
            ),
            (
                &format!("/v1/repo/github/{project_id}/pulls?page=0"),
                StatusCode::BAD_REQUEST,
                "invalid_request",
            ),
            (
                &format!("/v1/repo/github/{project_id}/pulls?page=abc"),
                StatusCode::BAD_REQUEST,
                "invalid_request",
            ),
            (
                &format!("/v1/repo/github/{project_id}/pulls?per_page=0"),
                StatusCode::BAD_REQUEST,
                "invalid_request",
            ),
            (
                &format!("/v1/repo/github/{project_id}/pulls?per_page=1.5"),
                StatusCode::BAD_REQUEST,
                "invalid_request",
            ),
            (
                &format!("/v1/repo/github/{project_id}/pulls?unknown=1"),
                StatusCode::BAD_REQUEST,
                "invalid_request",
            ),
            (
                &format!("/v1/repo/github/{project_id}/head-sha/abc/checks"),
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_sha",
            ),
        ] {
            let (status, _, body) = request_json(&app, uri).await;
            assert_eq!(status, expected_status, "{uri}");
            assert_eq!(body["error"]["code"], code, "{uri}");
            assert!(body.get("ok").is_some());
        }
        assert_eq!(upstream.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn upstream_byte_bound_accepts_exactly_two_mib_and_rejects_plus_one_uncached() {
        let mut exact_body = Vec::with_capacity(MAX_UPSTREAM_BYTES);
        exact_body.push(b'[');
        exact_body.resize(MAX_UPSTREAM_BYTES - 1, b' ');
        exact_body.push(b']');
        let exact_body = Arc::new(exact_body);
        let exact = fixture({
            let exact_body = exact_body.clone();
            Arc::new(move |_| (StatusCode::OK, exact_body.as_ref().clone(), None))
        })
        .await;
        let temp = tempfile::tempdir().unwrap();
        let state = crate::tests::fake_convene_state(&temp);
        let project_id = init_git_project(&state, &temp.path().join("workspace"));
        let uri = format!("/v1/repo/github/{project_id}/pulls");
        let exact_app = github_router(state.clone(), GitHubService::with_origin(&exact.origin));
        let (status, _, body) = request_json(&exact_app, &uri).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["pulls"], json!([]));

        let huge = Arc::new(vec![b' '; MAX_UPSTREAM_BYTES + 1]);
        let upstream = fixture({
            let huge = huge.clone();
            Arc::new(move |_| (StatusCode::OK, huge.as_ref().clone(), None))
        })
        .await;
        let app = github_router(state, GitHubService::with_origin(&upstream.origin));
        for expected_calls in 1..=2 {
            let (status, _, body) = request_json(&app, &uri).await;
            assert_eq!(status, StatusCode::BAD_GATEWAY);
            assert_eq!(
                body,
                json!({"ok":false,"error":{"code":"github_error","surface_message":"GitHub response too large"}})
            );
            assert_eq!(upstream.calls.load(Ordering::SeqCst), expected_calls);
        }
    }

    #[tokio::test]
    async fn oversized_error_statuses_take_precedence_over_status_mapping() {
        let huge = Arc::new(vec![b's'; MAX_UPSTREAM_BYTES + 1]);
        let upstream = fixture({
            let huge = huge.clone();
            Arc::new(move |uri| {
                let status = if uri.contains("state=closed") {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                };
                (status, huge.as_ref().clone(), None)
            })
        })
        .await;
        let temp = tempfile::tempdir().unwrap();
        let state = crate::tests::fake_convene_state(&temp);
        let project_id = init_git_project(&state, &temp.path().join("workspace"));
        let app = github_router(state, GitHubService::with_origin(&upstream.origin));
        for state in ["closed", "all"] {
            let (status, _, body) = request_json(
                &app,
                &format!("/v1/repo/github/{project_id}/pulls?state={state}"),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_GATEWAY);
            assert_eq!(body["error"]["code"], "github_error");
            assert_eq!(
                body["error"]["surface_message"],
                "GitHub response too large"
            );
            assert!(!body.to_string().contains(&"s".repeat(64)));
        }
    }

    #[tokio::test]
    async fn status_decode_timeout_and_error_refetch_are_sanitized() {
        for (upstream_status, expected_status, expected_code) in [
            (
                StatusCode::NOT_FOUND,
                StatusCode::NOT_FOUND,
                "not_found_or_private",
            ),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                StatusCode::BAD_GATEWAY,
                "github_error",
            ),
        ] {
            let upstream = fixture(Arc::new(move |_| {
                (
                    upstream_status,
                    b"fixture-secret-must-not-escape".to_vec(),
                    None,
                )
            }))
            .await;
            let temp = tempfile::tempdir().unwrap();
            let state = crate::tests::fake_convene_state(&temp);
            let project_id = init_git_project(&state, &temp.path().join("workspace"));
            let app = github_router(state, GitHubService::with_origin(&upstream.origin));
            for calls in 1..=2 {
                let (status, _, body) =
                    request_json(&app, &format!("/v1/repo/github/{project_id}/pulls")).await;
                assert_eq!(status, expected_status);
                assert_eq!(body["error"]["code"], expected_code);
                assert!(!body.to_string().contains("fixture-secret"));
                assert_eq!(upstream.calls.load(Ordering::SeqCst), calls);
            }
        }

        let decode = fixture(Arc::new(|_| {
            (StatusCode::OK, b"decode-secret-not-json".to_vec(), None)
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let state = crate::tests::fake_convene_state(&temp);
        let project_id = init_git_project(&state, &temp.path().join("workspace"));
        let app = github_router(state, GitHubService::with_origin(&decode.origin));
        for calls in 1..=2 {
            let (status, _, body) =
                request_json(&app, &format!("/v1/repo/github/{project_id}/pulls")).await;
            assert_eq!(status, StatusCode::BAD_GATEWAY);
            assert_eq!(body["error"]["code"], "github_error");
            assert!(!body.to_string().contains("decode-secret"));
            assert_eq!(decode.calls.load(Ordering::SeqCst), calls);
        }

        let slow = delayed_fixture(Duration::from_millis(100), b"[]".to_vec()).await;
        let temp = tempfile::tempdir().unwrap();
        let state = crate::tests::fake_convene_state(&temp);
        let project_id = init_git_project(&state, &temp.path().join("workspace"));
        let service =
            GitHubService::with_test_config(&slow.origin, Duration::from_millis(10), CACHE_TTL);
        let app = github_router(state, service);
        let (status, _, body) =
            request_json(&app, &format!("/v1/repo/github/{project_id}/pulls")).await;
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(body["error"]["code"], "github_timeout");
    }

    #[tokio::test]
    async fn malformed_upstream_full_shas_fail_closed_without_echo() {
        let upstream = fixture(Arc::new(|uri| {
            let body = if uri.contains("/commits?") {
                json!([{"sha":"malformed-secret-sha","commit":{"message":"m","author":{"name":"a","date":"d"}},"author":null}])
            } else {
                let mut pull = pull_json();
                pull["head"]["sha"] = json!("malformed-secret-sha");
                if uri.ends_with("/pulls/42") { pull } else { json!([pull]) }
            };
            (StatusCode::OK, serde_json::to_vec(&body).unwrap(), None)
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let state = crate::tests::fake_convene_state(&temp);
        let project_id = init_git_project(&state, &temp.path().join("workspace"));
        let app = github_router(state, GitHubService::with_origin(&upstream.origin));
        for uri in [
            format!("/v1/repo/github/{project_id}/pulls"),
            format!("/v1/repo/github/{project_id}/pulls/42"),
            format!("/v1/repo/github/{project_id}/commits"),
        ] {
            let (status, _, body) = request_json(&app, &uri).await;
            assert_eq!(status, StatusCode::BAD_GATEWAY);
            assert_eq!(body["error"]["code"], "github_error");
            assert!(!body.to_string().contains("malformed-secret-sha"));
        }
    }

    #[tokio::test]
    async fn short_test_ttl_expires_and_refetches() {
        let upstream = fixture(Arc::new(|_| {
            (
                StatusCode::OK,
                serde_json::to_vec(&json!([])).unwrap(),
                None,
            )
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let state = crate::tests::fake_convene_state(&temp);
        let project_id = init_git_project(&state, &temp.path().join("workspace"));
        let service = GitHubService::with_test_config(
            &upstream.origin,
            TOTAL_TIMEOUT,
            Duration::from_millis(15),
        );
        let app = github_router(state, service);
        let uri = format!("/v1/repo/github/{project_id}/pulls");
        let (status, _, first) = request_json(&app, &uri).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(first["page"], 1);
        assert_eq!(first["per_page"], 10);
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(request_json(&app, &uri).await.0, StatusCode::OK);
        assert_eq!(upstream.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn concurrent_identical_misses_singleflight() {
        let upstream = fixture(Arc::new(|_| {
            (
                StatusCode::OK,
                serde_json::to_vec(&json!([])).unwrap(),
                None,
            )
        }))
        .await;
        let temp = tempfile::tempdir().unwrap();
        let state = crate::tests::fake_convene_state(&temp);
        let project_id = init_git_project(&state, &temp.path().join("workspace"));
        let app = github_router(state, GitHubService::with_origin(&upstream.origin));
        let uri = format!("/v1/repo/github/{project_id}/pulls");
        let mut tasks = Vec::new();
        for _ in 0..12 {
            let app = app.clone();
            let uri = uri.clone();
            tasks.push(tokio::spawn(async move { request_json(&app, &uri).await }));
        }
        for task in tasks {
            assert_eq!(task.await.unwrap().0, StatusCode::OK);
        }
        assert_eq!(upstream.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn production_surface_has_no_auth_aggregate_or_write_route() {
        let source = include_str!("github.rs")
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap();
        assert!(!source.contains("Authorization"));
        assert!(!source.contains("\"/v1/repo/github/{project_id}/aggregate"));
        assert!(!source.contains("post("));
        assert!(!source.contains("patch("));
        assert!(!source.contains("delete("));
    }
}
