use anyhow::Context;
use clap::{Parser, Subcommand};
use ocean_core::{HealthResponse, PromptRequest, PromptResponse, SessionId, SessionResponse};

#[derive(Debug, Parser)]
#[command(name = "ocean-rs", about = "Ocean OS agent runtime client")]
struct Cli {
    #[arg(
        long,
        env = "OCEAN_DAEMON_URL",
        default_value = "http://127.0.0.1:4780"
    )]
    url: String,
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
                session_id: None,
                create_if_missing: true,
                max_turns,
                yolo,
                cwd: std::env::current_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
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
            let text = client
                .get(format!("{}/v1/sessions", cli.url))
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
