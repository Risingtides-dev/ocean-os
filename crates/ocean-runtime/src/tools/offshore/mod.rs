//! Offshore tools: dispatch agent work to a REMOTE Ocean daemon over the
//! tailnet, inside per-job git worktrees, and ship the results back via git.
//!
//! A 1:1 port of the battle-tested Python harness (`offshore.py`). The remote
//! box keeps bare mirror clones under `~/<remote_root>/mirrors/` and one git
//! worktree per job under `~/<remote_root>/jobs/<job>/work`, provisioned over
//! ssh. Agent turns run on the remote daemon via its HTTP API.
//!
//! **Lifecycle** (one session per job): `offshore_workspace` →
//! `offshore_dispatch` (repeat to steer, watching with `offshore_events` /
//! `offshore_sessions`, aborting with `offshore_cancel`) → `offshore_ship` or
//! `offshore_fetch` → `offshore_clean`.
//!
//! **Permission contract:** control-plane reads (health, sessions, events) are
//! permission-free. Everything that mutates state — remote worktrees
//! (workspace/clean), remote agent turns (dispatch/cancel), pushes and PRs
//! (ship), local refs (fetch), the local mode file (mode) — is permission-gated.
//!
//! Two hard-won git facts are encoded in the remote scripts; keep them:
//! - Mirror clones set `remote.origin.mirror=true`, which rejects per-branch
//!   refspec pushes through the named remote — so `ship` pushes to the remote
//!   **URL** with an explicit refspec (`git push "$url" b:refs/heads/b`), never
//!   `git push origin b`.
//! - The mirror refresh is `git fetch origin '+refs/heads/*:refs/heads/*'` —
//!   heads only, **no `--prune`** (a pruning mirror update would delete the
//!   local `offshore/*` job branches, which don't exist on origin).
//!
//! Known remote-daemon facts the tool descriptions teach the model:
//! - `POST /v1/agent/turns` is SYNCHRONOUS — it responds only when the turn
//!   finishes, so dispatch runs under the long configured turn timeout.
//! - The SSE event stream is live-only (no replay), so `offshore_events` is
//!   timeout-bounded and returns whatever arrived while it listened.
//! - The remote daemon does NOT validate `cwd` — only dispatch into a cwd
//!   returned by `offshore_workspace`.
//! - Old remote builds lack `/v1/requests/{id}/cancel`; the error says so
//!   instead of panicking.

pub mod git;
pub mod mode;
pub mod remote;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::capability::{CapabilityProvider, ProviderHealth, SessionContext, SharedTool};
use crate::types::{AgentTool, AgentToolResult};

/// Total deadline for one control-plane daemon call (health, sessions, cancel).
/// Dispatch uses [`OffshoreConfig::turn_timeout_secs`] instead.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
/// Connect-phase deadline (DNS + TCP) for every daemon call.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Total deadline for one ssh/git subprocess. Generous — a first `git clone
/// --mirror` of a big repo is slow — but bounded, so a blackholed tailnet link
/// can never freeze the turn forever (tool execution is NOT bounded by the
/// turn timeout; same hazard web_fetch fixed).
const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(600);

/// Static configuration for the offshore tool family, resolved from the
/// daemon's `[offshore]` config table (defaults already applied).
#[derive(Debug, Clone)]
pub struct OffshoreConfig {
    /// Base URL of the remote Ocean daemon, e.g. `http://100.90.205.60:4780`.
    pub remote_url: String,
    /// ssh destination of the remote box, e.g. `user@100.90.205.60`.
    pub ssh_host: String,
    /// ssh binary to run, e.g. `/usr/bin/ssh`.
    pub ssh_bin: String,
    /// Directory under the remote `$HOME` holding `mirrors/` and `jobs/`.
    pub remote_root: String,
    /// Deadline (seconds) for one synchronous dispatch turn — the remote daemon
    /// responds only when the turn finishes.
    pub turn_timeout_secs: u64,
}

/// Shared dependency injected into every offshore tool.
#[derive(Clone)]
pub struct OffshoreToolCtx {
    pub cfg: Arc<OffshoreConfig>,
}

impl OffshoreToolCtx {
    /// Absolute daemon URL for `path` (which starts with `/`).
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.cfg.remote_url.trim_end_matches('/'), path)
    }

    /// Build a fresh HTTP client (the web_fetch idiom — construction is cheap
    /// next to the multi-second calls this family makes). `total_timeout` is
    /// the whole-call deadline; `None` for the SSE stream, whose reads are
    /// deadline-bounded by the caller instead.
    fn http_client(&self, total_timeout: Option<Duration>) -> Result<reqwest::Client, String> {
        let mut builder = reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT);
        if let Some(t) = total_timeout {
            builder = builder.timeout(t);
        }
        builder
            .build()
            .map_err(|e| format!("building http client: {e}"))
    }

    /// One daemon HTTP call. 2xx → body (pretty-printed when it's JSON);
    /// anything else → `Err("daemon <code> on <path>: <body head>")`, matching
    /// the Python harness's error shape.
    async fn api(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<String, String> {
        let client = self.http_client(Some(timeout))?;
        let mut req = client.request(method, self.url(path));
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("offshore daemon unreachable on {path}: {e}"))?;
        let status = resp.status();
        let raw = resp
            .text()
            .await
            .map_err(|e| format!("reading daemon response on {path}: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "daemon {} on {path}: {}",
                status.as_u16(),
                head(&raw, 400)
            ));
        }
        Ok(pretty_json(raw))
    }

    /// Run one remote shell script over ssh (`ssh -o BatchMode=yes <host>
    /// <script>`), with output capture and a hard deadline. `kill_on_drop`
    /// guarantees no orphan ssh survives the timeout.
    async fn run_ssh(&self, remote_cmd: &str) -> Result<std::process::Output, String> {
        let mut command = tokio::process::Command::new(&self.cfg.ssh_bin);
        command
            .args(["-o", "BatchMode=yes"])
            .arg(&self.cfg.ssh_host)
            .arg(remote_cmd)
            .kill_on_drop(true);
        match tokio::time::timeout(SUBPROCESS_TIMEOUT, command.output()).await {
            Ok(out) => out.map_err(|e| format!("could not run {}: {e}", self.cfg.ssh_bin)),
            Err(_) => Err(format!(
                "ssh to {} timed out after {}s",
                self.cfg.ssh_host,
                SUBPROCESS_TIMEOUT.as_secs()
            )),
        }
    }
}

/// Construct the full offshore tool suite, in lifecycle order.
pub fn offshore_tools(ctx: OffshoreToolCtx) -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(remote::OffshoreHealthTool { ctx: ctx.clone() }),
        Arc::new(git::OffshoreWorkspaceTool { ctx: ctx.clone() }),
        Arc::new(remote::OffshoreDispatchTool { ctx: ctx.clone() }),
        Arc::new(remote::OffshoreSessionsTool { ctx: ctx.clone() }),
        Arc::new(remote::OffshoreEventsTool { ctx: ctx.clone() }),
        Arc::new(remote::OffshoreCancelTool { ctx: ctx.clone() }),
        Arc::new(git::OffshoreShipTool { ctx: ctx.clone() }),
        Arc::new(git::OffshoreFetchTool { ctx: ctx.clone() }),
        Arc::new(git::OffshoreCleanTool { ctx }),
        Arc::new(mode::OffshoreModeTool::new()),
    ]
}

/// Capability provider for the offshore tool family. Tools are built once at
/// construction and cloned per call (cheap `Arc` bumps) — listing them on every
/// turn never touches the network or spawns a process; only executing a tool
/// does. Registered by the daemon only when an `[offshore]` config table is
/// present and enabled.
pub struct OffshoreProvider {
    tools: Vec<SharedTool>,
}

impl OffshoreProvider {
    pub fn new(config: OffshoreConfig) -> Self {
        Self {
            tools: offshore_tools(OffshoreToolCtx {
                cfg: Arc::new(config),
            }),
        }
    }
}

#[async_trait]
impl CapabilityProvider for OffshoreProvider {
    fn id(&self) -> &str {
        "offshore"
    }

    async fn tools(&self, _ctx: &SessionContext) -> Vec<SharedTool> {
        self.tools.clone()
    }

    async fn health(&self) -> ProviderHealth {
        ProviderHealth::Ready
    }
}

// ── shared helpers ──────────────────────────────────────────────────────────

/// The bare repo name for a clone URL or mirror name: last path segment, `.git`
/// suffix stripped. `https://github.com/me/ocean-os.git` → `ocean-os`.
pub(crate) fn repo_name(repo: &str) -> String {
    let name = repo
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(repo);
    name.strip_suffix(".git").unwrap_or(name).to_string()
}

/// Whether `repo` is a clone URL (vs the bare name of an existing mirror).
pub(crate) fn is_clone_url(repo: &str) -> bool {
    repo.contains("://") || repo.starts_with("git@")
}

/// Validate a value that is interpolated into a remote shell script or a path
/// (job ids, repo mirror names). The Python harness trusts its CLI caller; a
/// model-driven tool must not, so these are restricted to a safe charset.
pub(crate) fn validate_name(kind: &str, value: &str) -> Result<(), String> {
    let ok = value
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "invalid {kind} '{value}': use ASCII letters, digits, '.', '_' and '-', starting with a letter or digit"
        ))
    }
}

/// The remote paths for one job: `(mirror, work, branch)`. `$HOME` is left
/// unexpanded — it resolves on the remote shell.
pub(crate) fn job_paths(remote_root: &str, repo: &str, job: &str) -> (String, String, String) {
    let root = format!("$HOME/{remote_root}");
    (
        format!("{root}/mirrors/{}.git", repo_name(repo)),
        format!("{root}/jobs/{job}/work"),
        format!("offshore/{job}"),
    )
}

/// A fresh job id: local timestamp + 4 random hex chars, matching the Python
/// harness's `%Y%m%d-%H%M%S-<token_hex(2)>` shape.
pub(crate) fn generate_job_id() -> String {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let hex = uuid::Uuid::new_v4().simple().to_string();
    format!("{stamp}-{}", &hex[..4])
}

/// Pretty-print `raw` when it parses as JSON; otherwise return it untouched.
pub(crate) fn pretty_json(raw: String) -> String {
    match serde_json::from_str::<Value>(&raw) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or(raw),
        Err(_) => raw,
    }
}

/// A pretty-JSON text result (the harness `emit` shape).
pub(crate) fn json_text(v: &Value) -> AgentToolResult {
    AgentToolResult::text(serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()))
}

/// First `n` bytes of `s`, cut back to a char boundary (error excerpts).
pub(crate) fn head(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut cut = n;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

/// Last `n` bytes of `s`, cut forward to a char boundary (output tails).
pub(crate) fn tail(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut cut = s.len() - n;
    while cut < s.len() && !s.is_char_boundary(cut) {
        cut += 1;
    }
    &s[cut..]
}

#[cfg(test)]
pub(crate) fn test_ctx() -> OffshoreToolCtx {
    OffshoreToolCtx {
        cfg: Arc::new(OffshoreConfig {
            remote_url: "http://100.90.205.60:4780".into(),
            ssh_host: "smathdaddy@100.90.205.60".into(),
            ssh_bin: "/usr/bin/ssh".into(),
            remote_root: "offshore".into(),
            turn_timeout_secs: 900,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn provider_lists_the_ten_tools_in_lifecycle_order() {
        let provider = OffshoreProvider::new((*test_ctx().cfg).clone());
        let tools = provider.tools(&SessionContext::default()).await;
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(
            names,
            vec![
                "offshore_health",
                "offshore_workspace",
                "offshore_dispatch",
                "offshore_sessions",
                "offshore_events",
                "offshore_cancel",
                "offshore_ship",
                "offshore_fetch",
                "offshore_clean",
                "offshore_mode",
            ]
        );
        assert_eq!(provider.id(), "offshore");
    }

    #[tokio::test]
    async fn permission_contract_reads_free_mutations_gated() {
        let provider = OffshoreProvider::new((*test_ctx().cfg).clone());
        let tools = provider.tools(&SessionContext::default()).await;
        for tool in tools {
            let gated = tool.requires_permission();
            match tool.name() {
                "offshore_health" | "offshore_sessions" | "offshore_events" => {
                    assert!(!gated, "{} must be permission-free", tool.name())
                }
                other => assert!(gated, "{other} must be permission-gated"),
            }
        }
    }

    #[test]
    fn repo_name_handles_urls_names_and_suffixes() {
        assert_eq!(repo_name("https://github.com/me/ocean-os.git"), "ocean-os");
        assert_eq!(repo_name("https://github.com/me/ocean-os/"), "ocean-os");
        assert_eq!(repo_name("git@github.com:me/ocean-os.git"), "ocean-os");
        assert_eq!(repo_name("ocean-os"), "ocean-os");
        assert_eq!(repo_name("ocean-os.git"), "ocean-os");
    }

    #[test]
    fn clone_url_detection_matches_the_harness() {
        assert!(is_clone_url("https://github.com/me/x.git"));
        assert!(is_clone_url("ssh://host/x"));
        assert!(is_clone_url("git@github.com:me/x.git"));
        assert!(!is_clone_url("ocean-os"));
    }

    #[test]
    fn name_validation_rejects_shell_metacharacters() {
        assert!(validate_name("job id", "20260709-153000-a1b2").is_ok());
        assert!(validate_name("repo name", "ocean-os").is_ok());
        assert!(validate_name("job id", "x; rm -rf ~").is_err());
        assert!(validate_name("job id", "$(reboot)").is_err());
        assert!(validate_name("job id", "-flag").is_err());
        assert!(validate_name("job id", "").is_err());
        assert!(validate_name("job id", "a/b").is_err());
    }

    #[test]
    fn job_paths_match_the_remote_layout() {
        let (mirror, work, branch) = job_paths("offshore", "https://x/y/repo.git", "j1");
        assert_eq!(mirror, "$HOME/offshore/mirrors/repo.git");
        assert_eq!(work, "$HOME/offshore/jobs/j1/work");
        assert_eq!(branch, "offshore/j1");
    }

    #[test]
    fn generated_job_ids_are_valid_names() {
        let job = generate_job_id();
        assert!(validate_name("job id", &job).is_ok(), "generated: {job}");
        // <YYYYmmdd>-<HHMMSS>-<4 hex>
        assert_eq!(job.len(), "20260709-153000-a1b2".len(), "shape: {job}");
    }

    #[test]
    fn head_and_tail_respect_char_boundaries() {
        let s = "é".repeat(10); // 2 bytes per char
        assert_eq!(head(&s, 5), "éé");
        assert_eq!(tail(&s, 5), "éé");
        assert_eq!(head("abc", 10), "abc");
        assert_eq!(tail("abc", 10), "abc");
    }
}
