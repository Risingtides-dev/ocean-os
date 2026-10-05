//! Explicit live Max-effort acceptance through isolated, tool-free Ocean turns.
use futures::StreamExt;
use ocean_agent::{AgentRuntime, PromptControl};
use ocean_core::PromptRequest;
use ocean_protocol::ThinkingLevel;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    anyhow::ensure!(
        std::env::var("OCEAN_PROVIDER_FALLBACK").as_deref() == Ok(""),
        "set OCEAN_PROVIDER_FALLBACK='' to prevent a fallback from masking failure"
    );
    anyhow::ensure!(
        std::env::var("OCEAN_MODEL").is_ok_and(|model| !model.is_empty()),
        "set OCEAN_MODEL to a valid bootstrap model for the isolated runtime"
    );
    let models: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(!models.is_empty(), "usage: max_effort_smoke <model-id>...");
    let probes = models.into_iter().flat_map(|model| {
        [ThinkingLevel::Max]
            .into_iter()
            .map(move |effort| (model.clone(), effort))
    });
    futures::stream::iter(probes)
        .for_each_concurrent(4, |(model, effort)| async move {
            let stage = std::cell::Cell::new("temporary_store");
            let result = async {
                let root = tempfile::tempdir()?;
                let workspace = root.path().join("workspace");
                std::fs::create_dir(&workspace)?;
                stage.set("runtime_construction");
                let runtime = AgentRuntime::with_config_dir(root.path().join("config"))?;
                stage.set("request_decode");
                let request: PromptRequest = serde_json::from_value(serde_json::json!({
                    "prompt":"Reply with exactly OCEAN_OK.",
                    "cwd":workspace.to_string_lossy(),
                    "max_turns":1,
                    "client_type":"cli"
                }))?;
                let control = PromptControl::yolo(false)
                    .without_tools()
                    .without_operator_memory()
                    .with_model_id(Some(model.clone()))
                    .with_thinking_level(Some(effort));
                stage.set("turn");
                let response = runtime.prompt(request, control).await;
                Ok::<_, anyhow::Error>(response.ok && response.stdout.trim() == "OCEAN_OK")
            };
            let outcome = match tokio::time::timeout(Duration::from_secs(45), result).await {
                Ok(Ok(true)) => "passed",
                Ok(Ok(false)) => "turn_failed_or_unexpected_reply",
                Ok(Err(_)) => "configuration_error",
                Err(_) => "timeout",
            };
            println!(
                "{}",
                serde_json::json!({"model":model,"effort":effort,"outcome":outcome,"stage":stage.get()})
            );
        })
        .await;
    Ok(())
}
