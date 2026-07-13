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
    }
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
