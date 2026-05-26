//! Ocean-owned provider registry, model mapping, auth resolution, and readiness.
//!
//! This crate is intentionally independent from Pi runtime/auth types. It owns the
//! provider/auth decision for Ocean callers; temporary adapters can translate the
//! resolved config into legacy runtime structs at the edge.

use std::{collections::BTreeMap, fmt, path::PathBuf};

use serde::{Deserialize, Serialize};

const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/v1";
const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com/v1";

/// Stable provider identifier used by Ocean runtime components.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderId {
    DeepSeek,
    OpenAi,
    Anthropic,
    OpenAiCompatible,
    Fake,
}

impl ProviderId {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DeepSeek => "deepseek",
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::OpenAiCompatible => "openai-compatible",
            Self::Fake => "fake",
        }
    }

    pub fn credential_env_names(&self) -> &'static [&'static str] {
        match self {
            Self::DeepSeek => &["OCEAN_DEEPSEEK_API_KEY", "DEEPSEEK_API_KEY"],
            Self::OpenAi | Self::OpenAiCompatible => &["OCEAN_OPENAI_API_KEY", "OPENAI_API_KEY"],
            Self::Anthropic => &["OCEAN_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"],
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
    UnknownModel { model: String },
    MissingBaseUrl { provider: String },
    MissingCredential { provider: String },
    InvalidAuthFile { path: String, message: String },
}

impl fmt::Display for ProviderConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownModel { model } => write!(f, "unknown model '{model}'"),
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
    Ok(ProviderConfig {
        selection,
        credential,
    })
}

/// Resolve model selection without reading credential values.
pub fn resolve_model_selection(env: &ProviderEnv) -> Result<ModelSelection, ProviderConfigError> {
    let model = env
        .get("OCEAN_MODEL")
        .or_else(|| env.get("PI_MODEL"))
        .unwrap_or("deepseek-chat")
        .trim();
    let provider_override = env.get("OCEAN_PROVIDER").map(str::trim);

    if let Some(provider) = provider_override {
        return model_for_explicit_provider(provider, model, env);
    }

    match model {
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
        "fake" | "fake-ok" => Ok(model_selection(
            ProviderId::Fake,
            "fake-ok",
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
        "anthropic" => Ok(model_selection(
            ProviderId::Anthropic,
            model,
            ANTHROPIC_BASE_URL,
            200_000,
            16_384,
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

    let key = auth_file_key(&json, provider.as_str()).and_then(SecretString::new);
    Ok(key.map(|secret| ResolvedCredential {
        secret,
        source: CredentialSource::OceanAuthFile {
            path: path.display().to_string(),
        },
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
}
