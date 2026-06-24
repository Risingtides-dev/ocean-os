use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use ocean_core::{
    EventEnvelope, HealthResponse, OceanEvent, PermissionDecision, PermissionDecisionRequest,
    PromptRequest, PromptResponse, SessionId, SessionResponse,
};
use std::io::{IsTerminal, Write};

/// How the CLI answers a daemon `PermissionRequest` when it is NOT attached to
/// an interactive terminal (piped / CI). In a TTY the operator is prompted
/// directly and this mode is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "lower")]
enum PermissionMode {
    /// Prompt interactively if a TTY is present; otherwise DENY (safe default).
    #[default]
    Prompt,
    /// Auto-allow every gated tool call for the rest of the run (allow_session).
    Allow,
    /// Auto-deny every gated tool call.
    Deny,
}

/// Parse `OCEAN_CLI_PERMISSION` into a [`PermissionMode`]. Unknown values fall
/// back to `Prompt` (the safe interactive-or-deny default).
fn permission_mode_from_env() -> Option<PermissionMode> {
    let raw = std::env::var("OCEAN_CLI_PERMISSION").ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "prompt" => Some(PermissionMode::Prompt),
        "allow" => Some(PermissionMode::Allow),
        "deny" => Some(PermissionMode::Deny),
        _ => None,
    }
}

/// The pure policy core (unit-tested): given the configured mode and whether
/// stdin is an interactive TTY, decide how a gated tool call resolves WITHOUT
/// any interactive prompt.
///
/// Returns `None` only for `Prompt` + TTY, which means "ask the operator"
/// (handled by the caller, which reads stdin). Every non-interactive path
/// resolves deterministically here:
/// - `Prompt` + no TTY  -> Deny   (never silent-allow)
/// - `Allow`            -> AllowSession
/// - `Deny`             -> Deny
fn resolve_permission(mode: PermissionMode, is_tty: bool) -> Option<PermissionDecision> {
    match mode {
        PermissionMode::Allow => Some(PermissionDecision::AllowSession),
        PermissionMode::Deny => Some(PermissionDecision::Deny {
            reason: Some("denied by --permission-mode deny".into()),
        }),
        PermissionMode::Prompt => {
            if is_tty {
                None
            } else {
                Some(PermissionDecision::Deny {
                    reason: Some(
                        "no approval path in non-interactive mode; pass --permission-mode allow or run interactively".into(),
                    ),
                })
            }
        }
    }
}

fn resolve_cwd(project: Option<&str>) -> String {
    // OCEAN_PROJECT env var takes lowest precedence among explicit overrides
    if let Some(path) = project.filter(|p| !p.is_empty()) {
        return path.to_string();
    }
    if let Ok(path) = std::env::var("OCEAN_PROJECT") {
        if !path.is_empty() {
            return path;
        }
    }
    std::env::current_dir()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

#[derive(Debug, Parser)]
#[command(name = "ocean-rs", about = "Ocean OS agent runtime client")]
struct Cli {
    #[arg(
        long,
        env = "OCEAN_DAEMON_URL",
        default_value = "http://127.0.0.1:4780"
    )]
    url: String,
    /// Working directory / project root for the session.
    /// Overrides OCEAN_PROJECT env var and current directory.
    #[arg(long, env = "OCEAN_PROJECT")]
    project: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    Health,
    Prompt {
        prompt: Vec<String>,
        /// Auto-approve all tool calls daemon-side (sets the legacy `yolo` bit on
        /// the turn). Must be EXPLICIT — equivalent to `--permission-mode allow`
        /// but skips the gate entirely in the daemon rather than answering each
        /// prompt. Never silent: you have to ask for it.
        #[arg(long)]
        yolo: bool,
        /// How to answer gated tool calls when NOT attached to a TTY (piped/CI).
        /// In an interactive terminal you are prompted regardless. Falls back to
        /// the `OCEAN_CLI_PERMISSION` env var, then defaults to `prompt`
        /// (interactive-or-deny). Non-interactive default is DENY — never a
        /// silent allow.
        #[arg(long, value_enum)]
        permission_mode: Option<PermissionMode>,
        #[arg(long)]
        max_turns: Option<u32>,
    },
    Sessions,
    Session {
        id: SessionId,
    },
    /// Onboard onto Ocean: verify a bedrock API token and print what to set.
    /// The interactive initiation surface — run it with no flags and it walks
    /// you through entering the URL + token; or pass `--bedrock-url`/`--token`
    /// (or set OCEAN_BEDROCK_URL / OCEAN_BEDROCK_TOKEN). Hits
    /// `<bedrock-url>/api/v1/info`, and on success prints the resolved principal
    /// plus the two env exports to add to your shell.
    Onboard {
        /// ocean-bedrock base URL (e.g. http://localhost:8080). Prompted if absent.
        #[arg(long, env = "OCEAN_BEDROCK_URL")]
        bedrock_url: Option<String>,
        /// The bedrock API token (admin issues via `npm run token:create`).
        /// Prompted if absent.
        #[arg(long, env = "OCEAN_BEDROCK_TOKEN")]
        token: Option<String>,
    },
}

/// Resolve an onboarding value: use the supplied one (trimmed), else prompt for
/// it interactively on a TTY. A piped/CI invocation with no value errors rather
/// than hanging on stdin — onboarding is interactive-or-explicit, never silent.
fn resolve_or_prompt(provided: Option<String>, label: &str) -> anyhow::Result<String> {
    if let Some(v) = provided {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Ok(v);
        }
    }
    anyhow::ensure!(
        std::io::stdin().is_terminal(),
        "{label} required — pass the flag or set the env var (no TTY to prompt)"
    );
    eprint!("[ocean onboard] enter {label}: ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let v = line.trim().to_string();
    anyhow::ensure!(!v.is_empty(), "{label} cannot be empty");
    Ok(v)
}

/// Build the onboarding log record (the client-side "log trip"): when, which
/// box, and the resolved principal. Pure so it's unit-testable; `ts_ms` is the
/// caller's clock.
fn onboarding_record(
    bedrock_url: &str,
    info: &serde_json::Value,
    ts_ms: u128,
) -> serde_json::Value {
    serde_json::json!({
        "ts_ms": ts_ms,
        "event": "onboard",
        "bedrock_url": bedrock_url.trim_end_matches('/'),
        "instance": info["instance"].as_str(),
        "principal": info["principal"].clone(),
    })
}

/// Where the local onboarding log lives: `$OCEAN_ONBOARD_LOG`, else
/// `$HOME/.local/state/ocean/onboarding.jsonl` (XDG state convention). `None`
/// when no home is resolvable and no override is set.
fn onboarding_log_path() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("OCEAN_ONBOARD_LOG") {
        return Some(std::path::PathBuf::from(p));
    }
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
    Some(std::path::Path::new(&home).join(".local/state/ocean/onboarding.jsonl"))
}

/// Append the onboarding record as one JSONL line. Fail-soft — a logging error
/// is warned to stderr and never propagated, so it can't fail a good onboard.
fn record_onboarding(bedrock_url: &str, info: &serde_json::Value) {
    let Some(path) = onboarding_log_path() else {
        return;
    };
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let line = onboarding_record(bedrock_url, info, ts_ms).to_string();
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(f, "{line}")
    };
    if let Err(e) = write() {
        eprintln!(
            "[ocean-rs] onboarding logged-trip write failed ({}): {e}",
            path.display()
        );
    }
}

/// Render the onboarding success summary from a bedrock `/api/v1/info` body:
/// the resolved principal + the two env exports a coworker should persist.
/// Pure (no I/O) so it's unit-testable.
fn format_onboard_summary(base_url: &str, token: &str, info: &serde_json::Value) -> String {
    let p = &info["principal"];
    let name = p["name"].as_str().unwrap_or("?");
    let role = p["role"].as_str().unwrap_or("?");
    let scopes = p["scopes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let instance = info["instance"].as_str().unwrap_or("ocean-bedrock");
    format!(
        "ok connected to {instance} (api {})\n  principal: {name} (role={role}, scopes=[{scopes}])\n\nAdd these to your shell profile to persist:\n  export OCEAN_BEDROCK_URL=\"{}\"\n  export OCEAN_BEDROCK_TOKEN=\"{}\"",
        info["apiVersion"].as_str().unwrap_or("v1"),
        base_url.trim_end_matches('/'),
        token,
    )
}

/// Build the trailing footer line printed after a prompt's output.
/// Mirrors the existing `[ocean-rs: ...]` style and always reports token
/// usage so operators have cost visibility (zeros included when the provider
/// reported none).
fn usage_footer(res: &PromptResponse) -> String {
    let u = &res.usage;
    format!(
        "\n[ocean-rs: ok={} wall={}ms rss=daemon tokens: in={} out={} cache_read={} cache_write={} total={}]",
        res.ok, res.wall_ms, u.input, u.output, u.cache_read, u.cache_write, u.total_tokens
    )
}

/// Decide the process exit semantics for a prompt response: a daemon that
/// returns HTTP 200 with `ok:false` is still a failed turn, so surface it as an
/// error (the caller prints stdout first so output isn't lost).
fn check_response(res: &PromptResponse) -> anyhow::Result<()> {
    anyhow::ensure!(res.ok, "daemon reported error: {}", res.stderr.trim());
    Ok(())
}

fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        let safe = byte.is_ascii_alphanumeric()
            || byte == b'-'
            || byte == b'_'
            || byte == b'.'
            || byte == b'~';
        if safe {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// POST a decision to the daemon's `/v1/permissions/{id}/decision` endpoint,
/// releasing the runtime waiter the gated tool call is blocked on.
async fn post_decision(
    client: &reqwest::Client,
    base_url: &str,
    permission_id: uuid::Uuid,
    decision: PermissionDecision,
    decision_token: Option<String>,
) -> anyhow::Result<()> {
    let body = PermissionDecisionRequest {
        permission_id,
        decision,
        // OCEAN-185: replay the per-turn secret we minted at submit time, proving
        // this decision came from the turn's submitter (the daemon 403s otherwise).
        decision_token,
    };
    let url = format!(
        "{}/v1/permissions/{}/decision",
        base_url.trim_end_matches('/'),
        permission_id
    );
    client
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?
        .error_for_status()
        .context("daemon rejected the permission decision")?;
    Ok(())
}

/// Interactively ask the operator how to resolve a gated tool call. Reads a
/// single line from stdin; `[a]llow once / allow [s]ession / [d]eny` (default
/// deny on EOF/empty/unknown). Blocking stdin is fine here — the SSE task is a
/// separate tokio task and the main turn is just awaiting the HTTP response.
fn prompt_operator(tool: &str, reason: &str) -> PermissionDecision {
    eprint!(
        "\n[ocean-rs] tool '{tool}' wants to run: {reason}\n  [a]llow once / allow [s]ession / [d]eny (default: deny) > "
    );
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return PermissionDecision::Deny {
            reason: Some("operator input unavailable".into()),
        };
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "a" | "allow" | "y" | "yes" => PermissionDecision::Allow,
        "s" | "session" => PermissionDecision::AllowSession,
        _ => PermissionDecision::Deny {
            reason: Some("denied by operator".into()),
        },
    }
}

/// Watch the daemon's `/v1/events` SSE feed for `PermissionRequest`s scoped to
/// our turn's `request_id` and resolve each one (TTY prompt or policy default),
/// POSTing the decision so the blocked turn unblocks.
///
/// Mirrors the ACP permission bridge (OCEAN-146): subscribe BEFORE the turn is
/// submitted so a gated tool call's `PermissionRequest` is never missed even
/// though `/v1/prompt` stays blocked the whole time. Because the CLI mints its
/// own `request_id` and sets it on the request, we already know what to match —
/// no need to wait for a `TurnStarted` to learn it.
///
/// Runs until the daemon emits a terminal event (`TurnFinished` / `Cancelled` /
/// `Error`) for our turn or the stream closes; the caller also aborts it once
/// the HTTP turn returns.
async fn permission_bridge(
    client: reqwest::Client,
    base_url: String,
    request_id: uuid::Uuid,
    // OCEAN-185: the per-turn secret this CLI minted and sent on the turn. Each
    // decision POST must replay it or the daemon rejects it 403.
    decision_token: String,
    mode: PermissionMode,
    is_tty: bool,
    resp: reqwest::Response,
) {
    let mut resp = resp;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let chunk = match resp.chunk().await {
            Ok(Some(c)) => c,
            Ok(None) => return, // stream closed
            Err(err) => {
                eprintln!("[ocean-rs] permission event stream error: {err}");
                return;
            }
        };
        buf.extend_from_slice(&chunk);

        // Drain complete lines from the buffer; SSE frames are newline-delimited
        // `field: value` lines and we only care about `data:` payloads (each is
        // a fully self-describing EventEnvelope).
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(rest) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = rest.trim();
            if payload.is_empty() {
                continue;
            }
            let envelope: EventEnvelope = match serde_json::from_str(payload) {
                Ok(ev) => ev,
                // Keep-alives / unknown frames (`{"type":"error",...}`) — skip.
                Err(_) => continue,
            };

            // The control feed is global; scope to OUR turn.
            if envelope.request_id != Some(request_id) {
                continue;
            }

            match &envelope.event {
                OceanEvent::PermissionRequest { tool, reason, .. } => {
                    let Some(permission_id) = envelope.permission_id else {
                        continue;
                    };
                    let decision = match resolve_permission(mode, is_tty) {
                        Some(d) => d,
                        // Prompt + TTY: ask the operator.
                        None => prompt_operator(tool, reason),
                    };
                    if matches!(decision, PermissionDecision::Deny { .. }) && !is_tty {
                        eprintln!(
                            "[ocean-rs] tool '{tool}' denied: no approval path in non-interactive mode; pass --permission-mode allow or run interactively"
                        );
                    }
                    if let Err(err) = post_decision(
                        &client,
                        &base_url,
                        permission_id,
                        decision,
                        Some(decision_token.clone()),
                    )
                    .await
                    {
                        eprintln!("[ocean-rs] failed to POST permission decision: {err}");
                    }
                }
                OceanEvent::TurnFinished { .. }
                | OceanEvent::Cancelled { .. }
                | OceanEvent::Error { .. } => {
                    return;
                }
                _ => {}
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let client = reqwest::Client::new();
    match cli.cmd {
        Cmd::Health => {
            let res: HealthResponse = client
                .get(format!("{}/health", cli.url))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            println!(
                "{} {} backend={}",
                if res.ok { "ok" } else { "error" },
                res.service,
                res.backend
            );
        }
        Cmd::Prompt {
            prompt,
            yolo,
            permission_mode,
            max_turns,
        } => {
            let prompt = prompt.join(" ");
            anyhow::ensure!(!prompt.trim().is_empty(), "prompt required");

            // CLI mints the turn's request_id so the permission bridge knows
            // exactly which `PermissionRequest`s on the global control feed are
            // ours — no need to wait for a `TurnStarted` to learn it.
            let request_id = uuid::Uuid::new_v4();
            // OCEAN-185 (P0): mint the per-turn permission secret. It rides the
            // turn body and is replayed on every decision POST; the daemon binds
            // the gate to it and never broadcasts it on /v1/events, so a localhost
            // page that sniffs the permission_id can't approve our gated tools.
            let decision_token = ocean_core::mint_decision_token();
            let mode = permission_mode
                .or_else(permission_mode_from_env)
                .unwrap_or_default();
            // stdin TTY drives interactivity: a piped/CI invocation can't answer
            // a prompt, so it falls back to the policy default (DENY unless an
            // explicit allow mode / --yolo was given).
            let is_tty = std::io::stdin().is_terminal();

            // Subscribe to the control event stream BEFORE submitting the turn so
            // a gated tool call's PermissionRequest is never missed while
            // /v1/prompt blocks (yolo turns never gate, but the subscription is
            // harmless and self-terminates on TurnFinished). Skipped under
            // --yolo: the daemon auto-approves, so there's nothing to bridge.
            let bridge = if yolo {
                None
            } else {
                match client
                    .get(format!("{}/v1/events", cli.url.trim_end_matches('/')))
                    .header("Accept", "text/event-stream")
                    .send()
                    .await
                    .and_then(|r| r.error_for_status())
                {
                    Ok(resp) => Some(tokio::spawn(permission_bridge(
                        client.clone(),
                        cli.url.clone(),
                        request_id,
                        decision_token.clone(),
                        mode,
                        is_tty,
                        resp,
                    ))),
                    Err(err) => {
                        eprintln!(
                            "[ocean-rs] warning: could not subscribe to permission events ({err}); gated tool calls may hang"
                        );
                        None
                    }
                }
            };

            let req = PromptRequest {
                request_id: Some(request_id),
                prompt,
                images: None,
                session_id: None,
                create_if_missing: true,
                max_turns,
                yolo,
                cwd: resolve_cwd(cli.project.as_deref()),
                project_id: None,
                client_type: Some("cli".into()),
                // OCEAN-185: bind this turn's permission gate to us. Sent even
                // under yolo (harmless — no gate fires); required when gated.
                decision_token: Some(decision_token.clone()),
            };
            let res: anyhow::Result<PromptResponse> = async {
                Ok(client
                    .post(format!("{}/v1/prompt", cli.url))
                    .json(&req)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?)
            }
            .await;

            // The turn returned (or failed): tear the bridge down regardless.
            if let Some(handle) = bridge {
                handle.abort();
            }
            let res = res?;

            if !res.stderr.trim().is_empty() {
                eprintln!("{}", res.stderr.trim_end());
            }
            print!("{}", res.stdout);
            eprintln!("{}", usage_footer(&res));
            // Print output above before failing so it isn't lost when ok:false.
            check_response(&res)?;
        }
        Cmd::Sessions => {
            let cwd = resolve_cwd(cli.project.as_deref());
            let url = format!(
                "{}/v1/sessions?cwd={}",
                cli.url.trim_end_matches('/'),
                urlencoding(&cwd)
            );
            let text = client
                .get(url)
                .send()
                .await?
                .error_for_status()?
                .text()
                .await
                .context("read sessions response")?;
            println!("{text}");
        }
        Cmd::Session { id } => {
            let response = client
                .get(format!("{}/v1/sessions/{id}", cli.url))
                .send()
                .await?;
            let status = response.status();
            let body: SessionResponse = response.json().await.context("read session response")?;
            anyhow::ensure!(
                status.is_success() && body.ok,
                "{}",
                body.error
                    .unwrap_or_else(|| format!("session request failed: {status}"))
            );
            println!("{}", serde_json::to_string_pretty(&body)?);
        }
        Cmd::Onboard { bedrock_url, token } => {
            let bedrock_url = resolve_or_prompt(
                bedrock_url,
                "ocean-bedrock URL (e.g. http://localhost:8080)",
            )?;
            let token = resolve_or_prompt(token, "bedrock API token")?;
            let url = format!("{}/api/v1/info", bedrock_url.trim_end_matches('/'));
            let resp = client
                .get(&url)
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .with_context(|| format!("GET {url} (is ocean-bedrock running / URL right?)"))?;
            let status = resp.status();
            let body: serde_json::Value =
                resp.json().await.context("read bedrock /info response")?;
            anyhow::ensure!(
                status.is_success() && body["ok"] != serde_json::Value::Bool(false),
                "bedrock rejected the token ({status}): {}",
                body["error"].as_str().unwrap_or("unknown error")
            );
            println!("{}", format_onboard_summary(&bedrock_url, &token, &body));
            // Leave a local "log trip": record the successful onboarding (which
            // box, which principal, when) so a coworker has a trail of what they
            // connected to — the client-side complement to bedrock's token-issue
            // audit. Fail-soft: a logging failure never fails the onboard.
            record_onboarding(&bedrock_url, &body);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocean_core::TokenUsage;

    #[test]
    fn onboarding_record_captures_box_and_principal() {
        let info = serde_json::json!({
            "instance": "ocean-bedrock",
            "principal": { "name": "alice", "role": "agent", "scopes": ["/docs"] }
        });
        let rec = onboarding_record("http://localhost:8080/", &info, 1_700_000_000_000);
        assert_eq!(rec["event"], "onboard");
        assert_eq!(rec["ts_ms"], 1_700_000_000_000u64);
        // URL is trimmed of the trailing slash.
        assert_eq!(rec["bedrock_url"], "http://localhost:8080");
        assert_eq!(rec["instance"], "ocean-bedrock");
        assert_eq!(rec["principal"]["name"], "alice");
        assert_eq!(rec["principal"]["role"], "agent");
    }

    #[test]
    fn resolve_or_prompt_uses_provided_value_trimmed() {
        // A supplied value is used verbatim (trimmed) without touching stdin.
        assert_eq!(
            resolve_or_prompt(Some("  http://localhost:8080  ".into()), "url").unwrap(),
            "http://localhost:8080"
        );
    }

    #[test]
    fn onboard_summary_reports_principal_and_exports() {
        let info = serde_json::json!({
            "instance": "ocean-bedrock",
            "apiVersion": "v1",
            "principal": { "name": "alice", "role": "agent", "scopes": ["/docs", "/context"] }
        });
        let out = format_onboard_summary("http://localhost:8080/", "tok_abc", &info);
        assert!(out.contains("ocean-bedrock"), "names the instance: {out}");
        assert!(out.contains("alice"), "names the principal: {out}");
        assert!(out.contains("role=agent"), "shows the role: {out}");
        assert!(out.contains("/docs, /context"), "lists scopes: {out}");
        // base url is trimmed of the trailing slash in the export line
        assert!(
            out.contains("export OCEAN_BEDROCK_URL=\"http://localhost:8080\""),
            "emits the url export: {out}"
        );
        assert!(
            out.contains("export OCEAN_BEDROCK_TOKEN=\"tok_abc\""),
            "emits the token export: {out}"
        );
    }

    fn response(ok: bool) -> PromptResponse {
        PromptResponse {
            ok,
            request_id: None,
            session_id: None,
            code: None,
            wall_ms: 1234,
            stdout: "out".into(),
            stderr: "boom".into(),
            cwd: String::new(),
            usage: TokenUsage {
                input: 10,
                output: 20,
                cache_read: 5,
                cache_write: 3,
                total_tokens: 38,
            },
        }
    }

    #[test]
    fn check_response_errors_on_ok_false() {
        let res = response(false);
        let err = check_response(&res).expect_err("ok:false must be an error");
        assert!(err.to_string().contains("daemon reported error"));
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn check_response_ok_on_ok_true() {
        let res = response(true);
        assert!(check_response(&res).is_ok());
    }

    #[test]
    fn usage_footer_includes_token_fields() {
        let footer = usage_footer(&response(true));
        assert!(footer.contains("ok=true"));
        assert!(footer.contains("wall=1234ms"));
        assert!(footer.contains("in=10"));
        assert!(footer.contains("out=20"));
        assert!(footer.contains("cache_read=5"));
        assert!(footer.contains("cache_write=3"));
        assert!(footer.contains("total=38"));
    }

    // --- permission policy core (OCEAN-212) --------------------------------
    //
    // `resolve_permission` is the testable heart of the approval path: it must
    // NEVER silent-allow a gated tool call in a non-interactive context. These
    // assertions pin that contract.

    #[test]
    fn non_interactive_no_flag_denies() {
        // Default mode (Prompt) without a TTY must DENY, never silent-allow.
        let d = resolve_permission(PermissionMode::Prompt, false)
            .expect("non-interactive prompt mode must resolve to a decision, not ask");
        assert!(matches!(d, PermissionDecision::Deny { .. }));
    }

    #[test]
    fn interactive_prompt_defers_to_operator() {
        // Prompt + TTY returns None: caller must ask the operator via stdin.
        assert!(resolve_permission(PermissionMode::Prompt, true).is_none());
    }

    #[test]
    fn allow_mode_allows_session_regardless_of_tty() {
        for tty in [true, false] {
            let d = resolve_permission(PermissionMode::Allow, tty)
                .expect("allow mode resolves without asking");
            assert!(matches!(d, PermissionDecision::AllowSession));
        }
    }

    #[test]
    fn deny_mode_denies_regardless_of_tty() {
        for tty in [true, false] {
            let d = resolve_permission(PermissionMode::Deny, tty)
                .expect("deny mode resolves without asking");
            assert!(matches!(d, PermissionDecision::Deny { .. }));
        }
    }

    #[test]
    fn env_parses_known_values_and_rejects_garbage() {
        std::env::set_var("OCEAN_CLI_PERMISSION", "allow");
        assert_eq!(permission_mode_from_env(), Some(PermissionMode::Allow));
        std::env::set_var("OCEAN_CLI_PERMISSION", "DENY");
        assert_eq!(permission_mode_from_env(), Some(PermissionMode::Deny));
        std::env::set_var("OCEAN_CLI_PERMISSION", "prompt");
        assert_eq!(permission_mode_from_env(), Some(PermissionMode::Prompt));
        std::env::set_var("OCEAN_CLI_PERMISSION", "bananas");
        assert_eq!(permission_mode_from_env(), None);
        std::env::remove_var("OCEAN_CLI_PERMISSION");
        assert_eq!(permission_mode_from_env(), None);
    }

    #[test]
    fn permission_request_envelope_scoped_to_our_turn_decodes() {
        // Integration-shaped check: a real PermissionRequest envelope (the exact
        // shape the bridge filters on) must decode, carry our request_id, and a
        // scripted "allow" decision must produce the AllowSession wire body that
        // unblocks the daemon waiter.
        let request_id = uuid::Uuid::new_v4();
        let permission_id = uuid::Uuid::new_v4();
        let raw = serde_json::json!({
            "id": uuid::Uuid::new_v4(),
            "at": "2026-06-07T00:00:00Z",
            "request_id": request_id,
            "permission_id": permission_id,
            "type": "permission_request",
            "tool": "write_file",
            "reason": "modify /etc/hosts",
            "args": {"path": "/etc/hosts"}
        });
        let env: EventEnvelope = serde_json::from_value(raw).expect("decode envelope");
        assert_eq!(env.request_id, Some(request_id));
        assert_eq!(env.permission_id, Some(permission_id));
        assert!(matches!(env.event, OceanEvent::PermissionRequest { .. }));

        // Scripted allow (operator chose allow-session) → wire body the daemon
        // decision endpoint expects.
        let decision =
            resolve_permission(PermissionMode::Allow, true).expect("allow resolves without asking");
        let token = ocean_core::mint_decision_token();
        let body = PermissionDecisionRequest {
            permission_id,
            decision,
            decision_token: Some(token.clone()),
        };
        let wire = serde_json::to_value(&body).unwrap();
        assert_eq!(wire["decision"], "allow_session");
        assert_eq!(wire["permission_id"], permission_id.to_string());
        // OCEAN-185: the decision body carries the per-turn secret on the wire.
        assert_eq!(wire["decision_token"], token);
    }
}
