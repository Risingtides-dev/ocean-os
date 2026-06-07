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
            let req = PromptRequest {
                request_id: None,
                prompt,
                images: None,
                session_id: None,
                create_if_missing: true,
                max_turns,
                yolo,
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
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocean_core::TokenUsage;

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
}
