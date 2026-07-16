//! Real LLM worker agents on **cheap** models.
//!
//! Each longhouse worker is one focused, single-turn LLM call. We reuse the
//! existing unified provider machinery — `ocean_protocol::stream_simple` plus
//! `ocean_providers` for credential + model-alias resolution — rather than
//! writing any new HTTP client. Keys come from `~/.config/ocean-rs/auth.json`
//! (the same path the daemon already resolves), so deepseek + kimi work with
//! zero extra config.
//!
//! Prompts are deliberately short (these are cheap models doing one turn each),
//! and every call is best-effort: if a model errors or times out, that agent
//! simply doesn't contribute. A council never crashes because one worker fell
//! over.

use std::time::Duration;

use futures::StreamExt;
use ocean_protocol::{
    stream_simple, AssistantMessageEvent, Content, Context, Message, Model, StreamOptions,
};
use ocean_providers::{ProviderConfig, ProviderEnv};

/// How long any single cheap-model turn is allowed to run before we give up on
/// that agent (it just doesn't contribute its mark).
const AGENT_TURN_TIMEOUT: Duration = Duration::from_secs(45);

/// Max output tokens per worker turn. Kept small — proposals and votes are short.
const AGENT_MAX_TOKENS: u32 = 512;

/// A resolved model handle plus its API key, ready to drive `stream_simple`.
#[derive(Clone)]
pub struct ModelHandle {
    /// The alias the operator/council asked for (e.g. "deepseek-v4-flash").
    pub alias: String,
    model: Model,
    api_key: Option<String>,
    base_url: String,
}

impl ModelHandle {
    /// The model id as the provider sees it.
    pub fn model_id(&self) -> &str {
        &self.model.id
    }

    /// Durable correlation identity used by the Longhouse evidence engine.
    /// Replicas routed to the same provider/model share one evidence budget,
    /// even when the roster gave them different display labels.
    pub fn correlation_group(&self) -> String {
        format!("{}:{}", self.model.provider, self.model.id)
    }

    /// Whether a usable credential was found for this model.
    pub fn has_credential(&self) -> bool {
        self.api_key.is_some()
    }

    /// Resolve a model alias (e.g. `deepseek-v4-flash`, `kimi-k2.6`) into a
    /// ready-to-call handle, pulling the credential from the standard auth.json.
    ///
    /// This reuses `ocean_providers::resolve_provider_config` exactly as the
    /// daemon does, just with `OCEAN_MODEL` pinned to the requested alias, so a
    /// council can mix models per-agent without touching the daemon's globally
    /// selected model.
    pub fn resolve(alias: &str) -> anyhow::Result<Self> {
        let mut env = ProviderEnv::from_process();
        // Pin the model selection to the requested alias. We intentionally drop
        // any inherited OCEAN_MODEL/OCEAN_PROVIDER so the alias decides routing.
        env.vars
            .insert("OCEAN_MODEL".to_string(), alias.to_string());
        env.vars.remove("OCEAN_PROVIDER");

        let config = ocean_providers::resolve_provider_config(&env)
            .map_err(|e| anyhow::anyhow!("resolve model '{alias}': {e}"))?;
        Ok(Self::from_provider_config(alias, config))
    }

    /// Build a handle from an already-resolved provider config. The cheap models
    /// we use (deepseek, kimi) are all OpenAI-compatible passthroughs, so a
    /// single `openai_compat` construction covers them; this mirrors
    /// `ocean-agent`'s `model_from_provider_config` for those providers.
    pub fn from_provider_config(alias: &str, config: ProviderConfig) -> Self {
        let selection = &config.selection;
        let model = Model::openai_compat(
            selection.provider.as_str(),
            selection.model.clone(),
            selection.base_url.clone(),
            selection.context_window,
            selection.max_output_tokens,
        );
        let api_key = config
            .credential
            .as_ref()
            .map(|c| c.secret.expose().to_string());
        Self {
            alias: alias.to_string(),
            base_url: selection.base_url.clone(),
            model,
            api_key,
        }
    }

    /// Run one focused turn: send `system` + `user` to the model and return the
    /// assistant's text. Best-effort — returns `None` on any error, timeout, or
    /// missing credential, so a downed worker never breaks the council.
    pub async fn ask(&self, system: &str, user: &str) -> Option<String> {
        if self.api_key.is_none() {
            tracing::warn!(alias = %self.alias, "no credential for model; skipping agent");
            return None;
        }

        let ctx = Context {
            system_prompt: Some(system.to_string()),
            messages: vec![Message::user_text(user)],
            tools: Vec::new(),
        };
        let options = StreamOptions {
            // Intentionally leave `temperature` unset: some cheap models
            // (e.g. kimi-k2.6) reject any value other than their default and
            // return a 400. Letting each provider use its own default keeps a
            // mixed-model council from dropping a worker over a sampling param.
            max_tokens: Some(AGENT_MAX_TOKENS),
            api_key: self.api_key.clone(),
            base_url: Some(self.base_url.clone()),
            ..Default::default()
        };

        let fut = run_once(&self.model, ctx, options);
        match tokio::time::timeout(AGENT_TURN_TIMEOUT, fut).await {
            Ok(Ok(text)) => {
                let text = text.trim().to_string();
                if text.is_empty() {
                    None
                } else {
                    Some(text)
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(alias = %self.alias, error = %e, "agent turn failed");
                None
            }
            Err(_) => {
                tracing::warn!(alias = %self.alias, "agent turn timed out");
                None
            }
        }
    }
}

/// Drive `stream_simple` to completion and collect the assistant text.
async fn run_once(model: &Model, ctx: Context, options: StreamOptions) -> anyhow::Result<String> {
    let mut stream = stream_simple(model, &ctx, &options)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let mut buf = String::new();
    while let Some(ev) = stream.next().await {
        match ev.map_err(|e| anyhow::anyhow!("{e}"))? {
            AssistantMessageEvent::TextDelta { delta, .. } => buf.push_str(&delta),
            AssistantMessageEvent::Done { message, .. } => {
                // Prefer the terminal message's text if deltas were sparse.
                if buf.trim().is_empty() {
                    for c in &message.content {
                        if let Content::Text { text } = c {
                            buf.push_str(text);
                        }
                    }
                }
                break;
            }
            AssistantMessageEvent::Error { error, .. } => {
                let msg = error
                    .error_message
                    .unwrap_or_else(|| "provider error".into());
                return Err(anyhow::anyhow!(msg));
            }
            _ => {}
        }
    }
    Ok(buf)
}
