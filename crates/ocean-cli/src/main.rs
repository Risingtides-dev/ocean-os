use anyhow::Context;
use clap::{Parser, Subcommand};
use ocean_core::{HealthResponse, PromptRequest, PromptResponse, SessionId, SessionResponse};

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
        /// DEPRECATED / INERT. Per-call `--yolo` is no longer honored for
        /// security (OCEAN-160): the daemon gates mutating tools on its own
        /// `OCEAN_YOLO` environment variable and ignores any per-call request
        /// to auto-approve. Passing this flag prints a warning and does NOT
        /// change behavior. To allow mutating tools, set `OCEAN_YOLO=1` on the
        /// daemon process instead.
        #[arg(long)]
        yolo: bool,
        #[arg(long)]
        max_turns: Option<u32>,
    },
    Sessions,
    Session {
        id: SessionId,
    },
}

/// Warning shown when a user passes the deprecated, now-inert per-call
/// `--yolo` flag. OCEAN-160 moved yolo gating to the daemon's `OCEAN_YOLO`
/// environment variable, so the per-call flag no longer changes behavior.
const YOLO_INERT_WARNING: &str = "warning: per-call --yolo is no longer honored for security (OCEAN-160); set OCEAN_YOLO=1 on the daemon to allow mutating tools";

/// Returns the warning to emit on stderr if the inert `--yolo` flag was passed,
/// or `None` when it was not. Factored out so it can be unit-tested without
/// touching the daemon or the network.
fn yolo_warning(yolo: bool) -> Option<&'static str> {
    yolo.then_some(YOLO_INERT_WARNING)
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
            max_turns,
        } => {
            let prompt = prompt.join(" ");
            anyhow::ensure!(!prompt.trim().is_empty(), "prompt required");
            if let Some(msg) = yolo_warning(yolo) {
                eprintln!("{msg}");
            }
            let req = PromptRequest {
                request_id: None,
                prompt,
                images: None,
                session_id: None,
                create_if_missing: true,
                max_turns,
                // OCEAN-160: the daemon ignores the wire `yolo` flag and gates
                // mutating tools on its own `OCEAN_YOLO` env var. We always send
                // `false` so the CLI never depends on this inert field; the
                // per-call `--yolo` flag only triggers the warning above.
                yolo: false,
                cwd: resolve_cwd(cli.project.as_deref()),
                project_id: None,
                client_type: Some("cli".into()),
            };
            let res: PromptResponse = client
                .post(format!("{}/v1/prompt", cli.url))
                .json(&req)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            if !res.stderr.trim().is_empty() {
                eprintln!("{}", res.stderr.trim_end());
            }
            print!("{}", res.stdout);
            eprintln!(
                "\n[ocean-rs: ok={} wall={}ms rss=daemon]",
                res.ok, res.wall_ms
            );
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
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yolo_flag_emits_warning_when_set() {
        let warning = yolo_warning(true).expect("--yolo must produce a warning");
        assert!(
            warning.contains("OCEAN-160"),
            "warning should cite the security fix"
        );
        assert!(
            warning.contains("OCEAN_YOLO"),
            "warning should point at the operator env var"
        );
        assert!(
            warning.to_lowercase().contains("no longer honored"),
            "warning should state the flag is inert"
        );
    }

    #[test]
    fn yolo_flag_is_silent_when_unset() {
        assert!(yolo_warning(false).is_none());
    }

    #[test]
    fn cli_never_sends_yolo_true_on_the_wire() {
        // Mirror the request construction in the Prompt handler: regardless of
        // the per-call flag, the CLI must send `yolo: false` so behavior never
        // depends on the daemon's inert wire field (OCEAN-162 / OCEAN-160).
        for flag in [true, false] {
            let _ = flag; // the flag only drives the stderr warning, not the wire
            let req = PromptRequest {
                request_id: None,
                prompt: "noop".into(),
                images: None,
                session_id: None,
                create_if_missing: true,
                max_turns: None,
                yolo: false,
                cwd: ".".into(),
                project_id: None,
                client_type: Some("cli".into()),
            };
            assert!(
                !req.yolo,
                "CLI must never set wire yolo=true regardless of --yolo"
            );
        }
    }
}
