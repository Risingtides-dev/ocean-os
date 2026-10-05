//! Explicit live Max-effort acceptance through isolated, tool-free Ocean turns.
use futures::StreamExt;
use ocean_agent::{AgentRuntime, PromptControl};
use ocean_core::PromptRequest;
use ocean_protocol::ThinkingLevel;
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    anyhow::ensure!(
        std::env::var_os("OCEAN_PROVIDER").is_none(),
        "unset OCEAN_PROVIDER so probes use their canonical provider routes"
    );
    anyhow::ensure!(
        std::env::var("OCEAN_PROVIDER_FALLBACK").as_deref() == Ok("disabled"),
        "set OCEAN_PROVIDER_FALLBACK=disabled; a blank value enables default fallback"
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
    let failure_count = AtomicUsize::new(0);
    let failures = &failure_count;
    futures::stream::iter(probes)
        .for_each_concurrent(4, |(model, effort)| async move {
            let stage = std::cell::Cell::new("temporary_store");
            let result = async {
                let mut env = ocean_providers::ProviderEnv::from_process();
                env.vars.insert("OCEAN_MODEL".into(), model.clone());
                let selected = ocean_providers::resolve_model_selection(&env)?;
                anyhow::ensure!(ocean_providers::fallback_candidates(
                    &env,
                    &selected.provider,
                    &ocean_providers::ProviderQuarantine::default(),
                    std::time::Instant::now(),
                ).is_empty(), "live acceptance requires zero fallback candidates");
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
                if !response.ok || response.stdout.trim() != "OCEAN_OK" {
                    return Ok(false);
                }
                stage.set("model_attribution");
                let actual = runtime
                    .session_model_config_optional(response.session_id.ok_or_else(|| {
                        anyhow::anyhow!("successful turn omitted session identity")
                    })?)?
                    .ok_or_else(|| anyhow::anyhow!("successful turn omitted persisted session"))?;
                let expected = ocean_providers::resolve_model_selection(&ocean_providers::ProviderEnv {
                    vars: std::collections::BTreeMap::from([("OCEAN_MODEL".into(), model.clone())]),
                    ..Default::default()
                })?;
                let wire_model = expected.model.replace("claude-code-", "claude-");
                Ok::<_, anyhow::Error>(actual.model == wire_model)
            };
            let outcome = match tokio::time::timeout(Duration::from_secs(45), result).await {
                Ok(Ok(true)) => "passed",
                Ok(Ok(false)) => "turn_failed_or_unexpected_reply",
                Ok(Err(_)) => "configuration_error",
                Err(_) => "timeout",
            };
            if outcome != "passed" {
                failures.fetch_add(1, Ordering::Relaxed);
            }
            println!(
                "{}",
                serde_json::json!({"model":model,"effort":effort,"outcome":outcome,"stage":stage.get()})
            );
        })
        .await;
    let failed = failure_count.load(Ordering::Relaxed);
    anyhow::ensure!(failed == 0, "{failed} Max-effort acceptance probes failed");
    Ok(())
}
