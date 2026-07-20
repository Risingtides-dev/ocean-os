//! One-time startup config validation (OCEAN-276).
//!
//! Critical runtime env (telephony, STT/TTS, retry policy, Longhouse) is read
//! *lazily* deep inside the call/token/provider handlers — so a misconfigured
//! deployment looks healthy at boot and only fails when the operator places the
//! first call (or fires the first turn). This module runs ONCE at boot and:
//!
//! - **Fail-fast** on a value that is *set but malformed* — a `LIVEKIT_URL` that
//!   isn't a URL, an `OCEAN_CALL_CALLER_NUMBER` that isn't E.164, a numeric env
//!   (`OCEAN_RETRY_MAX_ATTEMPTS`, the call timeouts, the Longhouse timeout, the
//!   shutdown grace) that isn't a number, a bad `OCEAN_LONGHOUSE_MODE`, an
//!   `OCEAN_ASSISTANTS_DIR` that exists but isn't a directory. The daemon
//!   refuses to boot rather than carry a typo to first use.
//! - **Warn** on a *partially / inconsistently configured feature* — LiveKit
//!   key without secret, an outbound trunk without LiveKit creds, telephony
//!   configured but no publish token — so the operator sees the gap at boot,
//!   not at first call.
//! - **Log a readiness summary** — which optional features (telephony, in-room
//!   voice, STT, TTS, providers, Longhouse) are configured/ready vs unconfigured
//!   — one boot line so the operator knows what's live.
//!
//! Crucially this NEVER *requires* an optional feature: a daemon with no
//! telephony/STT/provider creds must still boot fine for non-call use. We only
//! fail-fast on MALFORMED values, and warn on partial feature config.

use std::env;

use ocean_longhouse::LonghouseMode;

/// A single fatal config problem: an env var is set but its value is malformed.
struct ConfigError {
    var: &'static str,
    value: String,
    reason: String,
}

/// Run the full startup validation pass. Returns `Err` (so `main` aborts boot)
/// when any set env var is malformed; otherwise `Ok(())` after emitting any
/// partial-feature warnings and the readiness summary.
///
/// Pure over the process environment, so the tests below drive it by setting /
/// clearing real env vars. It performs no I/O beyond a single `Path::is_dir`
/// stat for `OCEAN_ASSISTANTS_DIR`.
pub fn validate_startup_config() -> anyhow::Result<()> {
    let mut errors: Vec<ConfigError> = Vec::new();

    validate_bind(&mut errors);
    validate_livekit_url(&mut errors);
    validate_caller_number(&mut errors);
    validate_u64_envs(&mut errors);
    validate_longhouse(&mut errors);
    validate_assistants_dir(&mut errors);

    if !errors.is_empty() {
        // Log each malformed var clearly (operators read the daemon log), then
        // return a single aggregated error so the process exits non-zero with a
        // message naming everything that's wrong — not just the first.
        for e in &errors {
            tracing::error!(
                var = e.var,
                value = %redact_if_secret(e.var, &e.value),
                reason = %e.reason,
                "startup config invalid"
            );
        }
        let summary = errors
            .iter()
            .map(|e| format!("{} ({})", e.var, e.reason))
            .collect::<Vec<_>>()
            .join("; ");
        anyhow::bail!("startup config validation failed: {summary}");
    }

    // Past fail-fast: nothing malformed. Now surface partial-feature gaps and the
    // readiness summary so the operator sees, at boot, exactly what's live.
    warn_partial_features();
    log_readiness_summary();

    Ok(())
}

/// Read an env var as a trimmed, non-empty `Option<String>`. Unset, empty, and
/// whitespace-only all collapse to `None` — the "absent" case every optional
/// feature treats as "not configured".
fn opt(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// True when the var is set to a non-empty value.
fn is_set(key: &str) -> bool {
    opt(key).is_some()
}

/// Redact obvious secrets in log output. Keys/secrets/tokens never belong in a
/// log line even when malformed.
fn redact_if_secret(var: &str, value: &str) -> String {
    let upper = var.to_ascii_uppercase();
    if upper.contains("KEY") || upper.contains("SECRET") || upper.contains("TOKEN") {
        "<redacted>".to_string()
    } else {
        value.to_string()
    }
}

/// `OCEAN_BIND` must parse as a `SocketAddr`. The daemon already re-parses it at
/// listen time (`main`), but that's after the runtime + DBs are built — by then
/// a typo'd bind has cost the whole boot. Validate it up front.
fn validate_bind(errors: &mut Vec<ConfigError>) {
    if let Some(bind) = opt("OCEAN_BIND") {
        if bind.parse::<std::net::SocketAddr>().is_err() {
            errors.push(ConfigError {
                var: "OCEAN_BIND",
                value: bind,
                reason: "not a valid host:port socket address".to_string(),
            });
        }
    }
}

/// `LIVEKIT_URL`, when set, must be an http(s)/wss URL — the exact shape
/// `ocean_call::{SipConfig,LiveKitTokenConfig}::validate` enforce at call time.
fn validate_livekit_url(errors: &mut Vec<ConfigError>) {
    if let Some(url) = opt("LIVEKIT_URL") {
        if !url.starts_with("http") && !url.starts_with("wss") {
            errors.push(ConfigError {
                var: "LIVEKIT_URL",
                value: url,
                reason: "must be an http(s)/wss URL".to_string(),
            });
        }
    }
}

/// `OCEAN_CALL_CALLER_NUMBER`, when set, must normalize to a valid E.164 number
/// — the same check `SipConfig::validate` makes, hoisted to boot so a bad caller
/// id fails the daemon at startup instead of at first dial.
fn validate_caller_number(errors: &mut Vec<ConfigError>) {
    if let Some(num) = opt("OCEAN_CALL_CALLER_NUMBER") {
        if ocean_call::normalize_e164(&num).is_none() {
            errors.push(ConfigError {
                var: "OCEAN_CALL_CALLER_NUMBER",
                value: num,
                reason: "not a valid E.164 phone number".to_string(),
            });
        }
    }
}

/// Numeric env vars that are read lazily with lenient `parse().ok()` fallbacks
/// deep in the providers / call loop / shutdown path. A typo today is silently
/// ignored (and the default used), so a deliberate operator override that's
/// fat-fingered quietly does nothing. Validate them up front: if set, they must
/// parse as `u64`.
fn validate_u64_envs(errors: &mut Vec<ConfigError>) {
    const NUMERIC_U64_ENVS: &[&str] = &[
        // Provider retry policy (ocean-protocol::retry).
        ocean_protocol::retry::ENV_MAX_ATTEMPTS,
        ocean_protocol::retry::ENV_BASE_BACKOFF_MS,
        ocean_protocol::retry::ENV_MAX_BACKOFF_MS,
        // Call-loop turn timeouts (ocean-call::session_task).
        "OCEAN_CALL_SUMMARY_TIMEOUT_MS",
        "OCEAN_CALL_ANSWER_TIMEOUT_MS",
        // Longhouse request timeout (ocean-longhouse::config).
        "OCEAN_LONGHOUSE_TIMEOUT_MS",
        // Graceful-shutdown drain budget (this crate's `shutdown_grace`).
        "OCEAN_SHUTDOWN_GRACE_SECS",
    ];

    for &key in NUMERIC_U64_ENVS {
        if let Some(raw) = opt(key) {
            if raw.parse::<u64>().is_err() {
                errors.push(ConfigError {
                    var: key,
                    value: raw,
                    reason: "must be a non-negative integer".to_string(),
                });
            }
        }
    }
}

/// Longhouse mode + URL. `OCEAN_LONGHOUSE_MODE`, when set, must be one of the
/// recognized modes (otherwise `LonghouseConfig::from_env` silently keeps the
/// default `embedded`, masking the operator's intent). `OCEAN_LONGHOUSE_URL`,
/// when set, must look like an http(s)/wss URL.
fn validate_longhouse(errors: &mut Vec<ConfigError>) {
    if let Some(mode) = opt("OCEAN_LONGHOUSE_MODE") {
        if LonghouseMode::parse(&mode).is_none() {
            errors.push(ConfigError {
                var: "OCEAN_LONGHOUSE_MODE",
                value: mode,
                reason: "must be one of: disabled | embedded | local | remote".to_string(),
            });
        }
    }
    if let Some(url) = opt("OCEAN_LONGHOUSE_URL") {
        if !url.starts_with("http") && !url.starts_with("wss") {
            errors.push(ConfigError {
                var: "OCEAN_LONGHOUSE_URL",
                value: url,
                reason: "must be an http(s)/wss URL".to_string(),
            });
        }
    }
}

/// `OCEAN_ASSISTANTS_DIR`, when set, points at the editable per-surface profile
/// tree the agent reads at turn time. If the path exists but is a *file* (not a
/// directory), every profile read silently misses and surfaces fall back to the
/// compiled-in seed — a confusing, silent misconfig. If it doesn't exist yet
/// that's fine (it's created/optional), so we only fail on exists-but-not-a-dir.
fn validate_assistants_dir(errors: &mut Vec<ConfigError>) {
    if let Some(dir) = opt("OCEAN_ASSISTANTS_DIR") {
        let path = std::path::Path::new(&dir);
        if path.exists() && !path.is_dir() {
            errors.push(ConfigError {
                var: "OCEAN_ASSISTANTS_DIR",
                value: dir,
                reason: "path exists but is not a directory".to_string(),
            });
        }
    }
}

/// Warn on partially / inconsistently configured optional features. None of
/// these abort boot — the daemon runs fine without the feature — but each gap
/// means a feature the operator *partly* configured silently won't work, so we
/// say so at boot rather than at first use.
fn warn_partial_features() {
    let lk_url = is_set("LIVEKIT_URL");
    let lk_key = is_set("LIVEKIT_API_KEY");
    let lk_secret = is_set("LIVEKIT_API_SECRET");
    let trunk = is_set("OCEAN_CALL_OUTBOUND_TRUNK");
    let caller = is_set("OCEAN_CALL_CALLER_NUMBER");

    // LiveKit auth pair: one half without the other mints nothing (in-room voice
    // tokens AND the call tap both need both halves).
    if lk_key != lk_secret {
        let missing = if lk_key {
            "LIVEKIT_API_SECRET"
        } else {
            "LIVEKIT_API_KEY"
        };
        tracing::warn!(
            missing,
            "LiveKit partially configured: in-room voice tokens and the call tap \
             need BOTH LIVEKIT_API_KEY and LIVEKIT_API_SECRET — minting is disabled \
             until {missing} is set"
        );
    }

    // Outbound telephony needs the full set (LIVEKIT_URL/_KEY/_SECRET + trunk +
    // caller). If the operator set *some* call vars but not the whole set,
    // outbound dialing 503s at first call — surface exactly what's missing now.
    let any_telephony = lk_url || lk_key || lk_secret || trunk || caller;
    let all_telephony = lk_url && lk_key && lk_secret && trunk && caller;
    if any_telephony && !all_telephony {
        let mut missing = Vec::new();
        if !lk_url {
            missing.push("LIVEKIT_URL");
        }
        if !lk_key {
            missing.push("LIVEKIT_API_KEY");
        }
        if !lk_secret {
            missing.push("LIVEKIT_API_SECRET");
        }
        if !trunk {
            missing.push("OCEAN_CALL_OUTBOUND_TRUNK");
        }
        if !caller {
            missing.push("OCEAN_CALL_CALLER_NUMBER");
        }
        tracing::warn!(
            missing = ?missing,
            "outbound telephony partially configured: POST /v1/calls/place will 503 \
             until these are set"
        );
    }

    // Telephony fully wired, but no publish token → fail-closed: no HTTP caller
    // can publish audio/video into a room. That's the intended default, but worth
    // a one-line heads-up so a silent "why can't the web client speak" is visible.
    if all_telephony && !is_set("OCEAN_LIVEKIT_PUBLISH_TOKEN") {
        tracing::warn!(
            "telephony is configured but OCEAN_LIVEKIT_PUBLISH_TOKEN is unset: \
             no HTTP caller can mint a publish-capable LiveKit token (fail-closed). \
             Set it to let the operator's surface publish into call rooms."
        );
    }

    // STT entitlement: streaming is gated on DEEPGRAM_API_KEY AND the
    // `deepgram-stt` build feature. With the key but not the feature, the live
    // call silently falls back to batch xAI STT — warn so the operator knows the
    // key isn't buying streaming in this binary.
    #[cfg(not(feature = "deepgram-stt"))]
    if is_set("DEEPGRAM_API_KEY") {
        tracing::warn!(
            "DEEPGRAM_API_KEY is set but this daemon was not built with the \
             `deepgram-stt` feature: live calls will use batch xAI STT, not \
             Deepgram streaming. Rebuild with --features deepgram-stt to enable it."
        );
    }

    // The live audio loop only exists under `livekit-tap`. Without it, STT/TTS
    // keys configure nothing in this binary — note it once so the keys aren't
    // assumed to be active.
    #[cfg(not(feature = "livekit-tap"))]
    if is_set("DEEPGRAM_API_KEY") || is_set("XAI_API_KEY") {
        tracing::warn!(
            "STT/TTS keys (DEEPGRAM_API_KEY/XAI_API_KEY) are set but this daemon \
             was not built with the `livekit-tap` feature: the live call audio \
             loop is not compiled in, so these keys are inert. Lifecycle + demo \
             call paths still work. Rebuild with --features livekit-tap to activate."
        );
    }
}

/// Emit a one-time readiness summary so the operator knows, at boot, which
/// optional features are live vs unconfigured. Read-only over the environment;
/// logs at INFO. Mirrors the exact entitlement each feature actually checks at
/// runtime so the summary can't lie.
fn log_readiness_summary() {
    // Telephony (outbound dialing): full SIP set required.
    let telephony_ready = is_set("LIVEKIT_URL")
        && is_set("LIVEKIT_API_KEY")
        && is_set("LIVEKIT_API_SECRET")
        && is_set("OCEAN_CALL_OUTBOUND_TRUNK")
        && is_set("OCEAN_CALL_CALLER_NUMBER");

    // In-room voice/video token minting: just the LiveKit auth triple.
    let livekit_tokens_ready =
        is_set("LIVEKIT_URL") && is_set("LIVEKIT_API_KEY") && is_set("LIVEKIT_API_SECRET");

    // STT: streaming (Deepgram, feature-gated) preferred; batch (xAI) fallback.
    let stt = if cfg!(feature = "deepgram-stt") && is_set("DEEPGRAM_API_KEY") {
        "deepgram-streaming"
    } else if is_set("XAI_API_KEY") {
        "xai-batch"
    } else {
        "none"
    };

    // TTS: the active lane speaks via xAI (feature-gated) using XAI_API_KEY.
    let tts = if is_set("XAI_API_KEY") { "xai" } else { "none" };

    // Provider credentials present. Names mirror
    // `ocean_providers::ProviderId::credential_env_names` — kept as a local table
    // so the daemon needn't take a new dependency just to print a boot summary.
    let providers = configured_providers();

    // Longhouse mode (validated above; default `embedded` when unset).
    let longhouse_mode = opt("OCEAN_LONGHOUSE_MODE")
        .and_then(|m| LonghouseMode::parse(&m).map(|_| m))
        .unwrap_or_else(|| "embedded (default)".to_string());

    tracing::info!(
        telephony = if telephony_ready { "ready" } else { "unconfigured" },
        livekit_voice = if livekit_tokens_ready { "ready" } else { "unconfigured" },
        stt,
        tts,
        providers = if providers.is_empty() { "<none with creds>".to_string() } else { providers.join(",") },
        longhouse = %longhouse_mode,
        live_audio_loop = cfg!(feature = "livekit-tap"),
        "startup config validated; optional-feature readiness summary"
    );
}

/// Which LLM providers have a credential present in the environment. Static
/// table of the canonical env var names per provider (mirrors
/// `ocean_providers::ProviderId::credential_env_names`); used only for the boot
/// summary, so it intentionally avoids pulling `ocean-providers` as a dep.
fn configured_providers() -> Vec<&'static str> {
    const PROVIDER_CREDS: &[(&str, &[&str])] = &[
        (
            "anthropic",
            &["OCEAN_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"],
        ),
        ("openai", &["OCEAN_OPENAI_API_KEY", "OPENAI_API_KEY"]),
        (
            "google",
            &["OCEAN_GOOGLE_API_KEY", "GOOGLE_API_KEY", "GEMINI_API_KEY"],
        ),
        ("deepseek", &["OCEAN_DEEPSEEK_API_KEY", "DEEPSEEK_API_KEY"]),
        ("minimax", &["OCEAN_MINIMAX_API_KEY", "MINIMAX_API_KEY"]),
        (
            "kimi",
            &["OCEAN_MOONSHOT_API_KEY", "MOONSHOT_API_KEY", "KIMI_API_KEY"],
        ),
    ];

    PROVIDER_CREDS
        .iter()
        .filter(|(_, keys)| keys.iter().any(|k| is_set(k)))
        .map(|(name, _)| *name)
        .collect()
}

// ---------------------------------------------------------------------------
// Launchd supervision guard (TASK-23)
//
// Long-lived TUIs keep old autostart code in memory, so the orphan race cannot
// be closed client-side: installing a fixed binary does not upgrade running
// processes. The durable invariant lives here — if launchd owns the
// `dev.risingtides.ocean-daemon` job and this process was not launched by it,
// refuse to bind and exit cleanly. `OCEAN_UNSUPERVISED=1` is the explicit
// escape hatch for deliberately supervisor-free runs (dev boxes, tests).

/// The launchd job label whose ownership of the port this guard enforces.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const LAUNCHD_LABEL: &str = "dev.risingtides.ocean-daemon";

/// Probe result for the launchd job: `None` = no job registered;
/// `Some(None)` = job registered but not currently running;
/// `Some(Some(pid))` = job running as `pid`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub type LaunchdJob = Option<Option<u32>>;

/// What the supervision guard decided at boot.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub enum SupervisionVerdict {
    /// Bind normally.
    Proceed,
    /// Launchd owns the job and this process is not it: do not bind, exit 0.
    RefuseForeignSupervisor {
        /// The supervised daemon's pid, when the job is currently running.
        job_pid: Option<u32>,
    },
}

/// Pure decision: refuse whenever the launchd job exists and this process is
/// not the one it launched — including a registered-but-idle job, which
/// launchd may kickstart onto the port at any moment.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn supervision_verdict(
    job: LaunchdJob,
    self_pid: u32,
    unsupervised_override: bool,
) -> SupervisionVerdict {
    if unsupervised_override {
        return SupervisionVerdict::Proceed;
    }
    match job {
        None => SupervisionVerdict::Proceed,
        Some(Some(pid)) if pid == self_pid => SupervisionVerdict::Proceed,
        Some(job_pid) => SupervisionVerdict::RefuseForeignSupervisor { job_pid },
    }
}

/// Extract `pid = N` from `launchctl print` output. Absent for idle jobs.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn parse_launchd_pid(output: &str) -> Option<u32> {
    output.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("pid = ")
            .and_then(|value| value.trim().parse::<u32>().ok())
    })
}

/// Ask launchd about the job in the current user's GUI domain. Non-macOS
/// platforms have no launchd; the guard is a no-op there.
#[cfg(target_os = "macos")]
pub fn probe_launchd_job(label: &str) -> LaunchdJob {
    let uid = std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse::<u32>()
                .ok()
        });
    // No uid → cannot address the GUI domain → fail open like a probe failure.
    let uid = uid?;
    let output = std::process::Command::new("launchctl")
        .arg("print")
        .arg(format!("gui/{uid}/{label}"))
        .output();
    match output {
        Ok(out) if out.status.success() => {
            Some(parse_launchd_pid(&String::from_utf8_lossy(&out.stdout)))
        }
        // Nonzero exit = no such job; probe failure = fail open (a broken
        // launchctl must not brick a dev box; the installer still owns prod).
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The process environment is global, so these tests must not run
    // concurrently against overlapping vars. A shared mutex serializes them and
    // each test clears the vars it touches on the way in and out.
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// All env vars the validator inspects — cleared before each test so a stray
    /// value from the real environment (or a previous test) can't leak in.
    const ALL_VARS: &[&str] = &[
        "OCEAN_BIND",
        "LIVEKIT_URL",
        "LIVEKIT_API_KEY",
        "LIVEKIT_API_SECRET",
        "OCEAN_CALL_OUTBOUND_TRUNK",
        "OCEAN_CALL_CALLER_NUMBER",
        "OCEAN_LIVEKIT_PUBLISH_TOKEN",
        "OCEAN_RETRY_MAX_ATTEMPTS",
        "OCEAN_RETRY_BASE_BACKOFF_MS",
        "OCEAN_RETRY_MAX_BACKOFF_MS",
        "OCEAN_CALL_SUMMARY_TIMEOUT_MS",
        "OCEAN_CALL_ANSWER_TIMEOUT_MS",
        "OCEAN_LONGHOUSE_TIMEOUT_MS",
        "OCEAN_SHUTDOWN_GRACE_SECS",
        "OCEAN_LONGHOUSE_MODE",
        "OCEAN_LONGHOUSE_URL",
        "OCEAN_ASSISTANTS_DIR",
        "DEEPGRAM_API_KEY",
        "XAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
    ];

    fn clear_all() {
        for v in ALL_VARS {
            env::remove_var(v);
        }
    }

    #[test]
    fn empty_env_boots_clean() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        // No optional feature configured → must still validate Ok.
        assert!(validate_startup_config().is_ok());
        clear_all();
    }

    #[test]
    fn malformed_retry_max_attempts_is_fatal() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        env::set_var("OCEAN_RETRY_MAX_ATTEMPTS", "lots");
        let err = validate_startup_config().unwrap_err().to_string();
        assert!(err.contains("OCEAN_RETRY_MAX_ATTEMPTS"), "got: {err}");
        clear_all();
    }

    #[test]
    fn non_e164_caller_number_is_fatal() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        env::set_var("OCEAN_CALL_CALLER_NUMBER", "not-a-number");
        let err = validate_startup_config().unwrap_err().to_string();
        assert!(err.contains("OCEAN_CALL_CALLER_NUMBER"), "got: {err}");
        clear_all();
    }

    #[test]
    fn valid_e164_caller_number_is_ok() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        env::set_var("OCEAN_CALL_CALLER_NUMBER", "+17035081859");
        // Caller number alone is a partial-telephony WARN, never a hard error.
        assert!(validate_startup_config().is_ok());
        clear_all();
    }

    #[test]
    fn bad_livekit_url_is_fatal() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        env::set_var("LIVEKIT_URL", "ftp://nope");
        let err = validate_startup_config().unwrap_err().to_string();
        assert!(err.contains("LIVEKIT_URL"), "got: {err}");
        clear_all();
    }

    #[test]
    fn wss_livekit_url_is_ok() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        env::set_var("LIVEKIT_URL", "wss://my-project.livekit.cloud");
        assert!(validate_startup_config().is_ok());
        clear_all();
    }

    #[test]
    fn bad_bind_is_fatal() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        env::set_var("OCEAN_BIND", "not-a-socket");
        let err = validate_startup_config().unwrap_err().to_string();
        assert!(err.contains("OCEAN_BIND"), "got: {err}");
        clear_all();
    }

    #[test]
    fn bad_longhouse_mode_is_fatal() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        env::set_var("OCEAN_LONGHOUSE_MODE", "sideways");
        let err = validate_startup_config().unwrap_err().to_string();
        assert!(err.contains("OCEAN_LONGHOUSE_MODE"), "got: {err}");
        clear_all();
    }

    #[test]
    fn valid_longhouse_mode_is_ok() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        env::set_var("OCEAN_LONGHOUSE_MODE", "local");
        assert!(validate_startup_config().is_ok());
        clear_all();
    }

    #[test]
    fn partial_livekit_is_warn_not_fatal() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        // Key without secret: a partial-feature WARN, never a hard error.
        env::set_var("LIVEKIT_API_KEY", "lk_key");
        assert!(validate_startup_config().is_ok());
        clear_all();
    }

    #[test]
    fn aggregated_errors_name_every_bad_var() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        env::set_var("OCEAN_BIND", "bad");
        env::set_var("OCEAN_RETRY_MAX_ATTEMPTS", "nan");
        let err = validate_startup_config().unwrap_err().to_string();
        assert!(err.contains("OCEAN_BIND"), "got: {err}");
        assert!(err.contains("OCEAN_RETRY_MAX_ATTEMPTS"), "got: {err}");
        clear_all();
    }

    // ---- TASK-23 supervision guard -------------------------------------

    #[test]
    fn supervision_verdict_is_exhaustive_over_job_states() {
        use SupervisionVerdict::*;
        // No job registered: any process may bind.
        assert_eq!(supervision_verdict(None, 42, false), Proceed);
        // Job running as this process: launchd launched us.
        assert_eq!(supervision_verdict(Some(Some(42)), 42, false), Proceed);
        // Job running as someone else: refuse.
        assert_eq!(
            supervision_verdict(Some(Some(7)), 42, false),
            RefuseForeignSupervisor { job_pid: Some(7) }
        );
        // Job registered but idle: launchd may kickstart any moment — refuse.
        assert_eq!(
            supervision_verdict(Some(None), 42, false),
            RefuseForeignSupervisor { job_pid: None }
        );
        // Explicit override always proceeds, even against a running job.
        assert_eq!(supervision_verdict(Some(Some(7)), 42, true), Proceed);
        assert_eq!(supervision_verdict(Some(None), 42, true), Proceed);
    }

    #[test]
    fn launchd_pid_parses_from_print_output_only_when_present() {
        let running = "\tstate = running\n\tpid = 16021\n\tprogram = /x\n";
        assert_eq!(parse_launchd_pid(running), Some(16021));
        let idle = "\tstate = not running\n\tprogram = /x\n";
        assert_eq!(parse_launchd_pid(idle), None);
        // A pid-like token inside another key must not match.
        let tricky = "\tspawn pid = abc\n\txpid = 99\n";
        assert_eq!(parse_launchd_pid(tricky), None);
    }
}
