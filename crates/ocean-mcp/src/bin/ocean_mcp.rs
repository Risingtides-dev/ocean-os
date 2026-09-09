//! `ocean-mcp` — Ocean as an MCP **server**.
//!
//! Claude Code, Codex, Cursor, and any other MCP client spawn this binary and
//! speak newline-delimited JSON-RPC to it over stdio. It holds no agent logic
//! and no sessions of its own: every tool is a thin, typed call against the
//! Ocean daemon's existing HTTP API on this machine.
//!
//! ```text
//!   Claude Code ──MCP/stdio──▶ ocean-mcp ──HTTP──▶ ocean-daemon (:4780)
//! ```
//!
//! The daemon stays the authority for sessions, rooms, permissions, and tools.
//! This bridge only translates. Two consequences follow and are deliberate:
//!
//! 1. **No secrets cross the wire.** The bridge never reads `auth.json`, the
//!    operator key, or a room bearer. Read routes are credential-free by the
//!    daemon's own design; the one write (`ocean_room_post`) is a room message
//!    authored by the local member the daemon already knows.
//! 2. **No new authority.** A prompt submitted through `ocean_prompt` runs
//!    under the daemon's normal permission policy. Nothing here widens it.
//!
//! Subcommands: `serve` (the MCP server, default), `setup` (install the
//! `ocean` skill for Claude Code / Codex and print the MCP config lines), and
//! `doctor` (is the daemon reachable, who am I).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const SERVER_NAME: &str = "ocean";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROTOCOL_VERSION: &str = "2025-06-18";
const DEFAULT_DAEMON_URL: &str = "http://127.0.0.1:4780";
const MAX_ROW_CHARS: usize = 600;
const MAX_TOOL_TEXT_CHARS: usize = 60_000;

#[derive(Parser, Debug)]
#[command(
    name = "ocean-mcp",
    about = "Ocean as an MCP server: rooms, sessions, and prompts for Claude Code, Codex, and friends"
)]
struct Cli {
    /// Ocean daemon base URL.
    #[arg(long, env = "OCEAN_DAEMON_URL", default_value = DEFAULT_DAEMON_URL, global = true)]
    daemon: String,
    /// The room member id this bridge posts as. Defaults to `$USER`.
    #[arg(long, env = "OCEAN_MEMBER_ID", global = true)]
    member: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the MCP server over stdio (the default when no subcommand is given).
    Serve,
    /// Install the `ocean` skill for Claude Code / Codex and print the MCP
    /// config lines to add. Writes only under your home directory.
    Setup {
        /// Print what would be written without writing it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Check that the daemon is reachable and report the identity this bridge
    /// will use.
    Doctor,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let member = cli
        .member
        .clone()
        .or_else(|| std::env::var("USER").ok())
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| "operator".to_string());
    let daemon = Daemon::new(&cli.daemon, &member);
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(daemon).await,
        Command::Setup { dry_run } => setup(dry_run),
        Command::Doctor => doctor(&daemon).await,
    }
}

// ── Daemon client ─────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Daemon {
    base: String,
    member: String,
    http: reqwest::Client,
}

impl Daemon {
    fn new(base: &str, member: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            member: member.to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .build()
                .expect("reqwest client"),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let response = self
            .http
            .get(self.url(path))
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .with_context(|| format!("daemon unreachable at {}", self.base))?;
        Self::body(response).await
    }

    async fn post(&self, path: &str, body: Value, timeout: Duration) -> Result<Value> {
        let response = self
            .http
            .post(self.url(path))
            .json(&body)
            .timeout(timeout)
            .send()
            .await
            .with_context(|| format!("daemon unreachable at {}", self.base))?;
        Self::body(response).await
    }

    async fn body(response: reqwest::Response) -> Result<Value> {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        let value: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));
        if !status.is_success() {
            let code = value
                .get("error")
                .or_else(|| value.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("request_failed");
            return Err(anyhow!("{} ({})", code, status.as_u16()));
        }
        Ok(value)
    }
}

// ── Tools ─────────────────────────────────────────────────────────────────────

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        }
    })
}

fn tool_definitions() -> Vec<Value> {
    vec![
        tool(
            "ocean_health",
            "Is the local Ocean daemon up, and which model backend is it on.",
            json!({}),
            &[],
        ),
        tool(
            "ocean_rooms",
            "List the Ocean rooms on this daemon: id, name, who is in them (humans and agents).",
            json!({}),
            &[],
        ),
        tool(
            "ocean_room_read",
            "Read the newest messages in a room (newest page, oldest first). Use before_seq to page back.",
            json!({
                "room": { "type": "string", "description": "Room id, e.g. campaigns" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Rows to return (default 30)" },
                "before_seq": { "type": "integer", "minimum": 0, "description": "Page backward: rows strictly before this seq" }
            }),
            &["room"],
        ),
        tool(
            "ocean_room_post",
            "Post a message into a room as you. Mention an agent with @name to wake it (e.g. \"@room-builder review the plan\").",
            json!({
                "room": { "type": "string" },
                "body": { "type": "string", "description": "Markdown message body" },
                "thread_parent_seq": { "type": "integer", "minimum": 1, "description": "Reply in the thread under this message" }
            }),
            &["room", "body"],
        ),
        tool(
            "ocean_room_join",
            "Join a room as you (adds your member id to the roster so you can post).",
            json!({ "room": { "type": "string" } }),
            &["room"],
        ),
        tool(
            "ocean_room_inspect",
            "Everything about a room's setup: authorized agents and their sessions, contributed folders, credential slots, where agents execute.",
            json!({ "room": { "type": "string" } }),
            &["room"],
        ),
        tool(
            "ocean_room_resources",
            "The folders contributed to a room (opaque resource ids, aliases, access mode, status). Never paths.",
            json!({ "room": { "type": "string" } }),
            &["room"],
        ),
        tool(
            "ocean_agents",
            "The agent packages installed on this daemon that can be authorized into rooms.",
            json!({}),
            &[],
        ),
        tool(
            "ocean_sessions",
            "Recent Ocean sessions on this daemon (id, title, model, turns, workspace).",
            json!({ "limit": { "type": "integer", "minimum": 1, "maximum": 100 } }),
            &[],
        ),
        tool(
            "ocean_prompt",
            "Run one Ocean agent turn and return its output. Runs in this process's working directory unless cwd is given; pass session_id to continue a session. Gated tools follow the daemon's permission policy unless yolo is true.",
            json!({
                "prompt": { "type": "string" },
                "cwd": { "type": "string", "description": "Absolute working directory (default: where this MCP server was started)" },
                "session_id": { "type": "string", "description": "Continue an existing session" },
                "yolo": { "type": "boolean", "description": "Auto-approve gated tools for this turn (default false)" },
                "max_turns": { "type": "integer", "minimum": 1, "maximum": 64 }
            }),
            &["prompt"],
        ),
    ]
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push_str(" …[truncated]");
    out
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("missing argument: {key}"))
}

fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

/// A room message row as the model should read it. System audit rows carry a
/// JSON body; the daemon already projects them as readable labels on newer
/// builds, and we fall back to the `type` field on older ones.
fn render_row(row: &Value) -> String {
    let seq = row.get("seq").and_then(Value::as_u64).unwrap_or(0);
    let author = row.get("author_id").and_then(Value::as_str).unwrap_or("?");
    let kind = row.get("kind").and_then(Value::as_str).unwrap_or("message");
    let body = row.get("body").and_then(Value::as_str).unwrap_or("");
    let body = if kind == "system" {
        serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| {
                v.get("type")
                    .and_then(Value::as_str)
                    .map(|t| format!("[{t}]"))
            })
            .unwrap_or_else(|| body.to_string())
    } else {
        body.to_string()
    };
    let thread = row
        .get("thread_parent_seq")
        .and_then(Value::as_u64)
        .map(|p| format!(" (reply to #{p})"))
        .unwrap_or_default();
    format!(
        "#{seq} {author}{thread}: {}",
        truncate(&body, MAX_ROW_CHARS)
    )
}

async fn call_tool(daemon: &Daemon, name: &str, args: &Value) -> Result<String> {
    match name {
        "ocean_health" => {
            let v = daemon.get("/health").await?;
            Ok(format!(
                "ok={} backend={} rev={}",
                v["ok"],
                v["backend"].as_str().unwrap_or("?"),
                v["rev"].as_str().unwrap_or("?")
            ))
        }
        "ocean_rooms" => {
            let v = daemon.get("/v1/rooms/persistent").await?;
            let rooms = v
                .get("rooms")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if rooms.is_empty() {
                return Ok("no rooms on this daemon".into());
            }
            let mut out = String::new();
            for r in rooms {
                let members: Vec<String> = r["participants"]
                    .as_array()
                    .map(|ps| {
                        ps.iter()
                            .map(|p| {
                                format!(
                                    "{}{}",
                                    p["id"].as_str().unwrap_or("?"),
                                    if p["kind"] == "agent" { " (agent)" } else { "" }
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out.push_str(&format!(
                    "- {} — {} — members: {}\n",
                    r["id"].as_str().unwrap_or("?"),
                    r["name"].as_str().unwrap_or(""),
                    if members.is_empty() {
                        "none".to_string()
                    } else {
                        members.join(", ")
                    }
                ));
            }
            Ok(out)
        }
        "ocean_room_read" => {
            let room = arg_str(args, "room")?;
            let limit = arg_u64(args, "limit").unwrap_or(30).clamp(1, 100);
            let before = arg_u64(args, "before_seq").unwrap_or(u64::MAX);
            let v = daemon
                .get(&format!(
                    "/v1/rooms/persistent/{room}/snapshot?before_seq={before}&limit={limit}"
                ))
                .await?;
            let rows = v
                .get("transcript")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut out = format!(
                "room {} — {} rows{}\n",
                room,
                rows.len(),
                if v["has_more"] == true {
                    format!(" (more before seq {})", v["prev_seq"].as_u64().unwrap_or(0))
                } else {
                    String::new()
                }
            );
            for row in &rows {
                out.push_str(&render_row(row));
                out.push('\n');
            }
            Ok(out)
        }
        "ocean_room_post" => {
            let room = arg_str(args, "room")?;
            let body = arg_str(args, "body")?;
            let mut req = json!({
                "author_id": daemon.member,
                "author_kind": "human",
                "body": body,
            });
            if let Some(parent) = arg_u64(args, "thread_parent_seq") {
                req["thread_parent_seq"] = json!(parent);
            }
            let v = daemon
                .post(
                    &format!("/v1/rooms/persistent/{room}/messages"),
                    req,
                    Duration::from_secs(30),
                )
                .await?;
            let seq = v["message"]["seq"].as_u64().unwrap_or(0);
            let fired = v["triggers_fired"].as_array().map(|t| t.len()).unwrap_or(0);
            Ok(format!(
                "posted #{seq} to {room} as {}{}",
                daemon.member,
                if fired > 0 {
                    format!("; woke {fired} agent(s) — read the room again shortly for replies")
                } else {
                    String::new()
                }
            ))
        }
        "ocean_room_join" => {
            let room = arg_str(args, "room")?;
            daemon
                .post(
                    &format!("/v1/rooms/persistent/{room}/participants"),
                    json!({
                        "id": daemon.member,
                        "kind": "human",
                        "display_name": daemon.member,
                    }),
                    Duration::from_secs(30),
                )
                .await?;
            Ok(format!("joined {room} as {}", daemon.member))
        }
        "ocean_room_inspect" => {
            let room = arg_str(args, "room")?;
            let v = daemon
                .get(&format!("/v1/rooms/persistent/{room}/inspect"))
                .await?;
            let mut out = format!(
                "room {} — access {} — federated {}\n",
                room, v["access"]["state"], v["federated"]
            );
            out.push_str(&format!(
                "execution: {}\n",
                serde_json::to_string(&v["execution"]).unwrap_or_default()
            ));
            match v["agents"].as_array() {
                Some(agents) if !agents.is_empty() => {
                    out.push_str("agents:\n");
                    for a in agents {
                        out.push_str(&format!(
                            "- {} status={} generation={} session={} cwd_source={}\n",
                            a["agent_member_id"].as_str().unwrap_or("?"),
                            a["status"].as_str().unwrap_or("?"),
                            a["generation"].as_str().unwrap_or("?"),
                            a["session_id"].as_str().unwrap_or("?"),
                            a["execution"]["cwd_source"].as_str().unwrap_or("?")
                        ));
                    }
                }
                _ => out.push_str("agents: none authorized\n"),
            }
            if let Some(slots) = v["credential_slots"].as_array() {
                if !slots.is_empty() {
                    out.push_str("credential slots:\n");
                    for s in slots {
                        out.push_str(&format!(
                            "- {} {} required={}\n",
                            s["name"].as_str().unwrap_or("?"),
                            s["status"].as_str().unwrap_or("?"),
                            s["required"]
                        ));
                    }
                }
            }
            if let Some(resources) = v["resources"].as_array() {
                if !resources.is_empty() {
                    out.push_str("contributed folders:\n");
                    for r in resources {
                        out.push_str(&format!(
                            "- {} ({}) {} status={} agents={}\n",
                            r["display_name"].as_str().unwrap_or("?"),
                            r["resource_id"].as_str().unwrap_or("?"),
                            r["access_mode"].as_str().unwrap_or("?"),
                            r["status"].as_str().unwrap_or("?"),
                            serde_json::to_string(&r["authorized_agent_member_ids"])
                                .unwrap_or_default()
                        ));
                    }
                }
            }
            Ok(out)
        }
        "ocean_room_resources" => {
            let room = arg_str(args, "room")?;
            let v = daemon
                .get(&format!("/v1/rooms/persistent/{room}/resources"))
                .await?;
            Ok(serde_json::to_string_pretty(&v["resources"]).unwrap_or_default())
        }
        "ocean_agents" => {
            let v = daemon.get("/v1/agents").await?;
            Ok(truncate(
                &serde_json::to_string_pretty(&v).unwrap_or_default(),
                MAX_TOOL_TEXT_CHARS,
            ))
        }
        "ocean_sessions" => {
            let limit = arg_u64(args, "limit").unwrap_or(20).clamp(1, 100) as usize;
            let v = daemon.get("/v1/agent/sessions").await?;
            let sessions = v
                .get("sessions")
                .and_then(Value::as_array)
                .cloned()
                .or_else(|| v.as_array().cloned())
                .unwrap_or_default();
            let mut out = String::new();
            for s in sessions.iter().take(limit) {
                out.push_str(&format!(
                    "- {} — {} — model={} turns={} workspace={}\n",
                    s["id"].as_str().unwrap_or("?"),
                    s["title"].as_str().unwrap_or(""),
                    s["model"].as_str().unwrap_or("?"),
                    s["turns"],
                    s["workspace_root"].as_str().unwrap_or("-")
                ));
            }
            if out.is_empty() {
                out.push_str("no sessions");
            }
            Ok(out)
        }
        "ocean_prompt" => {
            let prompt = arg_str(args, "prompt")?;
            let cwd = args
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    std::env::current_dir()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| "/".into())
                });
            let session_id = args.get("session_id").and_then(Value::as_str);
            let yolo = args.get("yolo").and_then(Value::as_bool).unwrap_or(false);
            let max_turns = arg_u64(args, "max_turns");
            let body = json!({
                "prompt": prompt,
                "images": null,
                "request_id": null,
                "session_id": session_id,
                "create_if_missing": true,
                "max_turns": max_turns,
                "yolo": yolo,
                "cwd": cwd,
                "project_id": null,
                "client_type": "mcp",
                "decision_token": null,
            });
            let v = daemon
                .post("/v1/prompt", body, Duration::from_secs(600))
                .await?;
            let mut out = String::new();
            if let Some(sid) = v["session_id"].as_str() {
                out.push_str(&format!("session_id: {sid}\n"));
            }
            out.push_str(v["stdout"].as_str().unwrap_or(""));
            let stderr = v["stderr"].as_str().unwrap_or("");
            if !stderr.trim().is_empty() {
                out.push_str("\n[stderr]\n");
                out.push_str(stderr);
            }
            if v["ok"] != true {
                out.push_str("\n[turn did not complete cleanly]");
            }
            Ok(truncate(&out, MAX_TOOL_TEXT_CHARS))
        }
        other => Err(anyhow!("unknown tool: {other}")),
    }
}

// ── MCP server loop ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Incoming {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
}

fn response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

fn tool_result(text: String, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

/// Handle one request. `None` for notifications (nothing to write back).
async fn handle(daemon: &Daemon, incoming: Incoming) -> Option<Value> {
    let method = incoming.method.clone().unwrap_or_default();
    let Some(id) = incoming.id.clone() else {
        // Notification: `notifications/initialized`, `notifications/cancelled`…
        return None;
    };
    Some(match method.as_str() {
        "initialize" => {
            let requested = incoming
                .params
                .as_ref()
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION)
                .to_string();
            response(
                id,
                json!({
                    "protocolVersion": requested,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
                    "instructions": "Ocean rooms are where your team's humans and agents work together. Read a room before posting; mention an agent with @name to wake it; use ocean_room_inspect to see which agents and folders a room has. ocean_prompt runs one Ocean turn in the current project.",
                }),
            )
        }
        "ping" => response(id, json!({})),
        "tools/list" => response(id, json!({ "tools": tool_definitions() })),
        "tools/call" => {
            let params = incoming.params.unwrap_or(Value::Null);
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match call_tool(daemon, &name, &args).await {
                Ok(text) => response(id, tool_result(text, false)),
                Err(err) => response(id, tool_result(format!("error: {err:#}"), true)),
            }
        }
        "resources/list" => response(id, json!({ "resources": [] })),
        "prompts/list" => response(id, json!({ "prompts": [] })),
        _ => error(id, -32601, format!("method not found: {method}")),
    })
}

async fn serve(daemon: Daemon) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let incoming: Incoming = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(err) => {
                let reply = error(Value::Null, -32700, format!("parse error: {err}"));
                stdout.write_all(format!("{reply}\n").as_bytes()).await?;
                stdout.flush().await?;
                continue;
            }
        };
        if let Some(reply) = handle(&daemon, incoming).await {
            stdout.write_all(format!("{reply}\n").as_bytes()).await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

// ── setup / doctor ────────────────────────────────────────────────────────────

const SKILL_MD: &str = r#"---
name: ocean
description: Use Ocean (the team's local agent daemon) from this session — read and post in Ocean rooms where teammates and agents work together, mention room agents, see which folders and credentials a room has, and run an Ocean turn in the current project. Trigger on "ocean", "room", "post in the room", "ask <agent-name>", "what's in the campaigns room", or when work should be visible to the team.
---

# Ocean

Ocean runs on this machine as a daemon. The `ocean` MCP server exposes it as tools.
Rooms are the team's shared channels: humans and authorized agents in one durable
transcript, with folders and credentials attached per room.

## How to work in a room

1. `ocean_rooms` to see the rooms, then `ocean_room_read {room}` before saying anything.
2. `ocean_room_inspect {room}` tells you which agents are authorized, which folders
   they can read, and whether their credential slots resolve. Do not assume an
   agent can see a file unless a folder is listed for it.
3. Post with `ocean_room_post`. Mention an agent as `@agent-id` to wake it; it replies
   in the room, so read again after a short wait rather than polling in a tight loop.
4. If a post fails with "not in roster", call `ocean_room_join {room}` once.

## Running work

- `ocean_prompt {prompt}` runs one Ocean turn in the current working directory and
  returns its output. Pass `session_id` back to continue the same session.
- Gated tools follow Ocean's permission policy; set `yolo: true` only when the
  user asked for an unattended run.

## Rules

- Never paste secrets into a room. Credential status is shown as resolved / missing;
  the values stay on the machine.
- Room ids are stable; display names are not. Use ids in tool calls.
- Audit lines in a transcript (`[room.agent.admission]` etc.) are the daemon's ledger,
  not conversation. Summarize them, don't repeat them.
"#;

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is not set"))
}

fn skill_targets(home: &Path) -> Vec<PathBuf> {
    // Claude Code, Codex, and the shared agents dir. Each is created if its
    // parent tool directory already exists; we never create a tool's root.
    [".claude/skills", ".codex/skills", ".agents/skills"]
        .iter()
        .map(|rel| home.join(rel))
        .filter(|dir| dir.parent().is_some_and(|p| p.exists()))
        .map(|dir| dir.join("ocean").join("SKILL.md"))
        .collect()
}

fn setup(dry_run: bool) -> Result<()> {
    let home = home()?;
    let targets = skill_targets(&home);
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "ocean-mcp".into());
    let mut out = std::io::stdout();
    if targets.is_empty() {
        writeln!(
            out,
            "no ~/.claude, ~/.codex, or ~/.agents directory found; skill not installed"
        )?;
    }
    for target in &targets {
        if dry_run {
            writeln!(out, "would write {}", target.display())?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, SKILL_MD)?;
        writeln!(out, "wrote {}", target.display())?;
    }
    writeln!(out)?;
    writeln!(out, "Add the MCP server to your tools:")?;
    writeln!(out)?;
    writeln!(out, "  Claude Code:")?;
    writeln!(out, "    claude mcp add --scope user ocean -- {exe} serve")?;
    writeln!(out)?;
    writeln!(out, "  Codex (~/.codex/config.toml):")?;
    writeln!(out, "    [mcp_servers.ocean]")?;
    writeln!(out, "    command = \"{exe}\"")?;
    writeln!(out, "    args = [\"serve\"]")?;
    writeln!(out)?;
    writeln!(out, "  Cursor / any MCP client (JSON):")?;
    writeln!(
        out,
        "    {{ \"mcpServers\": {{ \"ocean\": {{ \"command\": \"{exe}\", \"args\": [\"serve\"] }} }} }}"
    )?;
    writeln!(out)?;
    writeln!(
        out,
        "Then run `{exe} doctor` to confirm the daemon is reachable."
    )?;
    Ok(())
}

async fn doctor(daemon: &Daemon) -> Result<()> {
    let mut out = std::io::stdout();
    match daemon.get("/health").await {
        Ok(v) => writeln!(
            out,
            "daemon: ok at {} (backend {}, rev {})",
            daemon.base,
            v["backend"].as_str().unwrap_or("?"),
            v["rev"].as_str().unwrap_or("?")
        )?,
        Err(err) => {
            writeln!(out, "daemon: NOT reachable at {} — {err:#}", daemon.base)?;
            writeln!(
                out,
                "start it with `ocean-daemon` (or the launchd job) and try again"
            )?;
            return Ok(());
        }
    }
    writeln!(
        out,
        "member id: {} (override with --member or OCEAN_MEMBER_ID)",
        daemon.member
    )?;
    match daemon.get("/v1/rooms/persistent").await {
        Ok(v) => {
            let n = v["rooms"].as_array().map(|r| r.len()).unwrap_or(0);
            writeln!(out, "rooms: {n}")?;
        }
        Err(err) => writeln!(out, "rooms: could not list — {err:#}")?,
    }
    let home = home()?;
    for target in skill_targets(&home) {
        writeln!(
            out,
            "skill {}: {}",
            target.display(),
            if target.exists() {
                "installed"
            } else {
                "missing (run `ocean-mcp setup`)"
            }
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, routing::post, Json, Router};
    use std::sync::{Arc, Mutex};

    #[test]
    fn tool_definitions_are_well_formed_and_unique() {
        let tools = tool_definitions();
        let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), tools.len());
        for t in &tools {
            assert!(t["name"].as_str().unwrap().starts_with("ocean_"));
            assert!(!t["description"].as_str().unwrap().is_empty());
            assert_eq!(t["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn system_rows_render_as_labels_and_long_bodies_truncate() {
        let audit = json!({"seq": 7, "author_id": "system", "kind": "system",
            "body": "{\"type\":\"room.agent.admission\",\"decision_id\":\"secret-ish\"}"});
        assert_eq!(render_row(&audit), "#7 system: [room.agent.admission]");
        let long = json!({"seq": 8, "author_id": "john", "kind": "message", "body": "x".repeat(2000), "thread_parent_seq": 3});
        let rendered = render_row(&long);
        assert!(rendered.starts_with("#8 john (reply to #3): "));
        assert!(rendered.ends_with("…[truncated]"));
        assert!(rendered.chars().count() < 700);
    }

    #[test]
    fn skill_targets_only_use_existing_tool_roots() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".claude")).unwrap();
        let targets = skill_targets(tmp.path());
        assert_eq!(targets.len(), 1);
        assert!(targets[0].ends_with(".claude/skills/ocean/SKILL.md"));
        assert!(SKILL_MD.starts_with("---\nname: ocean\n"));
    }

    /// A stub daemon: enough of the routes for the bridge to be exercised
    /// end to end, plus a recorder for the one write.
    async fn stub_daemon() -> (String, Arc<Mutex<Vec<Value>>>) {
        let posted: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = posted.clone();
        let app = Router::new()
            .route("/health", get(|| async { Json(json!({"ok": true, "backend": "fake/model", "rev": "abc"})) }))
            .route("/v1/rooms/persistent", get(|| async {
                Json(json!({"rooms": [{"id": "campaigns", "name": "Campaigns", "participants": [
                    {"id": "smaths", "kind": "human"}, {"id": "room-builder", "kind": "agent"}]}]}))
            }))
            .route("/v1/rooms/persistent/{key}/snapshot", get(|| async {
                Json(json!({"transcript": [
                    {"seq": 1, "author_id": "smaths", "kind": "message", "body": "hello"},
                    {"seq": 2, "author_id": "system", "kind": "system", "body": "{\"type\":\"room.profile.updated\"}"}
                ], "has_more": false}))
            }))
            .route("/v1/rooms/persistent/{key}/messages", post(move |Json(body): Json<Value>| {
                let recorder = recorder.clone();
                async move {
                    recorder.lock().unwrap().push(body);
                    Json(json!({"ok": true, "message": {"seq": 3}, "triggers_fired": ["room-builder"]}))
                }
            }))
            .route("/v1/prompt", post(|Json(body): Json<Value>| async move {
                Json(json!({"ok": true, "session_id": "sess-1", "stdout": format!("echo: {}", body["prompt"].as_str().unwrap_or("")), "stderr": "", "cwd": body["cwd"]}))
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), posted)
    }

    #[tokio::test]
    async fn handshake_list_and_call_flow_against_a_stub_daemon() {
        let (base, posted) = stub_daemon().await;
        let daemon = Daemon::new(&base, "smaths");

        let init: Incoming = serde_json::from_value(
            json!({"id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}),
        )
        .unwrap();
        let reply = handle(&daemon, init).await.unwrap();
        assert_eq!(reply["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(reply["result"]["serverInfo"]["name"], "ocean");

        let note: Incoming =
            serde_json::from_value(json!({"method": "notifications/initialized"})).unwrap();
        assert!(
            handle(&daemon, note).await.is_none(),
            "notifications get no reply"
        );

        let list: Incoming =
            serde_json::from_value(json!({"id": 2, "method": "tools/list"})).unwrap();
        let reply = handle(&daemon, list).await.unwrap();
        assert!(reply["result"]["tools"].as_array().unwrap().len() >= 9);

        let call = |id: u64, name: &str, args: Value| -> Incoming {
            serde_json::from_value(json!({"id": id, "method": "tools/call", "params": {"name": name, "arguments": args}})).unwrap()
        };
        let reply = handle(&daemon, call(3, "ocean_rooms", json!({})))
            .await
            .unwrap();
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("campaigns") && text.contains("room-builder (agent)"),
            "{text}"
        );
        assert_eq!(reply["result"]["isError"], false);

        let reply = handle(
            &daemon,
            call(4, "ocean_room_read", json!({"room": "campaigns"})),
        )
        .await
        .unwrap();
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("#1 smaths: hello") && text.contains("[room.profile.updated]"),
            "{text}"
        );

        let reply = handle(
            &daemon,
            call(
                5,
                "ocean_room_post",
                json!({"room": "campaigns", "body": "@room-builder hi"}),
            ),
        )
        .await
        .unwrap();
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("posted #3") && text.contains("woke 1 agent"),
            "{text}"
        );
        {
            let recorded = posted.lock().unwrap();
            assert_eq!(recorded[0]["author_id"], "smaths");
            assert_eq!(recorded[0]["author_kind"], "human");
        }

        let reply = handle(
            &daemon,
            call(
                6,
                "ocean_prompt",
                json!({"prompt": "say hi", "cwd": "/tmp"}),
            ),
        )
        .await
        .unwrap();
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("session_id: sess-1") && text.contains("echo: say hi"),
            "{text}"
        );

        // A bad tool and a missing argument are tool errors, not protocol errors.
        let reply = handle(&daemon, call(7, "ocean_room_read", json!({})))
            .await
            .unwrap();
        assert_eq!(reply["result"]["isError"], true);
        assert!(reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("missing argument: room"));
        let reply = handle(&daemon, call(8, "nope", json!({}))).await.unwrap();
        assert_eq!(reply["result"]["isError"], true);

        let unknown: Incoming =
            serde_json::from_value(json!({"id": 9, "method": "resources/read"})).unwrap();
        let reply = handle(&daemon, unknown).await.unwrap();
        assert_eq!(reply["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn an_unreachable_daemon_is_a_tool_error_that_names_the_url() {
        let daemon = Daemon::new("http://127.0.0.1:9", "smaths");
        let call: Incoming = serde_json::from_value(json!({"id": 1, "method": "tools/call", "params": {"name": "ocean_health", "arguments": {}}})).unwrap();
        let reply = handle(&daemon, call).await.unwrap();
        assert_eq!(reply["result"]["isError"], true);
        assert!(reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("127.0.0.1:9"));
    }
}
