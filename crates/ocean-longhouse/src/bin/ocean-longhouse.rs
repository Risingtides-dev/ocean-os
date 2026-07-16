use std::net::SocketAddr;

use anyhow::Context;
use axum::{routing::get, routing::post, Json, Router};
use clap::{Parser, Subcommand};
use ocean_longhouse::{
    convene, ConveneRequest, Federation, LonghouseConfig, LonghouseEvent, LonghouseMode,
    ProposalTally, SystemClock,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(name = "ocean-longhouse")]
#[command(about = "Ocean Longhouse coordination service")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run Longhouse as a local HTTP service.
    Serve {
        /// Bind address for the local Longhouse service.
        #[arg(long, env = "OCEAN_LONGHOUSE_BIND", default_value = "127.0.0.1:4781")]
        bind: SocketAddr,
    },
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    ok: bool,
    service: &'static str,
    mode: LonghouseMode,
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CouncilConveneHttpRequest {
    question: String,
    #[serde(default = "default_federation")]
    federation: Federation,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    max_rounds: Option<usize>,
    #[serde(default)]
    deadline_ms_from_now: Option<i64>,
}

#[derive(Debug, Serialize)]
struct CouncilConveneHttpResponse {
    topic_id: Uuid,
    board_id: Uuid,
    decision: Option<Uuid>,
    convergence_basis: Option<&'static str>,
    tallies: Vec<ProposalTally>,
    proposals: HashMap<Uuid, String>,
    events: Vec<LonghouseEvent>,
}

fn default_federation() -> Federation {
    Federation::Commons
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ocean_longhouse=info,tower_http=info".into()),
        )
        .init();

    match Cli::parse().command {
        Command::Serve { bind } => serve(bind).await,
    }
}

async fn serve(bind: SocketAddr) -> anyhow::Result<()> {
    let config = LonghouseConfig::from_env();
    let app = Router::new()
        .route("/health", get(move || health(config.clone())))
        .route("/v1/council/convene", post(council_convene));

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind ocean-longhouse on {bind}"))?;

    tracing::info!(%bind, "ocean-longhouse listening");
    axum::serve(listener, app)
        .await
        .context("ocean-longhouse server failed")
}

async fn health(config: LonghouseConfig) -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        service: "ocean-longhouse",
        mode: config.mode,
        url: config.url,
    })
}

async fn council_convene(
    Json(req): Json<CouncilConveneHttpRequest>,
) -> Json<CouncilConveneHttpResponse> {
    let mut convene_req = ConveneRequest::new(req.question, req.federation);
    if !req.models.is_empty() {
        convene_req.models = req.models;
    }
    if let Some(max_rounds) = req.max_rounds {
        convene_req.max_rounds = max_rounds;
    }
    if let Some(deadline_ms_from_now) = req.deadline_ms_from_now {
        convene_req.deadline_ms_from_now = deadline_ms_from_now;
    }

    let clock = SystemClock;
    let mut events = Vec::new();
    let outcome = convene(convene_req, &clock, |ev| events.push(ev)).await;

    Json(CouncilConveneHttpResponse {
        topic_id: outcome.topic_id,
        board_id: outcome.board_id,
        decision: outcome.decision,
        convergence_basis: outcome.convergence_basis.map(|basis| basis.as_str()),
        tallies: outcome.tallies,
        proposals: outcome.proposals,
        events,
    })
}
