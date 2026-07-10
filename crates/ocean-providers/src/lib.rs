//! Ocean-owned provider registry, model mapping, auth resolution, and readiness.
//!
//! This crate is intentionally independent from Pi runtime/auth types. It owns the
//! provider/auth decision for Ocean callers; temporary adapters can translate the
//! resolved config into legacy runtime structs at the edge.

use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/v1";
const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
// OpenAI-compatible chat-completions endpoints. Both expose `/chat/completions`
// under these bases and stream reasoning separately (MiniMax via `<think>`
// tags / `reasoning_split`, Kimi via a thinking channel), so they ride the
// same streaming path as DeepSeek's reasoner models.
// MiniMax INTERNATIONAL platform. John's key is an intl key: verified live
// 2026-07-08 — 200 OK on api.minimax.io, 401 "invalid api key (2049)" on the
// mainland api.minimaxi.com host. Override with `OCEAN_MINIMAX_BASE_URL`
// (see `minimax_base_url`) to point back at the mainland platform.
const MINIMAX_BASE_URL: &str = "https://api.minimax.io/v1";
const MOONSHOT_BASE_URL: &str = "https://api.moonshot.ai/v1";
// Google Generative AI (Gemini). Routed through ocean-protocol's
// `google-generative-ai` provider, which targets the v1beta surface under
// this base — not an OpenAI-compatible endpoint.
const GOOGLE_BASE_URL: &str = "https://generativelanguage.googleapis.com";
// Z.AI / Zhipu GLM coding plan (OpenAI-compatible chat-completions; GLM-4/5
// family). The default targets the Z.AI coding plan; override the base with
// `OCEAN_GLM_BASE_URL` to reach the Zhipu coding plan or the bigmodel open
// platform instead.
const GLM_BASE_URL: &str = "https://api.z.ai/api/coding/paas/v4";

/// Stable provider identifier used by Ocean runtime components.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderId {
    DeepSeek,
    OpenAi,
    /// OpenAI Codex over a ChatGPT subscription OAuth token (Responses API).
    OpenAiCodex,
    Anthropic,
    /// Claude Code over Anthropic Messages wire protocol, authenticated with a
    /// Claude Code OAuth bearer token (not an x-api-key).
    ClaudeCode,
    /// MiniMax (OpenAI-compatible chat-completions; M2 family).
    MiniMax,
    /// Moonshot AI / Kimi (OpenAI-compatible chat-completions; K2 family).
    Kimi,
    /// Z.AI / Zhipu GLM (OpenAI-compatible chat-completions; GLM-4/5 family).
    Glm,
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
            Self::ClaudeCode => "claude-code",
            Self::MiniMax => "minimax",
            Self::Kimi => "kimi",
            Self::Glm => "glm",
            Self::Google => "google",
            Self::OpenAiCompatible => "openai-compatible",
            Self::Fake => "fake",
        }
    }

    pub fn credential_env_names(&self) -> &'static [&'static str] {
        match self {
            Self::DeepSeek => &["OCEAN_DEEPSEEK_API_KEY", "DEEPSEEK_API_KEY"],
            Self::OpenAi | Self::OpenAiCompatible => &["OCEAN_OPENAI_API_KEY", "OPENAI_API_KEY"],
            // Codex uses the OAuth token from auth.json / Codex CLI auth, not
            // an env API key.
            Self::OpenAiCodex => &[],
            // Claude Code env credentials are OAuth bearer access tokens.
            Self::ClaudeCode => &["OCEAN_CLAUDE_CODE_ACCESS_TOKEN", "CLAUDE_CODE_ACCESS_TOKEN"],
            Self::Anthropic => &["OCEAN_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"],
            Self::MiniMax => &["OCEAN_MINIMAX_API_KEY", "MINIMAX_API_KEY"],
            Self::Kimi => &["OCEAN_MOONSHOT_API_KEY", "MOONSHOT_API_KEY", "KIMI_API_KEY"],
            Self::Glm => &[
                "OCEAN_GLM_API_KEY",
                "GLM_API_KEY",
                "ZAI_API_KEY",
                "ZHIPUAI_API_KEY",
                "BIGMODEL_API_KEY",
            ],
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
    Env {
        name: String,
    },
    OceanAuthFile {
        path: String,
    },
    /// Codex CLI auth JSON (`OCEAN_CODEX_AUTH_FILE` / `$HOME/.codex/auth.json`),
    /// used as a fallback when the Ocean `openai-codex` OAuth block is absent or
    /// expired.
    CodexCliAuthFile {
        path: String,
    },
    NotRequired,
}

/// Non-secret credential kind: a regular API key vs an OAuth bearer token.
/// Lets Ocean pick a wire auth method (x-api-key vs Authorization: Bearer)
/// without inspecting the secret. See [`ResolvedCredential::kind`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    ApiKey,
    OAuthBearer,
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
    /// How this credential authenticates on the wire — API key vs OAuth bearer.
    pub kind: CredentialKind,
}

impl fmt::Debug for ResolvedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedCredential")
            .field("secret", &self.secret)
            .field("source", &self.source)
            .field("kind", &self.kind)
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
        let credential_kind = self.credential.as_ref().map(|cred| cred.kind.clone());
        let ok = credential_present || !self.selection.provider.requires_credential();
        ProviderReadiness {
            ok,
            provider: self.selection.provider.clone(),
            model: self.selection.model.clone(),
            base_url_host: base_url_host(&self.selection.base_url),
            credential_present,
            credential_source,
            credential_kind,
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
    pub credential_kind: Option<CredentialKind>,
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
    /// Codex CLI auth JSON path (`OCEAN_CODEX_AUTH_FILE`, else
    /// `$HOME/.codex/auth.json`). Fallback credential source for `openai-codex`
    /// when the Ocean auth-file OAuth block is absent or expired.
    pub codex_auth_file: Option<PathBuf>,
}

impl ProviderEnv {
    pub fn from_process() -> Self {
        Self {
            vars: std::env::vars().collect(),
            auth_file: std::env::var_os("OCEAN_AUTH_FILE")
                .map(PathBuf::from)
                .or_else(default_ocean_auth_file),
            codex_auth_file: default_codex_auth_file(),
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

/// Resolve a single provider's credential from the current process
/// environment — env names first, then the Ocean auth file — without pulling
/// the full model-selection config. Public entry for non-turn features that
/// need a raw provider credential, e.g. the daemon's Realtime voice
/// client-secret mint (voice phases 2/3): the browser never holds the key,
/// the daemon resolves it here and mints an ephemeral upstream secret.
pub fn resolve_credential_from_env(
    provider: &ProviderId,
) -> Result<Option<ResolvedCredential>, ProviderConfigError> {
    resolve_credential(&ProviderEnv::from_process(), provider)
}

/// Resolve the xAI API key for voice STT/TTS (voice phase 4). The daemon owns
/// the key so the surface never holds it. Resolution order: env `XAI_API_KEY`,
/// then the Ocean auth file `xai` block (providers/xai/api_key, xai/api_key,
/// xai/key) via the same `ProviderEnv` path the Realtime credential uses so a
/// custom `OCEAN_AUTH_FILE` is honored. Resolved per-request so key rotation
/// needs no daemon restart.
pub fn resolve_xai_api_key() -> Option<String> {
    // 1. Env var.
    if let Ok(key) = std::env::var("XAI_API_KEY") {
        let trimmed = key.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    // 2. Auth file xai block — reuse the same ProviderEnv path as
    //    resolve_credential so OCEAN_AUTH_FILE overrides work.
    let env = ProviderEnv::from_process();
    let path = env.auth_file?;
    let json = read_auth_json(&path).ok()?;
    auth_file_key(&json, "xai").map(str::to_string)
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
    "claude-sonnet-5",  // claude-code oauth
    "gpt-5.4",          // openai-codex
    "deepseek-v4-pro",  // deepseek
    "gemini-2.0-flash", // google
    "kimi-k2.6",        // kimi
    "minimax-m2",       // minimax
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

/// Read the ChatGPT account id from the same source that supplied the Codex
/// credential (valid Ocean `openai-codex` OAuth block, else Codex CLI auth
/// JSON), so the account id and the access token always come from the same
/// place. Returns `None` when no Codex auth source is available.
fn resolve_codex_account_id(env: &ProviderEnv) -> Option<String> {
    resolve_codex_auth(env)
        .ok()
        .flatten()
        .and_then(|auth| auth.account_id)
}

/// Codex auth resolved from the best available source: the raw access token, the
/// optional ChatGPT account id, and a non-secret source label. The token and
/// account id always come from the same source so a request never mixes an Ocean
/// OAuth token with a CLI account id (or vice versa).
struct CodexAuth {
    access_token: String,
    account_id: Option<String>,
    source: CredentialSource,
}

/// Resolve Codex auth from the first source with a usable OAuth access token:
/// 1. A valid (non-expired) `openai-codex` OAuth block in the Ocean auth file.
/// 2. The Codex CLI auth JSON at `OCEAN_CODEX_AUTH_FILE` (or
///    `$HOME/.codex/auth.json` when unset); shape
///    `{ "tokens": { "access_token": "...", "account_id": "..." } }`.
///
/// The Ocean auth file is the configured Ocean credential store, so a read or
/// parse failure there is surfaced as [`ProviderConfigError::InvalidAuthFile`];
/// the CLI fallback is best-effort and silently skipped on any error.
fn resolve_codex_auth(env: &ProviderEnv) -> Result<Option<CodexAuth>, ProviderConfigError> {
    // 1. Ocean auth-file `openai-codex` OAuth block.
    if let Some(path) = &env.auth_file {
        if path.exists() {
            let json = read_auth_json(path)?;
            // `oauth_access_token` rejects missing / expired blocks.
            if let Some(token) = oauth_access_token(&json, "openai-codex") {
                let account_id = json
                    .pointer("/openai-codex/accountId")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string);
                return Ok(Some(CodexAuth {
                    access_token: token.to_string(),
                    account_id,
                    source: CredentialSource::OceanAuthFile {
                        path: path.display().to_string(),
                    },
                }));
            }
        }
    }

    // 2. Codex CLI auth JSON fallback.
    if let Some(path) = &env.codex_auth_file {
        if path.exists() {
            // Best-effort: a malformed CLI file must not break Codex auth.
            if let Ok(json) = read_auth_json(path) {
                if let Some(token) = json
                    .pointer("/tokens/access_token")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                {
                    let account_id = json
                        .pointer("/tokens/account_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(str::to_string);
                    return Ok(Some(CodexAuth {
                        access_token: token.to_string(),
                        account_id,
                        source: CredentialSource::CodexCliAuthFile {
                            path: path.display().to_string(),
                        },
                    }));
                }
            }
        }
    }

    Ok(None)
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
///
/// Every entry here MUST be routable by [`resolve_model_selection`] — the
/// `known_models_are_all_routable` test enforces that. The list deliberately
/// covers exactly the *production* (credential-backed) canonical model ids the
/// resolver recognizes; the keyless `fake`/`fake-ok`/`fake-tool`/`fake-surface`
/// test providers and the `openai-compatible` catch-all are intentionally
/// omitted so they never surface in the public `GET /v1/models` menu. The
/// `id` used here is the exact string the matching arm emits as
/// `selection.model` (i.e. what `GET /v1/models` reports as `current.model`
/// after that model is selected) — so `resolve_model_selection(id).model == id`
/// holds for every entry. ocean-acp builds the advertised mode ids from these
/// `id`s but sets the *selected* mode from `current.model`; if the two diverged
/// (e.g. a lowercase `minimax-m2` id vs the API-cased `MiniMax-M2` the resolver
/// returns) the selected mode would not appear in the advertised list and the
/// ACP/Zed picker would break. The `known_models_id_equals_resolved_model` test
/// enforces this invariant.
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
        m("gpt-5.3-codex-spark", "openai-codex", "GPT-5.3 Codex Spark"),
        m("gpt-4o", "openai", "GPT-4o"),
        m("gpt-4o-mini", "openai", "GPT-4o Mini"),
        // Current Claude generation (verified against api.anthropic.com
        // 2026-07-08: all ids recognized on subscription OAuth; the old
        // 4-6/4-7 ids are retired from the menu but stay routable as legacy
        // arms so pinned sessions keep resolving). The plain ids route through
        // `ProviderId::ClaudeCode` — they assume the Claude Code OAuth login,
        // not a direct Anthropic API key. `claude-code-*` aliases are dropped
        // from the menu (they resolved to the same models); `fable` has no
        // plain alias so it keeps its `claude-code-fable-5` id.
        m("claude-opus-4-8", "claude-code", "Claude Opus 4.8"),
        m("claude-sonnet-5", "claude-code", "Claude Sonnet 5"),
        m("claude-haiku-4-5", "claude-code", "Claude Haiku 4.5"),
        m("claude-code-fable-5", "claude-code", "Claude Code Fable 5"),
        // MiniMax ids use the API casing the resolver returns as current.model
        // (`MiniMax-M2`, not the lowercase alias), so `id == current.model`
        // holds for every entry — ACP/Zed match the selected mode id against the
        // advertised list, and a lowercase alias here would never match. The
        // resolver lowercases the lookup key, so these still route correctly.
        m("MiniMax-M2.7", "minimax", "MiniMax M2.7"),
        m("MiniMax-M2", "minimax", "MiniMax M2"),
        m("kimi-k2.6", "kimi", "Kimi K2.6"),
        m("kimi-k2", "kimi", "Kimi K2"),
        m("glm-5.2", "glm", "GLM 5.2"),
        m("glm-4.7", "glm", "GLM 4.7"),
        m("glm-4.6", "glm", "GLM 4.6"),
        m("glm-4.5", "glm", "GLM 4.5"),
        m("glm-4.5-flash", "glm", "GLM 4.5 Flash"),
        m("gemini-2.0-flash", "google", "Gemini 2.0 Flash"),
    ]
}

/// A known model plus whether its provider credential is actually present in
/// the given environment — the difference between "the menu" and "what the
/// kitchen can cook". `credential_source` names where the credential came from
/// (env var / auth file) when present, so a picker can show it. Serialized
/// flat: `id`/`provider`/`label` stay top-level, keeping `GET /v1/models`
/// backward compatible (readiness is purely additive).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadyModel {
    #[serde(flatten)]
    pub model: KnownModel,
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_source: Option<CredentialSource>,
}

/// Per-model readiness for `GET /v1/models`: resolve each distinct provider's
/// credential once (env vars → Ocean auth file → Codex CLI fallback — the same
/// chain a real turn's selection uses) and stamp every model of that provider.
/// A resolver error (unreadable auth file, unroutable alias) marks the
/// provider not-ready rather than failing the listing.
///
/// Readiness is CONFIGURATION truth, not a liveness probe: a ready provider
/// can still 429/5xx at call time. It answers "could a turn select this model
/// right now", which is what a model picker needs.
pub fn known_models_with_readiness(env: &ProviderEnv) -> Vec<ReadyModel> {
    let mut by_provider: BTreeMap<String, (bool, Option<CredentialSource>)> = BTreeMap::new();
    known_models()
        .into_iter()
        .map(|m| {
            let (ready, credential_source) = by_provider
                .entry(m.provider.clone())
                .or_insert_with(|| {
                    let mut probe = env.clone();
                    probe.vars.insert("OCEAN_MODEL".into(), m.id.clone());
                    match resolve_provider_config(&probe) {
                        Ok(cfg) => {
                            let r = cfg.readiness();
                            (r.ok, r.credential_source)
                        }
                        Err(_) => (false, None),
                    }
                })
                .clone();
            ReadyModel {
                model: m,
                ready,
                credential_source,
            }
        })
        .collect()
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
        // Current Claude generation (2026-07 refresh; verified live). These
        // route through `ProviderId::ClaudeCode` — i.e. they assume the Claude
        // Code OAuth login (CLAUDE_CODE_ACCESS_TOKEN / the auth-file
        // `claude-code` block), NOT a direct Anthropic API key. Same Anthropic
        // Messages wire + base URL; only the auth header differs (Bearer vs
        // x-api-key). The convenience aliases ("sonnet", "opus", "haiku") track
        // the newest ids. Direct-API-key auth for these ids is intentionally
        // not wired (provision later if a custom-model API path is needed).
        "claude-sonnet-5" | "claude-sonnet" | "sonnet" => Ok(model_selection(
            ProviderId::ClaudeCode,
            "claude-sonnet-5",
            ANTHROPIC_BASE_URL,
            200_000,
            16_384,
        )),
        "claude-opus-4-8" | "claude-opus" | "opus" => Ok(model_selection(
            ProviderId::ClaudeCode,
            "claude-opus-4-8",
            ANTHROPIC_BASE_URL,
            200_000,
            16_384,
        )),
        "claude-haiku-4-5" | "claude-haiku" | "haiku" => Ok(model_selection(
            ProviderId::ClaudeCode,
            "claude-haiku-4-5",
            ANTHROPIC_BASE_URL,
            200_000,
            16_384,
        )),
        // LEGACY Claude ids — retired from the menu, kept routable so sessions
        // pinned before the refresh still resolve. Also OAuth (ClaudeCode).
        "claude-sonnet-4-6" => Ok(model_selection(
            ProviderId::ClaudeCode,
            "claude-sonnet-4-6",
            ANTHROPIC_BASE_URL,
            200_000,
            16_384,
        )),
        "claude-opus-4-7" => Ok(model_selection(
            ProviderId::ClaudeCode,
            "claude-opus-4-7",
            ANTHROPIC_BASE_URL,
            200_000,
            16_384,
        )),
        // Claude Code (Anthropic Messages wire protocol, OAuth bearer auth).
        // selection.model preserves the public alias id; ocean-agent maps it to
        // the real Anthropic API model id.
        "claude-code-fable-5" | "claude-code-fable" | "cc-fable" | "fable" => Ok(model_selection(
            ProviderId::ClaudeCode,
            "claude-code-fable-5",
            ANTHROPIC_BASE_URL,
            200_000,
            16_384,
        )),
        "claude-code-opus-4-8" | "claude-code-opus" | "cc-opus" => Ok(model_selection(
            ProviderId::ClaudeCode,
            "claude-code-opus-4-8",
            ANTHROPIC_BASE_URL,
            200_000,
            16_384,
        )),
        "claude-code-sonnet-5" | "claude-code-sonnet" | "cc-sonnet" => Ok(model_selection(
            ProviderId::ClaudeCode,
            "claude-code-sonnet-5",
            ANTHROPIC_BASE_URL,
            200_000,
            16_384,
        )),
        "claude-code-haiku-4-5" | "claude-code-haiku" | "cc-haiku" => Ok(model_selection(
            ProviderId::ClaudeCode,
            "claude-code-haiku-4-5",
            ANTHROPIC_BASE_URL,
            200_000,
            16_384,
        )),
        // LEGACY claude-code ids (see the anthropic legacy block above).
        "claude-code-sonnet-4-6" => Ok(model_selection(
            ProviderId::ClaudeCode,
            "claude-code-sonnet-4-6",
            ANTHROPIC_BASE_URL,
            200_000,
            16_384,
        )),
        "claude-code-opus-4-7" => Ok(model_selection(
            ProviderId::ClaudeCode,
            "claude-code-opus-4-7",
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
            minimax_base_url(env),
            200_000,
            8_192,
        )),
        "minimax-m2.7" | "minimax-m2-7" => Ok(model_selection(
            ProviderId::MiniMax,
            "MiniMax-M2.7",
            minimax_base_url(env),
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
        // Z.AI / Zhipu GLM family (OpenAI-compatible chat-completions). The
        // base defaults to the Z.AI coding plan; `glm_base_url(env)` applies an
        // `OCEAN_GLM_BASE_URL` override (non-empty wins, else the default), so
        // every GLM route — bare alias, explicit provider, and any fallback
        // alias — honors the same override.
        "glm" | "glm-4.6" | "glm-4-6" => Ok(model_selection(
            ProviderId::Glm,
            "glm-4.6",
            glm_base_url(env),
            200_000,
            8_192,
        )),
        "glm-4.7" | "glm-4-7" => Ok(model_selection(
            ProviderId::Glm,
            "glm-4.7",
            glm_base_url(env),
            200_000,
            8_192,
        )),
        "glm-5.2" | "glm-5-2" => Ok(model_selection(
            ProviderId::Glm,
            "glm-5.2",
            glm_base_url(env),
            200_000,
            8_192,
        )),
        "glm-4.5" | "glm-4-5" => Ok(model_selection(
            ProviderId::Glm,
            "glm-4.5",
            glm_base_url(env),
            200_000,
            8_192,
        )),
        "glm-4.5-flash" | "glm-4-5-flash" => Ok(model_selection(
            ProviderId::Glm,
            "glm-4.5-flash",
            glm_base_url(env),
            200_000,
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

/// Resolve the GLM base URL for the current environment: a non-empty
/// `OCEAN_GLM_BASE_URL` override wins, otherwise the Z.AI coding plan default
/// ([`GLM_BASE_URL`]). Mirrors how `OCEAN_OPENAI_BASE_URL` threads through
/// resolution — a set-but-blank value is treated as unset so a misconfigured
/// override can never yield an empty base URL and break a turn.
fn glm_base_url(env: &ProviderEnv) -> &str {
    match env
        .get("OCEAN_GLM_BASE_URL")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(override_url) => override_url,
        None => GLM_BASE_URL,
    }
}

/// Resolve the MiniMax base URL: a non-empty `OCEAN_MINIMAX_BASE_URL` override
/// wins, otherwise the international platform default ([`MINIMAX_BASE_URL`]).
/// Same blank-is-unset rule as [`glm_base_url`].
fn minimax_base_url(env: &ProviderEnv) -> &str {
    match env
        .get("OCEAN_MINIMAX_BASE_URL")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(override_url) => override_url,
        None => MINIMAX_BASE_URL,
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
        "claude-code" => Ok(model_selection(
            ProviderId::ClaudeCode,
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
            minimax_base_url(env),
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
        "glm" | "zhipu" | "zhipuai" => Ok(model_selection(
            ProviderId::Glm,
            model,
            glm_base_url(env),
            200_000,
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

    // Codex auth has its own resolution chain (valid Ocean `openai-codex` OAuth
    // block, else Codex CLI auth JSON) that also keeps the account id in sync,
    // so delegate to the dedicated helper.
    if matches!(provider, ProviderId::OpenAiCodex) {
        return Ok(resolve_codex_auth(env)?.map(|auth| ResolvedCredential {
            secret: SecretString::new(auth.access_token)
                .expect("resolve_codex_auth only yields non-empty tokens"),
            source: auth.source,
            kind: CredentialKind::OAuthBearer,
        }));
    }

    // 1. Plain API keys and provider-specific env credentials.
    for name in provider.credential_env_names() {
        if let Some(secret) = env.get(name).and_then(SecretString::new) {
            let kind = if matches!(provider, ProviderId::ClaudeCode) {
                CredentialKind::OAuthBearer
            } else {
                CredentialKind::ApiKey
            };
            return Ok(Some(ResolvedCredential {
                secret,
                source: CredentialSource::Env {
                    name: (*name).into(),
                },
                kind,
            }));
        }
    }

    let Some(path) = &env.auth_file else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }

    let json = read_auth_json(path)?;
    let source = CredentialSource::OceanAuthFile {
        path: path.display().to_string(),
    };

    // Claude Code authenticates with the OAuth access token from a `claude-code`
    // block (the `anthropic-oauth` block name is accepted as a synonym);
    // everyone else uses a plain api_key.
    let (secret, kind) = if matches!(provider, ProviderId::ClaudeCode) {
        let token = oauth_access_token(&json, "claude-code")
            .or_else(|| oauth_access_token(&json, "anthropic-oauth"));
        (token, CredentialKind::OAuthBearer)
    } else {
        (
            auth_file_key(&json, provider.as_str()),
            CredentialKind::ApiKey,
        )
    };
    Ok(secret
        .and_then(SecretString::new)
        .map(|secret| ResolvedCredential {
            secret,
            source,
            kind,
        }))
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
    let token = entry
        .pointer("/access")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    // Reject expired blocks. `expires` is milliseconds since epoch when large,
    // seconds otherwise; a missing `expires` means "accept" (no expiry known).
    if let Some(expires) = entry
        .pointer("/expires")
        .and_then(serde_json::Value::as_i64)
    {
        let now_secs = unix_epoch_secs();
        let expires_secs = if expires >= 1_000_000_000_000 {
            expires / 1_000
        } else {
            expires
        };
        if expires_secs <= now_secs {
            return None;
        }
    }
    Some(token)
}

/// Current wall clock as Unix seconds, saturating to 0 if the clock is somehow
/// before the epoch (only on a badly misconfigured system).
fn unix_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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

/// Default Codex CLI auth file: `OCEAN_CODEX_AUTH_FILE` when set, otherwise
/// `$HOME/.codex/auth.json` (the Codex CLI's own default location). Mirrors the
/// `auth_file` resolution pattern — the path is populated (when HOME is set) and
/// the reader checks existence.
fn default_codex_auth_file() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("OCEAN_CODEX_AUTH_FILE") {
        return Some(PathBuf::from(path));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex/auth.json"))
}

/// Read and parse an auth JSON file. Read/parse failures are surfaced as
/// [`ProviderConfigError::InvalidAuthFile`] so a malformed configured file is
/// visible rather than silently dropping credentials.
fn read_auth_json(path: &Path) -> Result<serde_json::Value, ProviderConfigError> {
    let text =
        std::fs::read_to_string(path).map_err(|err| ProviderConfigError::InvalidAuthFile {
            path: path.display().to_string(),
            message: err.to_string(),
        })?;
    serde_json::from_str(&text).map_err(|err| ProviderConfigError::InvalidAuthFile {
        path: path.display().to_string(),
        message: err.to_string(),
    })
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
            codex_auth_file: None,
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
    fn anthropic_base_url_has_no_version_suffix() {
        // ocean-protocol's Anthropic provider appends `/v1/messages`; the
        // registry base must therefore be the host root, not `/v1`.
        let selection =
            resolve_model_selection(&env(&[("OCEAN_MODEL", "claude-sonnet-4-6")])).unwrap();
        assert_eq!(selection.provider, ProviderId::ClaudeCode);
        assert_eq!(selection.base_url, "https://api.anthropic.com");

        let claude_code =
            resolve_model_selection(&env(&[("OCEAN_MODEL", "claude-code-sonnet-4-6")])).unwrap();
        assert_eq!(claude_code.provider, ProviderId::ClaudeCode);
        assert_eq!(claude_code.base_url, "https://api.anthropic.com");
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
            codex_auth_file: None,
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
            codex_auth_file: None,
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

    // ---- known_models() <-> resolve_model_selection() (OCEAN-369) ---------

    #[test]
    fn readiness_reflects_which_providers_have_credentials() {
        // Only deepseek gets a key: its models are ready, everyone else's are
        // not, and the flat serde shape keeps id/provider/label at top level.
        let env = env(&[("DEEPSEEK_API_KEY", "sk-test")]);
        let listed = known_models_with_readiness(&env);
        assert_eq!(listed.len(), known_models().len());
        for entry in &listed {
            let expect = entry.model.provider == "deepseek";
            assert_eq!(
                entry.ready, expect,
                "model {:?} (provider {}) readiness should be {expect}",
                entry.model.id, entry.model.provider,
            );
        }
        let json = serde_json::to_value(&listed[0]).unwrap();
        assert!(json.get("id").is_some(), "flatten keeps id top-level");
        assert!(json.get("ready").is_some());
    }

    #[test]
    fn known_models_are_all_routable() {
        // Every model the public picker advertises must actually route through
        // resolve_model_selection (passed as OCEAN_MODEL, the way a client
        // round-trips a picked id), and reach the provider the menu declares.
        // Otherwise GET /v1/models lists a model that fails the moment it's
        // selected.
        for known in known_models() {
            let selection = resolve_model_selection(&env(&[("OCEAN_MODEL", &known.id)]))
                .unwrap_or_else(|e| panic!("known model {:?} is not routable: {e}", known.id));
            assert_eq!(
                selection.provider.as_str(),
                known.provider,
                "known model {:?} routed to provider {} but the menu declares {}",
                known.id,
                selection.provider.as_str(),
                known.provider,
            );
        }
    }

    #[test]
    fn known_models_id_equals_resolved_model() {
        // The ACP invariant (OCEAN-369 Codex follow-up): ocean-acp advertises
        // mode ids from KnownModel.id but sets the *selected* mode from the
        // resolver's current.model. If those diverge for any model, the selected
        // mode id isn't in the advertised list and Zed/ACP pickers break. So
        // every advertised id must satisfy resolve_model_selection(id).model == id.
        // This is exactly what would have caught the lowercase `minimax-m2` id vs
        // the API-cased `MiniMax-M2` the resolver returns.
        for known in known_models() {
            let selection = resolve_model_selection(&env(&[("OCEAN_MODEL", &known.id)]))
                .unwrap_or_else(|e| panic!("known model {:?} is not routable: {e}", known.id));
            assert_eq!(
                selection.model, known.id,
                "known_models() id {:?} != resolve_model_selection's current.model {:?} \
                 — ACP would advertise {:?} but select {:?}, breaking the picker",
                known.id, selection.model, known.id, selection.model,
            );
        }
    }

    #[test]
    fn no_routable_production_model_is_missing_from_known_models() {
        // The inverse guard: enumerate every production (credential-backed)
        // canonical model id resolve_model_selection can route via its bare-alias
        // arms, and assert the public picker lists each one. Keyless `fake*`
        // variants and the `openai-compatible` catch-all are intentionally
        // excluded from the menu and so are not listed here. If a new production
        // arm is added to resolve_model_selection, add it here AND to
        // known_models() — this test is the tripwire that forces that.
        let routable_production_ids = [
            "deepseek-chat",
            "deepseek-reasoner",
            "deepseek-v4-flash",
            "deepseek-v4-pro",
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-5.5",
            "gpt-5.4",
            "gpt-5.4-mini",
            "gpt-5.3-codex-spark",
            // Current Claude generation (2026-07 refresh). The retired
            // 4-6/4-7 ids stay ROUTABLE (legacy arms, pinned sessions) but are
            // deliberately NOT in the menu, so they're absent here too.
            "claude-opus-4-8",
            "claude-sonnet-5",
            "claude-haiku-4-5",
            "claude-code-fable-5",
            // API-cased ids: `resolve_model_selection` returns these as
            // current.model, and known_models() advertises the same string.
            "MiniMax-M2",
            "MiniMax-M2.7",
            "kimi-k2.6",
            "kimi-k2",
            "glm-5.2",
            "glm-4.7",
            "glm-4.6",
            "glm-4.5",
            "glm-4.5-flash",
            "gemini-2.0-flash",
        ];
        let listed: std::collections::BTreeSet<String> =
            known_models().into_iter().map(|m| m.id).collect();
        for id in routable_production_ids {
            // Sanity: each really is routable (canonical id round-trips).
            resolve_model_selection(&env(&[("OCEAN_MODEL", id)]))
                .unwrap_or_else(|e| panic!("expected {id:?} to be routable: {e}"));
            assert!(
                listed.contains(id),
                "routable production model {id:?} is missing from known_models()"
            );
        }
        // Every listed id must be one of the enumerated production ids — catches
        // a fake/test variant accidentally leaking into the public menu.
        let production: std::collections::BTreeSet<&str> =
            routable_production_ids.into_iter().collect();
        for id in &listed {
            assert!(
                production.contains(id.as_str()),
                "known_models() lists {id:?}, which is not an enumerated production model \
                 (a fake/test variant must never appear in the public picker)"
            );
        }
    }

    // ---- Fallback / failover (OCEAN-275) ----------------------------------

    #[test]
    fn fallback_picks_a_ready_alternate_when_primary_provider_is_degraded() {
        // Primary = deepseek (its key is intentionally absent → degraded), but a
        // Claude Code OAuth bearer IS present. The default order leads with
        // claude-sonnet-5 (now ClaudeCode), so the first ready alternate must be
        // ClaudeCode.
        let e = env(&[
            ("OCEAN_MODEL", "deepseek-v4-pro"),
            ("CLAUDE_CODE_ACCESS_TOKEN", "cc-bearer"),
        ]);
        let alt = resolve_fallback_config(&e, &ProviderId::DeepSeek)
            .expect("a ready claude-code alternate should be found");
        assert_eq!(alt.selection.provider, ProviderId::ClaudeCode);
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
        // Claude Code bearer present; if the primary is ALSO ClaudeCode, the
        // claude entry must be excluded — failing over to the provider that just
        // failed is pointless. With no other key set, there's no alternate at all.
        let e = env(&[
            ("OCEAN_MODEL", "claude-opus-4-7"),
            ("CLAUDE_CODE_ACCESS_TOKEN", "cc-bearer"),
        ]);
        let alt = resolve_fallback_config(&e, &ProviderId::ClaudeCode);
        assert!(
            alt.is_none(),
            "claude-code is the primary; it must not be its own fallback"
        );
    }

    #[test]
    fn fallback_dedupes_by_provider_and_honors_priority_order() {
        // Both deepseek and Claude Code bearers present. Primary = google
        // (degraded). Default order is claude-code, then codex, then deepseek…
        // → the first ready alternate is ClaudeCode, and the candidate list holds
        // at most one entry per provider.
        let e = env(&[
            ("OCEAN_MODEL", "gemini-2.0-flash"),
            ("CLAUDE_CODE_ACCESS_TOKEN", "cc-bearer"),
            ("OCEAN_DEEPSEEK_API_KEY", "ds-secret"),
        ]);
        let candidates = fallback_candidates(&e, &ProviderId::Google);
        assert_eq!(
            candidates.first().map(|c| c.selection.provider.clone()),
            Some(ProviderId::ClaudeCode),
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
        // falls back to the default order (claude-code-first here).
        let e = env(&[
            ("OCEAN_MODEL", "deepseek-v4-pro"),
            ("OCEAN_PROVIDER_FALLBACK", "  , ,"),
            ("CLAUDE_CODE_ACCESS_TOKEN", "cc-bearer"),
        ]);
        let alt = resolve_fallback_config(&e, &ProviderId::DeepSeek).unwrap();
        assert_eq!(alt.selection.provider, ProviderId::ClaudeCode);
    }

    #[test]
    fn unknown_fallback_entries_are_skipped_not_fatal() {
        // A typo'd alias in the override is skipped; the next valid+ready entry
        // still wins.
        let e = env(&[
            ("OCEAN_MODEL", "gemini-2.0-flash"),
            ("OCEAN_PROVIDER_FALLBACK", "not-a-model, claude-opus-4-7"),
            ("CLAUDE_CODE_ACCESS_TOKEN", "cc-bearer"),
        ]);
        let alt = resolve_fallback_config(&e, &ProviderId::Google).unwrap();
        assert_eq!(alt.selection.provider, ProviderId::ClaudeCode);
    }
    // ---- GLM provider (Zhipu AI) -----------------------------------------

    #[test]
    fn glm_routes_and_resolves_zhipuai_credential() {
        // GLM routes through a new OpenAI-compatible provider and resolves a
        // credential from any of the Zhipu/GLM env names (ZHIPUAI_API_KEY here).
        let config = resolve_provider_config(&env(&[
            ("OCEAN_MODEL", "glm-4.6"),
            ("ZHIPUAI_API_KEY", "glm-secret"),
        ]))
        .unwrap();
        assert_eq!(config.selection.provider, ProviderId::Glm);
        assert_eq!(config.selection.model, "glm-4.6");
        assert_eq!(config.selection.base_url, GLM_BASE_URL);
        assert_eq!(config.selection.context_window, 200_000);
        assert_eq!(config.selection.max_output_tokens, 8_192);
        let credential = config
            .credential
            .as_ref()
            .expect("ZHIPUAI_API_KEY should resolve a GLM credential");
        assert_eq!(credential.secret.expose(), "glm-secret");
        assert_eq!(credential.kind, CredentialKind::ApiKey);
        assert!(config.readiness().ok);
    }

    #[test]
    fn glm_45_flash_routes_to_glm_provider() {
        let selection = resolve_model_selection(&env(&[("OCEAN_MODEL", "glm-4.5-flash")])).unwrap();
        assert_eq!(selection.provider, ProviderId::Glm);
        assert_eq!(selection.model, "glm-4.5-flash");
        assert_eq!(selection.base_url, GLM_BASE_URL);
    }

    #[test]
    fn glm_default_base_is_the_zai_coding_plan() {
        // The default GLM base targets the Z.AI coding plan (OpenAI-compatible
        // chat-completions), not the legacy bigmodel open platform. Asserted
        // literally so a future const edit can't silently drift the default,
        // and a blank OCEAN_GLM_BASE_URL is treated as unset (can't yield an
        // empty base).
        assert_eq!(GLM_BASE_URL, "https://api.z.ai/api/coding/paas/v4");
        let selection = resolve_model_selection(&env(&[("OCEAN_MODEL", "glm-4.6")])).unwrap();
        assert_eq!(selection.provider, ProviderId::Glm);
        assert_eq!(selection.base_url, "https://api.z.ai/api/coding/paas/v4");
        let blank = resolve_model_selection(&env(&[
            ("OCEAN_MODEL", "glm-4.6"),
            ("OCEAN_GLM_BASE_URL", "   "),
        ]))
        .unwrap();
        assert_eq!(
            blank.base_url, "https://api.z.ai/api/coding/paas/v4",
            "a blank override must fall back to the default base"
        );
    }

    #[test]
    fn ocean_glm_base_url_override_wins_on_every_glm_route() {
        // A non-empty override must win on both the known-id path
        // (resolve_model_selection) and the explicit-provider path
        // (model_for_explicit_provider via OCEAN_PROVIDER), mirroring how
        // OCEAN_OPENAI_BASE_URL threads through resolution.
        const ZHIPU_CODING: &str = "https://open.bigmodel.cn/api/coding/paas/v4";
        let known = resolve_model_selection(&env(&[
            ("OCEAN_MODEL", "glm-4.5"),
            ("OCEAN_GLM_BASE_URL", ZHIPU_CODING),
        ]))
        .unwrap();
        assert_eq!(known.base_url, ZHIPU_CODING);
        let explicit = resolve_model_selection(&env(&[
            ("OCEAN_PROVIDER", "glm"),
            ("OCEAN_MODEL", "glm-4.5"),
            ("OCEAN_GLM_BASE_URL", ZHIPU_CODING),
        ]))
        .unwrap();
        assert_eq!(explicit.provider, ProviderId::Glm);
        assert_eq!(explicit.base_url, ZHIPU_CODING);
        // The zhipu/zhipuai synonyms hit the same explicit arm and the same
        // override.
        let synonym = resolve_model_selection(&env(&[
            ("OCEAN_PROVIDER", "zhipuai"),
            ("OCEAN_MODEL", "glm-4.5"),
            ("OCEAN_GLM_BASE_URL", ZHIPU_CODING),
        ]))
        .unwrap();
        assert_eq!(synonym.base_url, ZHIPU_CODING);
    }

    #[test]
    fn zai_api_key_resolves_a_ready_glm_credential() {
        // ZAI_API_KEY is promoted above ZHIPUAI_API_KEY (the default base is
        // now z.ai), so it alone must authenticate a ready GLM config.
        let config = resolve_provider_config(&env(&[
            ("OCEAN_MODEL", "glm-4.6"),
            ("ZAI_API_KEY", "zai-secret"),
        ]))
        .unwrap();
        let credential = config
            .credential
            .as_ref()
            .expect("ZAI_API_KEY should resolve a GLM credential");
        assert_eq!(credential.secret.expose(), "zai-secret");
        assert_eq!(credential.kind, CredentialKind::ApiKey);
        assert_eq!(
            credential.source,
            CredentialSource::Env {
                name: "ZAI_API_KEY".into()
            }
        );
        assert!(config.readiness().ok);
    }

    #[test]
    fn glm_47_and_52_route_and_round_trip_through_known_models() {
        // New GLM ids route to ProviderId::Glm with the coding-plan base and
        // the shared 200k/8k limits, their hyphen aliases normalize to the same
        // canonical id, and they survive the known_models round-trip.
        for (id, hyphen) in [("glm-4.7", "glm-4-7"), ("glm-5.2", "glm-5-2")] {
            let sel = resolve_model_selection(&env(&[("OCEAN_MODEL", id)])).unwrap();
            assert_eq!(sel.provider, ProviderId::Glm);
            assert_eq!(sel.model, id);
            assert_eq!(sel.base_url, GLM_BASE_URL);
            assert_eq!(sel.context_window, 200_000);
            assert_eq!(sel.max_output_tokens, 8_192);
            let hyphenated = resolve_model_selection(&env(&[("OCEAN_MODEL", hyphen)])).unwrap();
            assert_eq!(hyphenated.model, id);
            assert_eq!(hyphenated.provider, ProviderId::Glm);
        }
        let listed: std::collections::BTreeSet<String> =
            known_models().into_iter().map(|m| m.id).collect();
        assert!(listed.contains("glm-4.7"), "glm-4.7 must be in the picker");
        assert!(listed.contains("glm-5.2"), "glm-5.2 must be in the picker");
    }

    #[test]
    fn glm_env_order_picks_glm_api_key_over_zai_api_key() {
        // credential_env_names order is OCEAN_GLM_API_KEY, GLM_API_KEY,
        // ZAI_API_KEY, ZHIPUAI_API_KEY, BIGMODEL_API_KEY. When both GLM_API_KEY
        // and ZAI_API_KEY are set, the earlier name wins and is reported as the
        // credential source.
        let config = resolve_provider_config(&env(&[
            ("OCEAN_MODEL", "glm-4.6"),
            ("GLM_API_KEY", "glm-first"),
            ("ZAI_API_KEY", "zai-second"),
        ]))
        .unwrap();
        let credential = config
            .credential
            .as_ref()
            .expect("credential should resolve");
        assert_eq!(credential.secret.expose(), "glm-first");
        assert_eq!(
            credential.source,
            CredentialSource::Env {
                name: "GLM_API_KEY".into()
            }
        );
    }

    // ---- Claude Code provider (Anthropic Messages + OAuth bearer) --------

    #[test]
    fn claude_code_alias_routes_with_oauth_and_is_listed() {
        let dir =
            std::env::temp_dir().join(format!("ocean-claude-code-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        fs::write(
            &path,
            r#"{"claude-code":{"type":"oauth","access":"cc-bearer-token","refresh":"rt_cc","expires":9999999999999}}"#,
        )
        .unwrap();

        let config = resolve_provider_config(&ProviderEnv {
            vars: BTreeMap::from([("OCEAN_MODEL".into(), "claude-code-sonnet-5".into())]),
            auth_file: Some(path.clone()),
            codex_auth_file: None,
        })
        .unwrap();
        assert_eq!(config.selection.provider, ProviderId::ClaudeCode);
        // selection.model preserves the public alias id; ocean-agent maps it to
        // the real Anthropic API model id.
        assert_eq!(config.selection.model, "claude-code-sonnet-5");
        assert_eq!(config.selection.base_url, ANTHROPIC_BASE_URL);
        let credential = config
            .credential
            .as_ref()
            .expect("claude-code oauth block should resolve");
        assert_eq!(credential.secret.expose(), "cc-bearer-token");
        assert_eq!(credential.kind, CredentialKind::OAuthBearer);
        assert!(config.readiness().ok);

        // The public Claude aliases are advertised in the model picker. The plain
        // ids (claude-sonnet-5 etc.) are the menu entries now; claude-code-* are
        // legacy aliases kept routable but off the menu (except fable).
        let listed: std::collections::BTreeSet<String> =
            known_models().into_iter().map(|m| m.id).collect();
        assert!(listed.contains("claude-sonnet-5"));
        assert!(listed.contains("claude-opus-4-8"));
        assert!(listed.contains("claude-haiku-4-5"));
        assert!(listed.contains("claude-code-fable-5"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_code_accepts_anthropic_oauth_block_synonym() {
        let dir =
            std::env::temp_dir().join(format!("ocean-claude-code-syn-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        fs::write(
            &path,
            r#"{"anthropic-oauth":{"type":"oauth","access":"syn-bearer","refresh":"rt","expires":9999999999999}}"#,
        )
        .unwrap();

        let config = resolve_provider_config(&ProviderEnv {
            vars: BTreeMap::from([("OCEAN_MODEL".into(), "claude-code-opus-4-7".into())]),
            auth_file: Some(path.clone()),
            codex_auth_file: None,
        })
        .unwrap();
        assert_eq!(config.selection.provider, ProviderId::ClaudeCode);
        let credential = config
            .credential
            .as_ref()
            .expect("anthropic-oauth synonym should resolve");
        assert_eq!(credential.secret.expose(), "syn-bearer");
        assert_eq!(credential.kind, CredentialKind::OAuthBearer);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_code_env_bearer_token_is_used() {
        // With no auth file, an env bearer token authenticates Claude Code.
        let config = resolve_provider_config(&env(&[
            ("OCEAN_MODEL", "claude-code-sonnet-4-6"),
            ("CLAUDE_CODE_ACCESS_TOKEN", "env-bearer"),
        ]))
        .unwrap();
        let credential = config
            .credential
            .as_ref()
            .expect("env bearer token should resolve");
        assert_eq!(credential.secret.expose(), "env-bearer");
        assert_eq!(credential.kind, CredentialKind::OAuthBearer);
        assert_eq!(
            credential.source,
            CredentialSource::Env {
                name: "CLAUDE_CODE_ACCESS_TOKEN".into()
            }
        );
    }

    // ---- Codex OAuth fallback to Codex CLI auth JSON ---------------------

    #[test]
    fn expired_codex_oauth_ignored_codex_cli_fallback_supplies_token_and_account_id() {
        let dir = std::env::temp_dir().join(format!("ocean-codex-cli-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Ocean auth file carries an EXPIRED openai-codex OAuth block, so it must
        // be ignored in favor of the Codex CLI fallback below.
        let ocean_path = dir.join("auth.json");
        fs::write(
            &ocean_path,
            r#"{"openai-codex":{"type":"oauth","access":"ocean-bearer-expired","refresh":"rt","expires":1,"accountId":"ocean-acct-should-not-win"}}"#,
        )
        .unwrap();

        // Codex CLI auth JSON (the shape the Codex CLI writes), pointed at by
        // OCEAN_CODEX_AUTH_FILE (modeled here via ProviderEnv.codex_auth_file).
        let cli_path = dir.join("codex-auth.json");
        fs::write(
            &cli_path,
            r#"{"tokens":{"access_token":"cli-token","account_id":"cli-acct-456"}}"#,
        )
        .unwrap();

        let config = resolve_provider_config(&ProviderEnv {
            vars: BTreeMap::from([("OCEAN_MODEL".into(), "gpt-5.4".into())]),
            auth_file: Some(ocean_path.clone()),
            codex_auth_file: Some(cli_path.clone()),
        })
        .unwrap();
        assert_eq!(config.selection.provider, ProviderId::OpenAiCodex);
        let credential = config
            .credential
            .as_ref()
            .expect("codex cli fallback should resolve a credential");
        assert_eq!(credential.secret.expose(), "cli-token");
        assert_eq!(credential.kind, CredentialKind::OAuthBearer);
        assert_eq!(
            credential.source,
            CredentialSource::CodexCliAuthFile {
                path: cli_path.display().to_string()
            }
        );
        // account id must come from the SAME source as the token (CLI fallback).
        assert_eq!(config.account_id.as_deref(), Some("cli-acct-456"));

        let _ = fs::remove_dir_all(&dir);
    }
}
