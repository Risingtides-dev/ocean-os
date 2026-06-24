//! Ocean-owned provider registry, model mapping, auth resolution, and readiness.
//!
//! This crate is intentionally independent from Pi runtime/auth types. It owns the
//! provider/auth decision for Ocean callers; temporary adapters can translate the
//! resolved config into legacy runtime structs at the edge.

use std::{collections::BTreeMap, fmt, path::PathBuf};

use serde::{Deserialize, Serialize};

const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/v1";
const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com/v1";
// OpenAI-compatible chat-completions endpoints. Both expose `/chat/completions`
// under these bases and stream reasoning separately (MiniMax via `<think>`
// tags / `reasoning_split`, Kimi via a thinking channel), so they ride the
// same streaming path as DeepSeek's reasoner models.
const MINIMAX_BASE_URL: &str = "https://api.minimaxi.com/v1";
const MOONSHOT_BASE_URL: &str = "https://api.moonshot.ai/v1";
// Google Generative AI (Gemini). Routed through ocean-protocol's
// `google-generative-ai` provider, which targets the v1beta surface under
// this base — not an OpenAI-compatible endpoint.
const GOOGLE_BASE_URL: &str = "https://generativelanguage.googleapis.com";

/// Stable provider identifier used by Ocean runtime components.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderId {
    DeepSeek,
    OpenAi,
    /// OpenAI Codex over a ChatGPT subscription OAuth token (Responses API).
    OpenAiCodex,
    Anthropic,
    /// MiniMax (OpenAI-compatible chat-completions; M2 family).
    MiniMax,
    /// Moonshot AI / Kimi (OpenAI-compatible chat-completions; K2 family).
    Kimi,
    /// Google Generative AI (Gemini family; v1beta generativelanguage API).
    Google,
    OpenAiCompatible,
    Fake,
}

impl ProviderId {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DeepSeek => "deepseek",
            Self::OpenAi => "openai",
            Self::OpenAiCodex => "openai-codex",
            Self::Anthropic => "anthropic",
            Self::MiniMax => "minimax",
            Self::Kimi => "kimi",
            Self::Google => "google",
            Self::OpenAiCompatible => "openai-compatible",
            Self::Fake => "fake",
        }
    }

    pub fn credential_env_names(&self) -> &'static [&'static str] {
        match self {
            Self::DeepSeek => &["OCEAN_DEEPSEEK_API_KEY", "DEEPSEEK_API_KEY"],
            Self::OpenAi | Self::OpenAiCompatible => &["OCEAN_OPENAI_API_KEY", "OPENAI_API_KEY"],
            // Codex uses the OAuth token from auth.json, not an env API key.
            Self::OpenAiCodex => &[],
            Self::Anthropic => &["OCEAN_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"],
            Self::MiniMax => &["OCEAN_MINIMAX_API_KEY", "MINIMAX_API_KEY"],
            Self::Kimi => &["OCEAN_MOONSHOT_API_KEY", "MOONSHOT_API_KEY", "KIMI_API_KEY"],
            // `GEMINI_API_KEY` is Google's own canonical env var (their SDKs
            // default to it). The GoogleProvider in ocean-protocol reads both
            // GOOGLE_API_KEY and GEMINI_API_KEY, so the registry's readiness
            // check must accept the same set — otherwise an operator who set
            // only GEMINI_API_KEY fails preflight ("missing credential") and
            // Gemini is silently unroutable even though the provider could auth.
            Self::Google => &["OCEAN_GOOGLE_API_KEY", "GOOGLE_API_KEY", "GEMINI_API_KEY"],
            Self::Fake => &[],
        }
    }

    pub fn requires_credential(&self) -> bool {
        !matches!(self, Self::Fake)
    }
}

/// Source label for a resolved credential. Never contains the credential value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialSource {
    Env { name: String },
    OceanAuthFile { path: String },
    NotRequired,
}

/// Secret-bearing credential wrapper. Debug/Display intentionally redact.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        (!value.trim().is_empty()).then_some(Self(value))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Resolved provider credential plus non-secret source label.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedCredential {
    pub secret: SecretString,
    pub source: CredentialSource,
}

impl fmt::Debug for ResolvedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedCredential")
            .field("secret", &self.secret)
            .field("source", &self.source)
            .finish()
    }
}

/// Resolved model/provider selection before credential lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSelection {
    pub provider: ProviderId,
    pub model: String,
    pub base_url: String,
    pub context_window: u32,
    pub max_output_tokens: u32,
}

/// Full runtime provider config for Ocean callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    pub selection: ModelSelection,
    pub credential: Option<ResolvedCredential>,
    /// ChatGPT account id, present only for the Codex OAuth provider. Sent as
    /// the `chatgpt-account-id` request header. Non-secret.
    pub account_id: Option<String>,
}

impl ProviderConfig {
    pub fn readiness(&self) -> ProviderReadiness {
        let credential_present = self.credential.is_some();
        let credential_source = self.credential.as_ref().map(|cred| cred.source.clone());
        let ok = credential_present || !self.selection.provider.requires_credential();
        ProviderReadiness {
            ok,
            provider: self.selection.provider.clone(),
            model: self.selection.model.clone(),
            base_url_host: base_url_host(&self.selection.base_url),
            credential_present,
            credential_source,
            error: (!ok).then_some(ProviderConfigError::MissingCredential {
                provider: self.selection.provider.as_str().to_string(),
            }),
        }
    }
}

/// Non-secret readiness status suitable for daemon APIs/logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderReadiness {
    pub ok: bool,
    pub provider: ProviderId,
    pub model: String,
    pub base_url_host: String,
    pub credential_present: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_source: Option<CredentialSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ProviderConfigError>,
}

/// Structured provider/auth configuration errors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProviderConfigError {
    UnknownModel {
        model: String,
    },
    /// No model was selected anywhere — no `OCEAN_MODEL`, no persisted choice.
    /// The daemon never picks a model for you; you set it.
    NoModelSelected,
    MissingBaseUrl {
        provider: String,
    },
    MissingCredential {
        provider: String,
    },
    InvalidAuthFile {
        path: String,
        message: String,
    },
}

impl fmt::Display for ProviderConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownModel { model } => write!(f, "unknown model '{model}'"),
            Self::NoModelSelected => write!(
                f,
                "no model selected — set OCEAN_MODEL or pick one via POST /v1/model \
                 (the daemon never defaults to a model for you)"
            ),
            Self::MissingBaseUrl { provider } => {
                write!(f, "missing base URL for provider {provider}")
            }
            Self::MissingCredential { provider } => {
                write!(f, "missing credential for provider {provider}")
            }
            Self::InvalidAuthFile { path, message } => {
                write!(f, "invalid Ocean auth file {path}: {message}")
            }
        }
    }
}

impl std::error::Error for ProviderConfigError {}

/// Environment snapshot used for deterministic, unit-testable resolution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderEnv {
    pub vars: BTreeMap<String, String>,
    pub auth_file: Option<PathBuf>,
}

impl ProviderEnv {
    pub fn from_process() -> Self {
        Self {
            vars: std::env::vars().collect(),
            auth_file: std::env::var_os("OCEAN_AUTH_FILE")
                .map(PathBuf::from)
                .or_else(default_ocean_auth_file),
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }
}

/// Resolve full provider config from the current process environment.
pub fn resolve_provider_config_from_env() -> Result<ProviderConfig, ProviderConfigError> {
    resolve_provider_config(&ProviderEnv::from_process())
}

/// Resolve full provider config from a provided environment snapshot.
pub fn resolve_provider_config(env: &ProviderEnv) -> Result<ProviderConfig, ProviderConfigError> {
    let selection = resolve_model_selection(env)?;
    let credential = resolve_credential(env, &selection.provider)?;
    let account_id = if matches!(selection.provider, ProviderId::OpenAiCodex) {
        resolve_codex_account_id(env)
    } else {
        None
    };
    Ok(ProviderConfig {
        selection,
        credential,
        account_id,
    })
}

// ---------------------------------------------------------------------------
// Provider fallback / failover (OCEAN-275)
// ---------------------------------------------------------------------------
//
// Readiness already tells us whether *the selected* provider can serve a turn,
// but a degraded/credential-missing primary used to just fail the turn — nothing
// routed to a ready alternate. This block resolves an ordered list of ready
// *alternate* providers so a caller (the agent layer) can fail over at SELECTION
// time, and on a connect-failure before any output streamed. It deliberately
// does NOT decide *when* to fail over (that's the agent layer, which owns the
// turn lifecycle and the mid-stream-safety boundary); it only answers "given the
// environment, what ready providers could serve this request, in what order?".

/// Env var holding the ordered fallback list (OCEAN-275), comma-separated model
/// aliases — e.g. `claude-sonnet-4-6,gpt-5.4,deepseek-v4-pro`. Each alias is
/// resolved through the same [`resolve_provider_config`] path as a primary
/// selection, so anything valid for `OCEAN_MODEL` is valid here. Unset ⇒
/// [`DEFAULT_FALLBACK_ORDER`]. Unparseable/unknown entries are skipped (with a
/// warning), never fatal — a typo degrades the list, it doesn't break turns.
/// Mirrors the env-config pattern of `OCEAN_RETRY_*` (OCEAN-259) and the provider
/// timeout knobs (OCEAN-221): config, never code; absent ⇒ a sensible default.
pub const ENV_PROVIDER_FALLBACK: &str = "OCEAN_PROVIDER_FALLBACK";

/// Default cross-provider fallback order when [`ENV_PROVIDER_FALLBACK`] is unset.
///
/// One representative model alias per real, credential-backed provider, ordered
/// most- to least-capable. Only the entries whose credential is actually present
/// in the environment survive [`fallback_candidates`]; the rest are silently
/// unavailable. `Fake` is intentionally excluded — failing a production turn over
/// to a canned echo would hide an outage rather than route around it (an operator
/// who wants that can still list `fake` explicitly in the env override).
pub const DEFAULT_FALLBACK_ORDER: &[&str] = &[
    "claude-sonnet-4-6", // anthropic
    "gpt-5.4",           // openai-codex
    "deepseek-v4-pro",   // deepseek
    "gemini-2.0-flash",  // google
    "kimi-k2.6",         // kimi
    "minimax-m2",        // minimax
];

/// Parse the configured fallback order into a list of model aliases.
///
/// Returns the [`ENV_PROVIDER_FALLBACK`] entries (trimmed, empties dropped) when
/// set and non-empty, otherwise [`DEFAULT_FALLBACK_ORDER`]. A set-but-blank value
/// (e.g. `OCEAN_PROVIDER_FALLBACK=""` or all-commas) falls back to the default
/// rather than yielding an empty list, so an exported-but-empty var can never
/// silently disable failover.
fn fallback_order(env: &ProviderEnv) -> Vec<String> {
    if let Some(raw) = env.get(ENV_PROVIDER_FALLBACK) {
        let parsed: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if !parsed.is_empty() {
            return parsed;
        }
    }
    DEFAULT_FALLBACK_ORDER
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Resolve the ordered list of **ready alternate** provider configs for failover.
///
/// Walks the configured [`fallback_order`], resolving each model alias against the
/// same `env` snapshot the primary used (so credentials line up), and keeps only
/// configs that are *ready* (`readiness().ok` — credential present, or the
/// provider needs none). The result is:
/// - deduped by [`ProviderId`] (first ready alias per provider wins), since two
///   aliases on the same degraded provider are not independent failover targets;
/// - excludes `exclude_provider` (the primary), so we never "fail over" to the
///   same provider that just failed.
///
/// An alias that doesn't resolve (unknown model, missing base url) is skipped — a
/// bad fallback entry must not break the turn (the agent layer logs the overall
/// failover decision; this crate stays dependency-light and silent). The returned
/// vec is in priority order; an empty vec means "no ready alternate exists".
pub fn fallback_candidates(
    env: &ProviderEnv,
    exclude_provider: &ProviderId,
) -> Vec<ProviderConfig> {
    let mut out: Vec<ProviderConfig> = Vec::new();
    for alias in fallback_order(env) {
        // Resolve this alias as if it were the selected model: same env, so the
        // OCEAN_MODEL override is the only thing that changes.
        let mut alias_env = env.clone();
        alias_env
            .vars
            .insert("OCEAN_MODEL".to_string(), alias.clone());
        // A fallback alias must route purely on the alias itself, not inherit an
        // OCEAN_PROVIDER pin meant for the primary (which would force every
        // candidate onto the same provider and defeat failover).
        alias_env.vars.remove("OCEAN_PROVIDER");
        let Ok(config) = resolve_provider_config(&alias_env) else {
            // Unknown model / missing base url for this entry — skip it.
            continue;
        };
        let provider = config.selection.provider.clone();
        if &provider == exclude_provider {
            continue;
        }
        if !config.readiness().ok {
            // Not ready (e.g. its own credential is missing) — not a usable
            // failover target right now. Skip silently; this is the common,
            // expected case for providers the operator hasn't configured.
            continue;
        }
        if out.iter().any(|c| c.selection.provider == provider) {
            continue;
        }
        out.push(config);
    }
    out
}

/// Resolve the single best ready alternate provider for failover, or `None` when
/// every configured fallback is unavailable.
///
/// Thin convenience over [`fallback_candidates`] returning just the first (
/// highest-priority) ready alternate. The agent layer uses this both at
/// selection time (primary not ready) and after a connect-failure (primary
/// returned an availability error before streaming any output).
pub fn resolve_fallback_config(
    env: &ProviderEnv,
    exclude_provider: &ProviderId,
) -> Option<ProviderConfig> {
    fallback_candidates(env, exclude_provider)
        .into_iter()
        .next()
}

/// Read the ChatGPT account id from the `openai-codex` auth.json block.
fn resolve_codex_account_id(env: &ProviderEnv) -> Option<String> {
    let path = env.auth_file.as_ref()?;
    let text = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    json.pointer("/openai-codex/accountId")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// A user-selectable model: the canonical id to pass to `OCEAN_MODEL` /
/// `POST /v1/model`, its provider, and a short human label for a picker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownModel {
    /// Canonical model id (also a valid alias for selection).
    pub id: String,
    /// Provider this model routes to (e.g. "deepseek", "openai-codex").
    pub provider: String,
    /// Short human-facing label for a dropdown.
    pub label: String,
}

/// The catalogue of models the daemon knows how to route, for clients that
/// render a model picker. Kept in sync with the alias arms in
/// `resolve_model_selection`. (Availability still depends on the relevant
/// provider credential being present; this is the menu, not a guarantee.)
pub fn known_models() -> Vec<KnownModel> {
    let m = |id: &str, provider: &str, label: &str| KnownModel {
        id: id.to_string(),
        provider: provider.to_string(),
        label: label.to_string(),
    };
    vec![
        m("deepseek-v4-pro", "deepseek", "DeepSeek V4 Pro"),
        m("deepseek-v4-flash", "deepseek", "DeepSeek V4 Flash"),
        m("deepseek-reasoner", "deepseek", "DeepSeek Reasoner"),
        m("deepseek-chat", "deepseek", "DeepSeek Chat"),
        m("gpt-5.5", "openai-codex", "GPT-5.5 (Codex)"),
        m("gpt-5.4", "openai-codex", "GPT-5.4 (Codex)"),
        m("gpt-5.4-mini", "openai-codex", "GPT-5.4 Mini (Codex)"),
        m("claude-opus-4-7", "anthropic", "Claude Opus 4.7"),
        m("claude-sonnet-4-6", "anthropic", "Claude Sonnet 4.6"),
        m("minimax-m2", "minimax", "MiniMax M2"),
        m("kimi-k2.6", "kimi", "Kimi K2.6"),
        m("gemini-2.0-flash", "google", "Gemini 2.0 Flash"),
    ]
}

/// Resolve model selection without reading credential values.
pub fn resolve_model_selection(env: &ProviderEnv) -> Result<ModelSelection, ProviderConfigError> {
    // No hardcoded model. The operator's choice flows in as OCEAN_MODEL (set
    // explicitly, or injected from the persisted last-used selection). A cold
    // machine with no choice anywhere may set OCEAN_DEFAULT_MODEL in its env —
    // still config, never code. If nothing is selected, we error rather than
    // silently pick one for you.
    let chosen = env
        .get("OCEAN_MODEL")
        .or_else(|| env.get("OCEAN_DEFAULT_MODEL"))
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(chosen) = chosen else {
        return Err(ProviderConfigError::NoModelSelected);
    };
    let model = normalize_model_id(chosen);
    let provider_override = env.get("OCEAN_PROVIDER").map(str::trim);

    if let Some(provider) = provider_override {
        return model_for_explicit_provider(provider, model.as_str(), env);
    }

    match model.as_str() {
        "deepseek" | "deepseek-chat" => Ok(model_selection(
            ProviderId::DeepSeek,
            "deepseek-chat",
            DEEPSEEK_BASE_URL,
            64_000,
            8_192,
        )),
        "deepseek-reasoner" | "deepseek-r1" => Ok(model_selection(
            ProviderId::DeepSeek,
            "deepseek-reasoner",
            DEEPSEEK_BASE_URL,
            64_000,
            8_192,
        )),
        "deepseek-v4-flash" => Ok(model_selection(
            ProviderId::DeepSeek,
            "deepseek-v4-flash",
            DEEPSEEK_BASE_URL,
            64_000,
            8_192,
        )),
        "deepseek-v4-pro" | "deepseek-v4" | "deepseek-pro" | "v4-pro" => Ok(model_selection(
            ProviderId::DeepSeek,
            "deepseek-v4-pro",
            DEEPSEEK_BASE_URL,
            64_000,
            8_192,
        )),
        "gpt-4o" => Ok(model_selection(
            ProviderId::OpenAi,
            "gpt-4o",
            OPENAI_BASE_URL,
            128_000,
            16_384,
        )),
        "gpt-4o-mini" => Ok(model_selection(
            ProviderId::OpenAi,
            "gpt-4o-mini",
            OPENAI_BASE_URL,
            128_000,
            16_384,
        )),
        "gpt-5.5" | "gpt-5-5" => Ok(model_selection(
            ProviderId::OpenAiCodex,
            "gpt-5.5",
            CODEX_BASE_URL,
            400_000,
            128_000,
        )),
        "gpt-5.4" | "gpt-5-4" => Ok(model_selection(
            ProviderId::OpenAiCodex,
            "gpt-5.4",
            CODEX_BASE_URL,
            400_000,
            128_000,
        )),
        "gpt-5.4-mini" | "gpt-5-4-mini" => Ok(model_selection(
            ProviderId::OpenAiCodex,
            "gpt-5.4-mini",
            CODEX_BASE_URL,
            400_000,
            128_000,
        )),
        "gpt-5.3-codex-spark" | "gpt-5-3-codex-spark" => Ok(model_selection(
            ProviderId::OpenAiCodex,
            "gpt-5.3-codex-spark",
            CODEX_BASE_URL,
            400_000,
            128_000,
        )),
        "claude-sonnet-4-6" | "claude-sonnet" | "sonnet" => Ok(model_selection(
            ProviderId::Anthropic,
            "claude-sonnet-4-6",
            ANTHROPIC_BASE_URL,
            200_000,
            16_384,
        )),
        "claude-opus-4-7" | "claude-opus" | "opus" => Ok(model_selection(
            ProviderId::Anthropic,
            "claude-opus-4-7",
            ANTHROPIC_BASE_URL,
            200_000,
            16_384,
        )),
        // MiniMax M2 family. `normalize_model_id` lowercases the lookup key, but
        // MiniMax's API expects the original `MiniMax-…` casing, so the value we
        // pass through preserves it.
        "minimax" | "minimax-m2" => Ok(model_selection(
            ProviderId::MiniMax,
            "MiniMax-M2",
            MINIMAX_BASE_URL,
            200_000,
            8_192,
        )),
        "minimax-m2.7" | "minimax-m2-7" => Ok(model_selection(
            ProviderId::MiniMax,
            "MiniMax-M2.7",
            MINIMAX_BASE_URL,
            200_000,
            8_192,
        )),
        // Moonshot AI / Kimi K2 family. Official OpenAI-compatible base; model
        // ids are lowercase so casing survives normalization as-is.
        "kimi" | "kimi-k2.6" | "kimi-k2-6" => Ok(model_selection(
            ProviderId::Kimi,
            "kimi-k2.6",
            MOONSHOT_BASE_URL,
            256_000,
            8_192,
        )),
        "kimi-k2" | "moonshot-v1" => Ok(model_selection(
            ProviderId::Kimi,
            "kimi-k2",
            MOONSHOT_BASE_URL,
            128_000,
            8_192,
        )),
        // Google Gemini family. Routed through ocean-protocol's
        // `google-generative-ai` provider (not OpenAI-compatible). Model ids
        // are lowercase, so casing survives normalization as-is.
        "gemini" | "gemini-2.0-flash" | "gemini-2-0-flash" => Ok(model_selection(
            ProviderId::Google,
            "gemini-2.0-flash",
            GOOGLE_BASE_URL,
            1_000_000,
            8_192,
        )),
        "fake" | "fake-ok" => Ok(model_selection(
            ProviderId::Fake,
            "fake-ok",
            "fake://local",
            1_000,
            1_000,
        )),
        // OCEAN-130: a keyless Fake variant that deterministically emits ONE
        // tool call, so the permission gate's release path (block→decide→
        // proceed) can be live-tested over HTTP with no external LLM key. Like
        // `fake-ok` it needs no credential; the model id is preserved as
        // `fake-tool` so the agent layer can route it through the real loop with
        // an injected `FakeToolProvider`.
        "fake-tool" => Ok(model_selection(
            ProviderId::Fake,
            "fake-tool",
            "fake://local",
            1_000,
            1_000,
        )),
        // OCEAN-150: a keyless Fake variant that deterministically emits ONE
        // `surface_patch` tool call, so the daemon's SurfacePatch SSE bridge
        // (runtime side effect → `AgentTurnEvent::SurfacePatch` on
        // `/v1/agent/events`) can be live-tested over HTTP with no external key.
        "fake-surface" => Ok(model_selection(
            ProviderId::Fake,
            "fake-surface",
            "fake://local",
            1_000,
            1_000,
        )),
        other if env.get("OCEAN_OPENAI_BASE_URL").is_some() => Ok(model_selection(
            ProviderId::OpenAiCompatible,
            other,
            env.get("OCEAN_OPENAI_BASE_URL").unwrap_or(OPENAI_BASE_URL),
            128_000,
            16_384,
        )),
        other => Err(ProviderConfigError::UnknownModel {
            model: other.to_string(),
        }),
    }
}

fn normalize_model_id(model: &str) -> String {
    model
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if ch == ' ' || ch == '_' { '-' } else { ch })
        .collect()
}

/// Restore MiniMax's case-sensitive API model id from a lowercased alias:
/// `minimax-m2` -> `MiniMax-M2`, `minimax-m2.7` -> `MiniMax-M2.7`. Anything that
/// doesn't match the `minimax-m…` shape passes through unchanged.
fn minimax_api_casing(model: &str) -> String {
    match model.to_ascii_lowercase().strip_prefix("minimax-m") {
        Some(rest) => format!("MiniMax-M{}", rest.to_ascii_uppercase()),
        None => model.to_string(),
    }
}

fn model_for_explicit_provider(
    provider: &str,
    model: &str,
    env: &ProviderEnv,
) -> Result<ModelSelection, ProviderConfigError> {
    match provider {
        "deepseek" => Ok(model_selection(
            ProviderId::DeepSeek,
            model,
            DEEPSEEK_BASE_URL,
            64_000,
            8_192,
        )),
        "openai" => Ok(model_selection(
            ProviderId::OpenAi,
            model,
            OPENAI_BASE_URL,
            128_000,
            16_384,
        )),
        "openai-codex" | "codex" => Ok(model_selection(
            ProviderId::OpenAiCodex,
            model,
            CODEX_BASE_URL,
            400_000,
            128_000,
        )),
        "anthropic" => Ok(model_selection(
            ProviderId::Anthropic,
            model,
            ANTHROPIC_BASE_URL,
            200_000,
            16_384,
        )),
        // MiniMax's API is case-sensitive on model ids (`MiniMax-M2`), but `model`
        // arrives lowercased (normalize_model_id). Restore the API casing for
        // known forms so the explicit-provider path matches the bare-alias path —
        // without this, `OCEAN_PROVIDER=minimax OCEAN_MODEL=minimax-m2` sends
        // `minimax-m2` and MiniMax rejects it, while the bare alias works.
        "minimax" => Ok(model_selection(
            ProviderId::MiniMax,
            minimax_api_casing(model),
            MINIMAX_BASE_URL,
            200_000,
            8_192,
        )),
        "kimi" | "moonshot" => Ok(model_selection(
            ProviderId::Kimi,
            model,
            MOONSHOT_BASE_URL,
            256_000,
            8_192,
        )),
        "google" | "gemini" => Ok(model_selection(
            ProviderId::Google,
            model,
            GOOGLE_BASE_URL,
            1_000_000,
            8_192,
        )),
        "openai-compatible" => {
            let base = env.get("OCEAN_OPENAI_BASE_URL").ok_or_else(|| {
                ProviderConfigError::MissingBaseUrl {
                    provider: provider.into(),
                }
            })?;
            Ok(model_selection(
                ProviderId::OpenAiCompatible,
                model,
                base,
                128_000,
                16_384,
            ))
        }
        "fake" => Ok(model_selection(
            ProviderId::Fake,
            model,
            "fake://local",
            1_000,
            1_000,
        )),
        other => Err(ProviderConfigError::UnknownModel {
            model: format!("{other}:{model}"),
        }),
    }
}

fn model_selection(
    provider: ProviderId,
    model: impl Into<String>,
    base_url: impl Into<String>,
    context_window: u32,
    max_output_tokens: u32,
) -> ModelSelection {
    ModelSelection {
        provider,
        model: model.into(),
        base_url: base_url.into(),
        context_window,
        max_output_tokens,
    }
}

fn resolve_credential(
    env: &ProviderEnv,
    provider: &ProviderId,
) -> Result<Option<ResolvedCredential>, ProviderConfigError> {
    if !provider.requires_credential() {
        return Ok(None);
    }

    for name in provider.credential_env_names() {
        if let Some(secret) = env.get(name).and_then(SecretString::new) {
            return Ok(Some(ResolvedCredential {
                secret,
                source: CredentialSource::Env {
                    name: (*name).into(),
                },
            }));
        }
    }

    let Some(path) = &env.auth_file else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }

    let text =
        std::fs::read_to_string(path).map_err(|err| ProviderConfigError::InvalidAuthFile {
            path: path.display().to_string(),
            message: err.to_string(),
        })?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|err| ProviderConfigError::InvalidAuthFile {
            path: path.display().to_string(),
            message: err.to_string(),
        })?;

    let source = CredentialSource::OceanAuthFile {
        path: path.display().to_string(),
    };

    // The Codex provider authenticates with the OAuth access token from the
    // "openai-codex" block; everyone else uses a plain api_key.
    let secret = if matches!(provider, ProviderId::OpenAiCodex) {
        oauth_access_token(&json, "openai-codex").and_then(SecretString::new)
    } else {
        auth_file_key(&json, provider.as_str()).and_then(SecretString::new)
    };
    Ok(secret.map(|secret| ResolvedCredential { secret, source }))
}

fn auth_file_key<'a>(json: &'a serde_json::Value, provider: &str) -> Option<&'a str> {
    json.pointer(&format!("/providers/{provider}/api_key"))
        .or_else(|| json.pointer(&format!("/{provider}/api_key")))
        .or_else(|| json.pointer(&format!("/{provider}/key")))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Pull an OAuth access token from an auth.json block of `type: "oauth"`.
/// Used so the OpenAI provider can authenticate with the Codex OAuth login
/// (bearer token) when no plain api_key is configured.
fn oauth_access_token<'a>(json: &'a serde_json::Value, block: &str) -> Option<&'a str> {
    let entry = json.pointer(&format!("/{block}"))?;
    if entry.pointer("/type").and_then(serde_json::Value::as_str) != Some("oauth") {
        return None;
    }
    entry
        .pointer("/access")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn default_ocean_auth_file() -> Option<PathBuf> {
    if let Some(config) = std::env::var_os("OCEAN_CONFIG_DIR") {
        return Some(PathBuf::from(config).join("auth.json"));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("ocean-rs").join("auth.json"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/ocean-rs/auth.json"))
}

fn base_url_host(base_url: &str) -> String {
    base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url)
        .split('/')
        .next()
        .unwrap_or(base_url)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn minimax_api_casing_restores_case_sensitive_id() {
        // The explicit-provider path must send the API-cased id (the bare-alias
        // path already does), or MiniMax rejects the request.
        assert_eq!(minimax_api_casing("minimax-m2"), "MiniMax-M2");
        assert_eq!(minimax_api_casing("MiniMax-M2"), "MiniMax-M2");
        assert_eq!(minimax_api_casing("minimax-m2.7"), "MiniMax-M2.7");
        // Non-minimax shapes pass through unchanged.
        assert_eq!(minimax_api_casing("something-else"), "something-else");
    }

    fn env(vars: &[(&str, &str)]) -> ProviderEnv {
        ProviderEnv {
            vars: vars
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
            auth_file: None,
        }
    }

    #[test]
    fn maps_deepseek_v4_flash_to_deepseek_provider() {
        let selection =
            resolve_model_selection(&env(&[("OCEAN_MODEL", "deepseek-v4-flash")])).unwrap();
        assert_eq!(selection.provider, ProviderId::DeepSeek);
        assert_eq!(selection.model, "deepseek-v4-flash");
        assert_eq!(selection.base_url, DEEPSEEK_BASE_URL);
    }

    #[test]
    fn maps_deepseek_v4_pro_to_official_deepseek_model() {
        let selection =
            resolve_model_selection(&env(&[("OCEAN_MODEL", "deepseek-v4-pro")])).unwrap();
        assert_eq!(selection.provider, ProviderId::DeepSeek);
        assert_eq!(selection.model, "deepseek-v4-pro");
        assert_eq!(selection.base_url, DEEPSEEK_BASE_URL);
    }

    #[test]
    fn normalizes_spaced_deepseek_v4_pro_alias_to_pro_not_flash() {
        let selection =
            resolve_model_selection(&env(&[("OCEAN_MODEL", "DeepSeek V4 Pro")])).unwrap();
        assert_eq!(selection.provider, ProviderId::DeepSeek);
        assert_eq!(selection.model, "deepseek-v4-pro");
    }

    #[test]
    fn maps_gemini_2_0_flash_to_google_provider() {
        let selection =
            resolve_model_selection(&env(&[("OCEAN_MODEL", "gemini-2.0-flash")])).unwrap();
        assert_eq!(selection.provider, ProviderId::Google);
        assert_eq!(selection.model, "gemini-2.0-flash");
        assert_eq!(selection.base_url, GOOGLE_BASE_URL);
        assert_eq!(selection.context_window, 1_000_000);
        assert_eq!(selection.max_output_tokens, 8_192);
    }

    #[test]
    fn gemini_alias_routes_to_google_provider() {
        let selection = resolve_model_selection(&env(&[("OCEAN_MODEL", "gemini")])).unwrap();
        assert_eq!(selection.provider, ProviderId::Google);
        assert_eq!(selection.model, "gemini-2.0-flash");
    }

    #[test]
    fn explicit_google_provider_routes_to_google() {
        let selection = resolve_model_selection(&env(&[
            ("OCEAN_PROVIDER", "google"),
            ("OCEAN_MODEL", "gemini-2.0-flash"),
        ]))
        .unwrap();
        assert_eq!(selection.provider, ProviderId::Google);
        assert_eq!(selection.base_url, GOOGLE_BASE_URL);
    }

    #[test]
    fn gemini_resolves_full_config_with_google_credential() {
        // A gemini model id (provider="google") must resolve to a ready
        // ProviderConfig: the Gemini-targeting api host plus a credential drawn
        // from the Google key env. This is the end-to-end registry guarantee
        // OCEAN-169 was about — without it, a Gemini turn fails at runtime.
        let config = resolve_provider_config(&env(&[
            ("OCEAN_MODEL", "gemini-2.0-flash"),
            ("GOOGLE_API_KEY", "g-secret"),
        ]))
        .unwrap();
        assert_eq!(config.selection.provider, ProviderId::Google);
        assert_eq!(config.selection.model, "gemini-2.0-flash");
        assert_eq!(config.selection.base_url, GOOGLE_BASE_URL);
        let credential = config
            .credential
            .as_ref()
            .expect("google key should resolve");
        assert_eq!(credential.secret.expose(), "g-secret");
        assert_eq!(
            credential.source,
            CredentialSource::Env {
                name: "GOOGLE_API_KEY".into()
            }
        );
        let readiness = config.readiness();
        assert!(readiness.ok);
        assert!(readiness.credential_present);
        assert_eq!(readiness.base_url_host, "generativelanguage.googleapis.com");
    }

    #[test]
    fn gemini_is_ready_with_canonical_gemini_api_key() {
        // The GoogleProvider (ocean-protocol) reads GEMINI_API_KEY, so the
        // registry readiness check must accept it too — otherwise an operator
        // with only GEMINI_API_KEY set fails preflight and Gemini is silently
        // unroutable despite the provider being able to authenticate.
        let config = resolve_provider_config(&env(&[
            ("OCEAN_MODEL", "gemini-2.0-flash"),
            ("GEMINI_API_KEY", "gemini-secret"),
        ]))
        .unwrap();
        let credential = config
            .credential
            .as_ref()
            .expect("GEMINI_API_KEY should resolve");
        assert_eq!(credential.secret.expose(), "gemini-secret");
        assert_eq!(
            credential.source,
            CredentialSource::Env {
                name: "GEMINI_API_KEY".into()
            }
        );
        assert!(config.readiness().ok);
    }

    #[test]
    fn unknown_model_does_not_silently_become_openai() {
        let err =
            resolve_model_selection(&env(&[("OCEAN_MODEL", "not-a-known-model")])).unwrap_err();
        assert_eq!(
            err,
            ProviderConfigError::UnknownModel {
                model: "not-a-known-model".into()
            }
        );
    }

    #[test]
    fn explicit_openai_compatible_requires_base_url() {
        let err = resolve_model_selection(&env(&[
            ("OCEAN_PROVIDER", "openai-compatible"),
            ("OCEAN_MODEL", "custom-model"),
        ]))
        .unwrap_err();
        assert_eq!(
            err,
            ProviderConfigError::MissingBaseUrl {
                provider: "openai-compatible".into()
            }
        );
    }

    #[test]
    fn env_credential_wins_and_is_redacted() {
        let config = resolve_provider_config(&env(&[
            ("OCEAN_MODEL", "deepseek-chat"),
            ("OCEAN_DEEPSEEK_API_KEY", "secret-value"),
        ]))
        .unwrap();
        let credential = config.credential.unwrap();
        assert_eq!(credential.secret.expose(), "secret-value");
        assert_eq!(format!("{:?}", credential.secret), "<redacted>");
        assert_eq!(format!("{}", credential.secret), "<redacted>");
        assert_eq!(
            credential.source,
            CredentialSource::Env {
                name: "OCEAN_DEEPSEEK_API_KEY".into()
            }
        );
    }

    #[test]
    fn ocean_auth_file_can_supply_key() {
        let dir = std::env::temp_dir().join(format!("ocean-providers-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        fs::write(
            &path,
            r#"{"providers":{"deepseek":{"api_key":"file-secret"}}}"#,
        )
        .unwrap();

        let config = resolve_provider_config(&ProviderEnv {
            vars: BTreeMap::from([("OCEAN_MODEL".into(), "deepseek-chat".into())]),
            auth_file: Some(path.clone()),
        })
        .unwrap();
        let credential = config.credential.unwrap();
        assert_eq!(credential.secret.expose(), "file-secret");
        assert_eq!(
            credential.source,
            CredentialSource::OceanAuthFile {
                path: path.display().to_string()
            }
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn gpt5_routes_to_codex_with_oauth_token_and_account_id() {
        let dir = std::env::temp_dir().join(format!("ocean-oauth-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        fs::write(
            &path,
            r#"{"openai-codex":{"type":"oauth","access":"oauth-bearer-token","refresh":"rt_x","expires":9999999999999,"accountId":"acct-123"}}"#,
        )
        .unwrap();

        let config = resolve_provider_config(&ProviderEnv {
            vars: BTreeMap::from([("OCEAN_MODEL".into(), "gpt-5.5".into())]),
            auth_file: Some(path.clone()),
        })
        .unwrap();
        let credential = config.credential.expect("oauth token should resolve");
        assert_eq!(credential.secret.expose(), "oauth-bearer-token");
        assert_eq!(config.selection.provider, ProviderId::OpenAiCodex);
        assert_eq!(config.selection.model, "gpt-5.5");
        assert_eq!(config.selection.base_url, CODEX_BASE_URL);
        assert_eq!(config.account_id.as_deref(), Some("acct-123"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn readiness_reports_missing_credential_without_secret() {
        let config = resolve_provider_config(&env(&[("OCEAN_MODEL", "deepseek-chat")])).unwrap();
        let readiness = config.readiness();
        assert!(!readiness.ok);
        assert!(!readiness.credential_present);
        assert_eq!(readiness.provider, ProviderId::DeepSeek);
        assert_eq!(
            readiness.error,
            Some(ProviderConfigError::MissingCredential {
                provider: "deepseek".into()
            })
        );
    }

    #[test]
    fn fake_provider_is_ready_without_credential() {
        let config = resolve_provider_config(&env(&[("OCEAN_MODEL", "fake-ok")])).unwrap();
        let readiness = config.readiness();
        assert!(readiness.ok);
        assert!(!readiness.credential_present);
    }

    // ---- Fallback / failover (OCEAN-275) ----------------------------------

    #[test]
    fn fallback_picks_a_ready_alternate_when_primary_provider_is_degraded() {
        // Primary = deepseek (its key is intentionally absent → degraded), but an
        // Anthropic key IS present. The default order leads with Anthropic, so the
        // first ready alternate must be Anthropic.
        let e = env(&[
            ("OCEAN_MODEL", "deepseek-v4-pro"),
            ("ANTHROPIC_API_KEY", "sk-ant"),
        ]);
        let alt = resolve_fallback_config(&e, &ProviderId::DeepSeek)
            .expect("a ready anthropic alternate should be found");
        assert_eq!(alt.selection.provider, ProviderId::Anthropic);
        assert!(alt.readiness().ok);
        assert!(alt.credential.is_some());
    }

    #[test]
    fn fallback_is_empty_when_every_alternate_is_degraded() {
        // Only the deepseek key is set, and deepseek is the excluded primary, so
        // no *alternate* provider has a credential → no failover target. This is
        // the "all providers degraded → clear error" precondition the agent layer
        // turns into an explicit error rather than a silent hang.
        let e = env(&[
            ("OCEAN_MODEL", "deepseek-v4-pro"),
            ("OCEAN_DEEPSEEK_API_KEY", "ds-secret"),
        ]);
        assert!(resolve_fallback_config(&e, &ProviderId::DeepSeek).is_none());
        assert!(fallback_candidates(&e, &ProviderId::DeepSeek).is_empty());
    }

    #[test]
    fn fallback_never_routes_back_to_the_excluded_primary() {
        // Anthropic key present; if the primary is ALSO anthropic, the anthropic
        // entry must be excluded — failing over to the provider that just failed
        // is pointless. With no other key set, there's no alternate at all.
        let e = env(&[
            ("OCEAN_MODEL", "claude-opus-4-7"),
            ("ANTHROPIC_API_KEY", "sk-ant"),
        ]);
        let alt = resolve_fallback_config(&e, &ProviderId::Anthropic);
        assert!(
            alt.is_none(),
            "anthropic is the primary; it must not be its own fallback"
        );
    }

    #[test]
    fn fallback_dedupes_by_provider_and_honors_priority_order() {
        // Both deepseek and anthropic keys present. Primary = google (degraded).
        // Default order is anthropic, then codex, then deepseek… → the first ready
        // alternate is anthropic, and the candidate list holds at most one entry
        // per provider.
        let e = env(&[
            ("OCEAN_MODEL", "gemini-2.0-flash"),
            ("ANTHROPIC_API_KEY", "sk-ant"),
            ("OCEAN_DEEPSEEK_API_KEY", "ds-secret"),
        ]);
        let candidates = fallback_candidates(&e, &ProviderId::Google);
        assert_eq!(
            candidates.first().map(|c| c.selection.provider.clone()),
            Some(ProviderId::Anthropic),
            "highest-priority ready alternate should be first"
        );
        // deepseek is also ready and must appear, exactly once.
        let providers: Vec<_> = candidates.iter().map(|c| &c.selection.provider).collect();
        assert!(providers.contains(&&ProviderId::DeepSeek));
        let deepseek_count = providers
            .iter()
            .filter(|p| ***p == ProviderId::DeepSeek)
            .count();
        assert_eq!(deepseek_count, 1, "no duplicate provider entries");
    }

    #[test]
    fn env_override_reorders_and_restricts_the_fallback_list() {
        // Operator pins the order to deepseek-first. Both deepseek and anthropic
        // are ready; primary = google. The override must put deepseek first even
        // though the default order leads with anthropic.
        let e = env(&[
            ("OCEAN_MODEL", "gemini-2.0-flash"),
            (
                "OCEAN_PROVIDER_FALLBACK",
                "deepseek-v4-pro, claude-opus-4-7",
            ),
            ("ANTHROPIC_API_KEY", "sk-ant"),
            ("OCEAN_DEEPSEEK_API_KEY", "ds-secret"),
        ]);
        let alt = resolve_fallback_config(&e, &ProviderId::Google).unwrap();
        assert_eq!(alt.selection.provider, ProviderId::DeepSeek);
        assert_eq!(alt.selection.model, "deepseek-v4-pro");
    }

    #[test]
    fn blank_env_override_falls_back_to_the_default_order() {
        // An exported-but-empty override must not silently disable failover; it
        // falls back to the default order (anthropic-first here).
        let e = env(&[
            ("OCEAN_MODEL", "deepseek-v4-pro"),
            ("OCEAN_PROVIDER_FALLBACK", "  , ,"),
            ("ANTHROPIC_API_KEY", "sk-ant"),
        ]);
        let alt = resolve_fallback_config(&e, &ProviderId::DeepSeek).unwrap();
        assert_eq!(alt.selection.provider, ProviderId::Anthropic);
    }

    #[test]
    fn unknown_fallback_entries_are_skipped_not_fatal() {
        // A typo'd alias in the override is skipped; the next valid+ready entry
        // still wins.
        let e = env(&[
            ("OCEAN_MODEL", "gemini-2.0-flash"),
            ("OCEAN_PROVIDER_FALLBACK", "not-a-model, claude-opus-4-7"),
            ("ANTHROPIC_API_KEY", "sk-ant"),
        ]);
        let alt = resolve_fallback_config(&e, &ProviderId::Google).unwrap();
        assert_eq!(alt.selection.provider, ProviderId::Anthropic);
    }
}
