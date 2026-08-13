use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Scheduleable Ocean routines: prompt-injection hooks now, courier jobs later.
#[derive(Debug, Parser)]
#[command(name = "ocean-heartbeat")]
#[command(about = "Run or generate schedulers for Ocean daemon routines")]
struct Cli {
    /// Local daemon URL.
    #[arg(
        long,
        env = "OCEAN_DAEMON_URL",
        default_value = "http://127.0.0.1:4780"
    )]
    daemon_url: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run one configured heartbeat now.
    Run {
        /// TOML routine file.
        #[arg(long)]
        config: PathBuf,
    },
    /// Write a starter TOML routine file.
    Init {
        /// Destination TOML file.
        #[arg(long)]
        config: PathBuf,
        /// Workspace/repo cwd sent to the daemon.
        #[arg(long)]
        cwd: PathBuf,
        /// Routine id.
        #[arg(long, default_value = "ocean-site-docs")]
        id: String,
    },
    /// Print a render-protocol component snapshot for PWA/dashboard clients.
    Component {
        /// TOML routine file to render.
        #[arg(long)]
        config: PathBuf,
        /// Optional session id to include when wrapping as an AgentTurnEvent.
        #[arg(long)]
        session_id: Option<String>,
        /// Emit only {kind, props, ...} instead of an AgentTurnEvent-like envelope.
        #[arg(long)]
        props_only: bool,
    },
    /// Print a macOS launchd plist for this routine. Does not install it.
    Launchd {
        /// TOML routine file the launchd job should run.
        #[arg(long)]
        config: PathBuf,
        /// launchd label.
        #[arg(long)]
        label: Option<String>,
        /// Start interval in seconds.
        #[arg(long)]
        every_seconds: Option<u64>,
        /// Path to the ocean-heartbeat binary launchd should execute.
        #[arg(long)]
        bin: Option<PathBuf>,
    },
    /// One-shot event-triggered wake: post a single turn to an Ocean session
    /// and (by default) wait for it to finish.
    ///
    /// This is the generic push-wake primitive for external channels — pad
    /// watchers (stitchpad), cron shims, notification bridges — anything that
    /// needs "nudge that agent now" without linking against Ocean or knowing
    /// daemon internals. Exit contract (adapter-friendly):
    ///   0 = turn delivered and completed
    ///   3 = deferred (daemon at capacity, or wait timed out) — retry later
    ///   1 = failed (daemon unreachable, unknown session, turn failed)
    Wake {
        /// Session to wake. Wins over --session-file.
        #[arg(long)]
        session_id: Option<String>,
        /// Durable session-id file (read when --session-id is absent; updated
        /// after a successful post).
        #[arg(long)]
        session_file: Option<PathBuf>,
        /// Prompt text for the wake turn. Use `-` to read stdin.
        #[arg(long)]
        prompt: String,
        /// Working directory for the turn.
        #[arg(long)]
        cwd: PathBuf,
        /// Client type reported to the daemon.
        #[arg(long, default_value = "wake")]
        client_type: String,
        /// Optional project id for daemon project routing.
        #[arg(long)]
        project_id: Option<String>,
        /// Seconds to wait for the turn to finish before deferring (exit 3).
        #[arg(long, default_value_t = 600)]
        timeout_seconds: u64,
        /// Ack-only: don't wait for TurnFinished on the event stream.
        #[arg(long)]
        no_wait: bool,
        /// Allow minting a brand-new session when no session id resolves.
        /// Default is to fail (a wake targets a specific existing agent).
        #[arg(long)]
        allow_new_session: bool,
        /// Pin this wake turn to a specific model id (e.g. `glm-4.6`). Absent =
        /// the daemon's current global model. Lets one pad roster run several
        /// Ocean seats on different models without flipping the daemon default.
        #[arg(long)]
        model: Option<String>,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct RoutineConfig {
    /// Stable routine id. Used for logs and default launchd labels.
    id: String,
    /// Human-readable purpose.
    description: Option<String>,
    /// Daemon working directory for the turn.
    cwd: PathBuf,
    /// Client type sent to the daemon.
    #[serde(default = "default_client_type")]
    client_type: String,
    /// Optional durable session id file. Created/updated after a successful turn.
    session_file: Option<PathBuf>,
    /// Prompt-injection hook for the scheduled agent turn.
    prompt: String,
    /// Optional project id for daemon project routing.
    project_id: Option<String>,
    /// Request timeout in seconds.
    #[serde(default = "default_timeout_seconds")]
    timeout_seconds: u64,
}

fn default_client_type() -> String {
    "heartbeat".into()
}
fn default_timeout_seconds() -> u64 {
    3300
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run { config } => run_once(&cli.daemon_url, &config).await,
        Command::Init { config, cwd, id } => init_config(&config, &cwd, &id),
        Command::Component {
            config,
            session_id,
            props_only,
        } => print_component(&config, session_id, props_only),
        Command::Launchd {
            config,
            label,
            every_seconds,
            bin,
        } => print_launchd(&cli.daemon_url, &config, label, every_seconds, bin),
        Command::Wake {
            session_id,
            session_file,
            prompt,
            cwd,
            client_type,
            project_id,
            timeout_seconds,
            no_wait,
            allow_new_session,
            model,
        } => {
            let code = wake_once(
                &cli.daemon_url,
                WakeArgs {
                    session_id,
                    session_file,
                    prompt,
                    cwd,
                    client_type,
                    project_id,
                    timeout_seconds,
                    no_wait,
                    allow_new_session,
                    model,
                },
            )
            .await
            .unwrap_or_else(|e| {
                eprintln!("wake failed: {e:#}");
                WAKE_FAILED
            });
            std::process::exit(code);
        }
    }
}

/// Adapter exit codes for `wake` (matches the common pad-watcher contract:
/// 0 delivered · 3 deferred/retry-later · everything else failed).
const WAKE_DELIVERED: i32 = 0;
const WAKE_FAILED: i32 = 1;
const WAKE_DEFERRED: i32 = 3;

struct WakeArgs {
    session_id: Option<String>,
    session_file: Option<PathBuf>,
    prompt: String,
    cwd: PathBuf,
    client_type: String,
    project_id: Option<String>,
    timeout_seconds: u64,
    no_wait: bool,
    allow_new_session: bool,
    model: Option<String>,
}

async fn wake_once(daemon_url: &str, args: WakeArgs) -> Result<i32> {
    // Resolve the prompt (`-` = stdin so callers can pipe untrusted pad text
    // without shell-quoting games).
    let prompt = if args.prompt == "-" {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
        text
    } else {
        args.prompt.clone()
    };
    if prompt.trim().is_empty() {
        eprintln!("wake: empty prompt");
        return Ok(WAKE_FAILED);
    }

    // Resolve the target session: explicit id wins, else the durable file.
    // A wake addresses a *specific* agent; minting a fresh session silently
    // would "deliver" the nudge to nobody, so that needs explicit opt-in.
    let session_path = args.session_file.clone().map(expand_home);
    let session_id = args
        .session_id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            session_path
                .as_ref()
                .and_then(|p| fs::read_to_string(p).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        });
    if session_id.is_none() && !args.allow_new_session {
        eprintln!("wake: no session id resolved (pass --session-id/--session-file, or --allow-new-session)");
        return Ok(WAKE_FAILED);
    }

    // The POST is fire-and-ack (202 + status Running), so the HTTP client only
    // needs a short request timeout; turn completion is awaited separately on
    // the SSE stream below.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let base = daemon_url.trim_end_matches('/');
    let health = format!("{base}/health");
    if let Err(e) = client
        .get(&health)
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        eprintln!("wake: daemon not healthy at {health}: {e}");
        return Ok(WAKE_FAILED);
    }

    // P1: preflight the requested model against the daemon's ready list.
    // A dispatch to a model the daemon cannot run must never report "running".
    if let Some(model) = args.model.as_ref() {
        if let Err(preflight_err) = preflight_model(&client, base, model).await {
            eprintln!("wake: model preflight failed — {preflight_err}");
            return Ok(WAKE_FAILED);
        }
    }

    let mut body = json!({
        "prompt": prompt,
        "cwd": args.cwd,
        "client_type": args.client_type,
    });
    if let Some(project_id) = args.project_id.as_ref() {
        body["project_id"] = json!(project_id);
    }
    if let Some(sid) = session_id.as_ref() {
        body["session_id"] = json!(sid);
    }
    // Per-seat model: the daemon treats an explicit model_id as a hard pin for
    // this turn (a bad alias fails cleanly rather than silently substituting).
    if let Some(model) = args.model.as_ref() {
        body["model_id"] = json!(model);
    }

    let url = format!("{base}/v1/agent/turns");
    eprintln!(
        "[{}] wake → {url} (session {})",
        chrono::Utc::now().to_rfc3339(),
        session_id.as_deref().unwrap_or("<new>")
    );
    let response = client.post(&url).json(&body).send().await?;
    // Backpressure is a defer, not a failure: the daemon sheds load with 429
    // and an honest busy body; the caller's gate stays open for a retry.
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        eprintln!("wake: daemon at capacity (429); deferring");
        return Ok(WAKE_DEFERRED);
    }
    let status = response.status();
    let ack: serde_json::Value = response.json().await.unwrap_or_default();
    let ok = ack.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if !status.is_success() || !ok {
        let error = ack.get("error").and_then(|v| v.as_str()).unwrap_or("");
        if error.contains("busy") || error.contains("capacity") {
            eprintln!("wake: daemon busy: {error}; deferring");
            return Ok(WAKE_DEFERRED);
        }
        eprintln!("wake: turn rejected ({status}): {error}");
        return Ok(WAKE_FAILED);
    }

    let turn_id = ack
        .get("turn_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let acked_session = ack
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    // Persist the (possibly daemon-minted) session id for the next wake.
    if let (Some(path), false) = (session_path.as_ref(), acked_session.is_empty()) {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(path, format!("{acked_session}\n"));
    }

    if args.no_wait {
        println!("{}", serde_json::to_string_pretty(&ack)?);
        return Ok(WAKE_DELIVERED);
    }

    // Wait for THIS turn's TurnFinished on the session-scoped event stream.
    // A wait timeout is a defer (the agent may legitimately still be working);
    // a Failed status is a hard failure.
    match tokio::time::timeout(
        Duration::from_secs(args.timeout_seconds),
        wait_for_turn_finished(base, &acked_session, &turn_id),
    )
    .await
    {
        Ok(Ok(true)) => {
            println!("{}", serde_json::to_string_pretty(&ack)?);
            Ok(WAKE_DELIVERED)
        }
        Ok(Ok(false)) => {
            eprintln!("wake: turn {turn_id} finished with status=failed");
            Ok(WAKE_FAILED)
        }
        Ok(Err(e)) => {
            eprintln!("wake: event stream error while waiting: {e:#}");
            Ok(WAKE_FAILED)
        }
        Err(_) => {
            eprintln!(
                "wake: turn {turn_id} still running after {}s; deferring",
                args.timeout_seconds
            );
            Ok(WAKE_DEFERRED)
        }
    }
}

/// Stream `/v1/agent/events?session_id=…` until `turn_id`'s `turn_finished`
/// frame arrives. Returns `true` iff the turn completed successfully.
async fn wait_for_turn_finished(base: &str, session_id: &str, turn_id: &str) -> Result<bool> {
    // No overall client timeout here — the SSE connection is long-lived; the
    // caller bounds the wait with `tokio::time::timeout`.
    let client = reqwest::Client::builder().build()?;
    let url = format!("{base}/v1/agent/events?session_id={session_id}");
    let mut response = client
        .get(&url)
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("subscribe {url}"))?;

    let mut buf = String::new();
    while let Some(chunk) = response.chunk().await? {
        buf.push_str(&String::from_utf8_lossy(&chunk));
        // Consume complete SSE lines; keep the trailing partial line buffered.
        while let Some(pos) = buf.find('\n') {
            let line = buf[..pos].trim_end_matches('\r').to_string();
            buf.drain(..=pos);
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<serde_json::Value>(data.trim()) else {
                continue;
            };
            let is_finished = event
                .get("type")
                .and_then(|v| v.as_str())
                .map(|t| t.eq_ignore_ascii_case("turn_finished"))
                .unwrap_or(false);
            if !is_finished {
                continue;
            }
            if event.get("turn_id").and_then(|v| v.as_str()) != Some(turn_id) {
                continue;
            }
            let Some(status) = event.get("status").and_then(|v| v.as_str()) else {
                continue;
            };
            if status.eq_ignore_ascii_case("running") {
                continue;
            }
            return Ok(status.eq_ignore_ascii_case("completed"));
        }
    }
    anyhow::bail!("event stream ended before turn {turn_id} finished")
}

/// Call `GET /v1/models` and check that `model_id` is both known to the daemon
/// AND ready (provider credential present). Returns `Ok(())` when the model can
/// be dispatched; returns `Err` with a human-readable refusal that names the
/// model and lists ready alternatives. A typo (unknown id) also suggests the
/// nearest valid id via simple edit-distance matching.
async fn preflight_model(
    client: &reqwest::Client,
    base: &str,
    model_id: &str,
) -> Result<(), String> {
    let url = format!("{base}/v1/models");
    let models_resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("could not fetch model list from {url}: {e}"))?;
    let body: serde_json::Value = models_resp
        .json()
        .await
        .map_err(|e| format!("could not parse model list: {e}"))?;
    let models: Vec<serde_json::Value> = body
        .get("models")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if models.is_empty() {
        return Err(format!(
            "model `{model_id}`: daemon returned an empty model list"
        ));
    }

    // Find the requested model in the registry.
    let entry = models
        .iter()
        .find(|m| m.get("id").and_then(|v| v.as_str()) == Some(model_id));

    match entry {
        Some(entry) => {
            let ready = entry
                .get("ready")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !ready {
                // Model is known but not ready — list ready alternatives.
                let ready_ids: Vec<&str> = models
                    .iter()
                    .filter(|m| m.get("ready").and_then(|v| v.as_bool()).unwrap_or(false))
                    .filter_map(|m| m.get("id").and_then(|v| v.as_str()))
                    .collect();
                if ready_ids.is_empty() {
                    return Err(format!(
                        "model `{model_id}` is NOT ready (provider credential missing or configuration error) — no ready models available"
                    ));
                }
                return Err(format!(
                    "model `{model_id}` is NOT ready (provider credential missing or configuration error)\n  ready models: {}",
                    ready_ids.join(", ")
                ));
            }
            Ok(())
        }
        None => {
            // Unknown model id — collect valid ids and suggest nearest.
            let known_ids: Vec<&str> = models
                .iter()
                .filter_map(|m| m.get("id").and_then(|v| v.as_str()))
                .collect();
            let suggestion = nearest_model(model_id, &known_ids);
            match suggestion {
                Some(nearest) => Err(format!(
                    "unknown model `{model_id}` — did you mean `{nearest}`?\n  valid models: {}",
                    known_ids.join(", ")
                )),
                None => Err(format!(
                    "unknown model `{model_id}`\n  valid models: {}",
                    known_ids.join(", ")
                )),
            }
        }
    }
}

/// Simple edit-distance–based nearest match for typo suggestions.
/// Returns the valid id with the smallest Levenshtein distance. Returns `None`
/// only when the candidate list is empty or every distance exceeds 3/4 of the
/// longer string length (a sanity cap: wildly different ids never suggest).
fn nearest_model<'a>(query: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let q = query.to_lowercase();
    candidates
        .iter()
        .filter_map(|c| {
            let dist = levenshtein(&q, &c.to_lowercase());
            let max_len = q.len().max(c.len());
            // Only suggest if the distance is at most 3/4 the longer length.
            if max_len > 0 && dist * 4 <= max_len * 3 {
                Some((dist, *c))
            } else {
                None
            }
        })
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

/// Levenshtein (edit) distance between two strings.
fn levenshtein(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let n = a_chars.len();
    let m = b_chars.len();
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr = vec![0usize; m + 1];
    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = if a_chars[i - 1] == b_chars[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[m]
}

async fn run_once(daemon_url: &str, path: &Path) -> Result<()> {
    let cfg: RoutineConfig = load_config(path)?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(cfg.timeout_seconds))
        .build()?;

    let health = format!("{}/health", daemon_url.trim_end_matches('/'));
    client
        .get(&health)
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("daemon not healthy at {health}"))?;

    let mut body = json!({
        "prompt": cfg.prompt,
        "cwd": cfg.cwd,
        "client_type": cfg.client_type,
    });
    if let Some(project_id) = cfg.project_id {
        body["project_id"] = json!(project_id);
    }
    if let Some(session_file) = cfg
        .session_file
        .as_ref()
        .cloned()
        .map(expand_home)
        .filter(|p| p.exists())
    {
        let sid = fs::read_to_string(&session_file)
            .unwrap_or_default()
            .trim()
            .to_string();
        if !sid.is_empty() {
            body["session_id"] = json!(sid);
        }
    }

    let url = format!("{}/v1/agent/turns", daemon_url.trim_end_matches('/'));
    eprintln!(
        "[{}] posting routine={} to {url}",
        chrono::Utc::now().to_rfc3339(),
        cfg.id
    );
    let response: serde_json::Value = client
        .post(&url)
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    if let (Some(session_file), Some(session_id)) = (
        cfg.session_file.map(expand_home),
        response.get("session_id").and_then(|v| v.as_str()),
    ) {
        if let Some(parent) = session_file.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&session_file, format!("{session_id}\n"))?;
    }

    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

fn load_config(path: &Path) -> Result<RoutineConfig> {
    toml::from_str(
        &fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?,
    )
    .with_context(|| format!("parse config {}", path.display()))
}

fn print_component(path: &Path, session_id: Option<String>, props_only: bool) -> Result<()> {
    let cfg = load_config(path)?;
    let state = routine_state(&cfg);
    let props = json!({
        "stats": [
            {"label": "Routine", "value": cfg.id, "trend": "flat"},
            {"label": "Cadence", "value": "scheduled", "trend": "flat"},
            {"label": "Session", "value": state.session_status, "trend": "flat"},
            {"label": "Mode", "value": cfg.client_type, "trend": "flat"}
        ]
    });
    let component = json!({
        "id": format!("heartbeat:{}", cfg.id),
        "kind": "stat",
        "props": props,
        "replace": true,
        "metadata": {
            "routine_id": cfg.id,
            "description": cfg.description,
            "cwd": cfg.cwd,
            "session_file": state.session_file,
            "last_session_id": state.last_session_id,
            "timeout_seconds": cfg.timeout_seconds,
            "project_id": cfg.project_id
        }
    });

    if props_only {
        println!("{}", serde_json::to_string_pretty(&component)?);
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "type": "component_render",
                "session_id": session_id.unwrap_or_else(|| "heartbeat-dashboard".into()),
                "component_id": component["id"],
                "kind": component["kind"],
                "props": component["props"],
                "replace": component["replace"],
                "metadata": component["metadata"]
            }))?
        );
    }
    Ok(())
}

struct RoutineState {
    session_file: Option<String>,
    last_session_id: Option<String>,
    session_status: String,
}

fn routine_state(cfg: &RoutineConfig) -> RoutineState {
    let session_path = cfg.session_file.as_ref().cloned().map(expand_home);
    let last_session_id = session_path
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let session_status = if last_session_id.is_some() {
        "linked"
    } else {
        "new"
    }
    .to_string();
    RoutineState {
        session_file: session_path.map(|p| p.display().to_string()),
        last_session_id,
        session_status,
    }
}

fn expand_home(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if s == "~" {
        return std::env::var_os("HOME").map(PathBuf::from).unwrap_or(path);
    }
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    path
}

fn init_config(path: &Path, cwd: &Path, id: &str) -> Result<()> {
    let cfg = RoutineConfig {
        id: id.into(),
        description: Some("Ocean OS docs-site heartbeat: one small documentation slice per run.".into()),
        cwd: cwd.to_path_buf(),
        client_type: "heartbeat-cron".into(),
        session_file: Some(PathBuf::from("~/.local/state/ocean-heartbeat/ocean-site-docs.session")),
        prompt: "Heartbeat: resume work on the Ocean OS documentation website in docs/ocean-os-site.\n\nRules:\n- Work in ONE small slice only.\n- Inspect the current site and repo state first.\n- Prefer the next incomplete docs page in order.\n- Ground claims in current repo files.\n- After edits, launch or leave a clear file:// URL.".into(),
        project_id: None,
        timeout_seconds: default_timeout_seconds(),
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, toml::to_string_pretty(&cfg)?)?;
    eprintln!("wrote {}", path.display());
    Ok(())
}

fn print_launchd(
    daemon_url: &str,
    config: &Path,
    label: Option<String>,
    every_seconds: Option<u64>,
    bin: Option<PathBuf>,
) -> Result<()> {
    let cfg: RoutineConfig = toml::from_str(&fs::read_to_string(config)?)?;
    let label = label.unwrap_or_else(|| format!("dev.risingtides.{}", cfg.id));
    let every = every_seconds.unwrap_or(3600);
    let bin = bin.unwrap_or_else(|| PathBuf::from("/usr/local/bin/ocean-heartbeat"));
    let stdout = format!(
        "{}/.local/state/ocean-heartbeat/{}.out",
        std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
        cfg.id
    );
    let stderr = format!(
        "{}/.local/state/ocean-heartbeat/{}.err",
        std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
        cfg.id
    );
    println!(
        "{}",
        launchd_plist(&label, &bin, config, every, daemon_url, &stdout, &stderr)
    );
    Ok(())
}

fn launchd_plist(
    label: &str,
    bin: &Path,
    config: &Path,
    every: u64,
    daemon_url: &str,
    stdout: &str,
    stderr: &str,
) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
    <string>--daemon-url</string><string>{}</string>
    <string>run</string>
    <string>--config</string><string>{}</string>
  </array>
  <key>StartInterval</key><integer>{}</integer>
  <key>RunAtLoad</key><false/>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>"#,
        label,
        bin.display(),
        daemon_url,
        config.display(),
        every,
        stdout,
        stderr
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_identical() {
        assert_eq!(levenshtein("k3", "k3"), 0);
    }

    #[test]
    fn levenshtein_one_substitution() {
        assert_eq!(levenshtein("kimi-k3", "k3"), 5);
    }

    #[test]
    fn levenshtein_one_insertion() {
        assert_eq!(levenshtein("cat", "cats"), 1);
    }

    #[test]
    fn levenshtein_one_deletion() {
        assert_eq!(levenshtein("cats", "cat"), 1);
    }

    #[test]
    fn levenshtein_empty_lhs() {
        assert_eq!(levenshtein("", "abc"), 3);
    }

    #[test]
    fn levenshtein_empty_rhs() {
        assert_eq!(levenshtein("abc", ""), 3);
    }

    #[test]
    fn nearest_model_suggests_close_match() {
        let candidates = &["k3", "deepseek-v4-pro", "gpt-5.6-sol"];
        assert_eq!(nearest_model("kimi-k3", candidates), Some("k3"));
    }

    #[test]
    fn nearest_model_exact_match_found() {
        let candidates = &["k3", "kimi-k3"];
        assert_eq!(nearest_model("k3", candidates), Some("k3"));
    }

    #[test]
    fn nearest_model_too_far_returns_none() {
        let candidates = &["k3", "deepseek-v4-pro"];
        assert_eq!(nearest_model("xyzzy-wizard-extreme", candidates), None);
    }

    #[test]
    fn nearest_model_case_insensitive() {
        let candidates = &["K3", "deepseek-v4-pro"];
        assert_eq!(nearest_model("k3", candidates), Some("K3"));
    }

    #[test]
    fn nearest_model_empty_candidates() {
        let candidates: &[&str] = &[];
        assert_eq!(nearest_model("k3", candidates), None);
    }

    #[test]
    fn nearest_model_short_typo() {
        let candidates = &["gpt-5.6-sol", "gpt-4o", "k3"];
        assert_eq!(nearest_model("gpt-5.6-sol", candidates), Some("gpt-5.6-sol"));
    }
}
