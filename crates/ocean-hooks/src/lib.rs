//! Plugin-agnostic runtime hooks for Ocean.
//!
//! This crate owns the low-level hook primitive: run configured subprocesses at
//! lifecycle events, pass a stable JSON payload on stdin, and interpret JSON stdout
//! as a decision. It is deliberately independent from any one plugin or MCP server.
//! Stitchpad, memory systems, audit loggers, notification bridges, and future Ocean
//! plugins should all be able to adapt to this same contract.

use std::{collections::BTreeMap, path::Path, time::Duration};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{io::AsyncWriteExt, process::Command, time};

/// Default timeout for one hook command, matching the common local-agent hook
/// default used by adjacent runtimes.
pub const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 15;

/// A lifecycle event that hooks can subscribe to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum HookEvent {
    /// Fired after an agent turn finishes and before the turn is considered fully
    /// released back to the operator. A blocking decision can hold the foreground
    /// flow until the runtime re-enters.
    Stop,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::Stop => "Stop",
        }
    }
}

/// TOML-facing hook section, grouped by lifecycle event.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct HooksConfig {
    #[serde(default, rename = "Stop")]
    pub stop: Vec<HookCommand>,
}

impl HooksConfig {
    pub fn commands_for(&self, event: HookEvent) -> &[HookCommand] {
        match event {
            HookEvent::Stop => &self.stop,
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        for hook in &self.stop {
            hook.validate(HookEvent::Stop)?;
        }
        Ok(())
    }

    pub fn count_for(&self, event: HookEvent) -> usize {
        self.commands_for(event).len()
    }
}

/// One subprocess hook command.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct HookCommand {
    /// Executable or script path to run.
    pub command: String,
    /// Arguments passed to `command`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Per-command timeout in seconds.
    #[serde(default = "default_hook_timeout_secs")]
    pub timeout_secs: u64,
    /// Disabled entries stay in config but are skipped.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl HookCommand {
    pub fn validate(&self, event: HookEvent) -> anyhow::Result<()> {
        if self.command.trim().is_empty() {
            anyhow::bail!(
                "invalid [[hooks.{}]]: command cannot be empty",
                event.as_str()
            );
        }
        Ok(())
    }
}

fn default_hook_timeout_secs() -> u64 {
    DEFAULT_HOOK_TIMEOUT_SECS
}

fn default_true() -> bool {
    true
}

/// Runtime context supplied to every hook.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HookContext {
    pub cwd: String,
    pub session_id: String,
    /// Extra plugin/runtime-neutral context. Hook adapters may ignore keys they do
    /// not understand; Ocean core does not attach plugin-specific semantics here.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Value>,
}

impl HookContext {
    pub fn new(cwd: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            cwd: cwd.into(),
            session_id: session_id.into(),
            metadata: BTreeMap::new(),
        }
    }
}

/// JSON payload written to hook stdin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HookPayload {
    pub hook_event_name: String,
    pub cwd: String,
    pub session_id: String,
    /// Compatibility field used by local-agent Stop hook adapters to avoid
    /// recursively re-entering their own stop flow.
    pub stop_hook_active: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Value>,
}

impl HookPayload {
    pub fn new(event: HookEvent, context: &HookContext, stop_hook_active: bool) -> Self {
        Self {
            hook_event_name: event.as_str().to_string(),
            cwd: context.cwd.clone(),
            session_id: context.session_id.clone(),
            stop_hook_active,
            metadata: context.metadata.clone(),
        }
    }
}

/// Decision returned by a hook command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    Continue,
    Block { reason: String },
}

/// Complete result of running a hook chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRunResult {
    pub outcome: HookOutcome,
    /// Fail-open warnings from skipped/failed hook commands. Values never include
    /// secret env values from Ocean; stderr is included only from the child itself.
    pub warnings: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawHookOutput {
    decision: Option<String>,
    reason: Option<String>,
}

/// Run every enabled command configured for one hook event.
///
/// Failure posture is fail-open: spawn errors, non-zero exit, invalid JSON, and
/// timeouts are returned as warnings and do not fail the turn. The first command
/// that prints `{"decision":"block","reason":"..."}` wins and stops the chain.
pub async fn run_hooks(
    config: &HooksConfig,
    event: HookEvent,
    context: &HookContext,
    stop_hook_active: bool,
) -> HookRunResult {
    let mut warnings = Vec::new();
    for hook in config.commands_for(event).iter().filter(|h| h.enabled) {
        match run_one_hook(hook, event, context, stop_hook_active).await {
            Ok(HookOutcome::Continue) => {}
            Ok(block @ HookOutcome::Block { .. }) => {
                return HookRunResult {
                    outcome: block,
                    warnings,
                };
            }
            Err(e) => warnings.push(format!("hook `{}` failed: {e}", hook.command)),
        }
    }
    HookRunResult {
        outcome: HookOutcome::Continue,
        warnings,
    }
}

async fn run_one_hook(
    hook: &HookCommand,
    event: HookEvent,
    context: &HookContext,
    stop_hook_active: bool,
) -> anyhow::Result<HookOutcome> {
    let payload = HookPayload::new(event, context, stop_hook_active);
    let stdin = serde_json::to_vec(&payload).context("serialize hook payload")?;

    let mut child = Command::new(&hook.command)
        .args(&hook.args)
        .current_dir(Path::new(&context.cwd))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn hook `{}`", hook.command))?;

    if let Some(mut child_stdin) = child.stdin.take() {
        child_stdin
            .write_all(&stdin)
            .await
            .context("write hook stdin")?;
    }

    let timeout = Duration::from_secs(hook.timeout_secs.max(1));
    let output = match time::timeout(timeout, child.wait_with_output()).await {
        Ok(out) => out.context("wait for hook")?,
        Err(_) => return Err(anyhow::anyhow!("timed out after {}s", timeout.as_secs())),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "exited with status {}{}",
            output.status,
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        return Ok(HookOutcome::Continue);
    }

    let parsed: RawHookOutput = serde_json::from_str(&stdout).with_context(|| {
        format!(
            "parse hook stdout as JSON (first 200 chars: {:?})",
            stdout.chars().take(200).collect::<String>()
        )
    })?;

    if parsed.decision.as_deref() == Some("block") {
        let reason = parsed.reason.unwrap_or_default();
        if !reason.trim().is_empty() {
            return Ok(HookOutcome::Block { reason });
        }
    }
    Ok(HookOutcome::Continue)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stop_hook_config() {
        #[derive(Deserialize)]
        struct Wrapper {
            hooks: HooksConfig,
        }

        let parsed: Wrapper = toml_like_parse(
            r#"
            [hooks]
            [[hooks.Stop]]
            command = "node"
            args = ["adapter.mjs"]
            "#,
        );

        assert_eq!(parsed.hooks.stop.len(), 1);
        assert_eq!(parsed.hooks.stop[0].command, "node");
        assert_eq!(parsed.hooks.stop[0].timeout_secs, DEFAULT_HOOK_TIMEOUT_SECS);
        assert!(parsed.hooks.stop[0].enabled);
    }

    #[tokio::test]
    async fn block_decision_stops_chain() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("block.sh");
        std::fs::write(
            &blocker,
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"decision\":\"block\",\"reason\":\"mention pending\"}'\n",
        )
        .unwrap();
        make_executable(&blocker);

        let config = HooksConfig {
            stop: vec![HookCommand {
                command: blocker.to_string_lossy().to_string(),
                args: vec![],
                timeout_secs: 5,
                enabled: true,
            }],
        };
        let result = run_hooks(
            &config,
            HookEvent::Stop,
            &HookContext::new(dir.path().to_string_lossy(), "ses_test"),
            false,
        )
        .await;

        assert_eq!(
            result.outcome,
            HookOutcome::Block {
                reason: "mention pending".to_string()
            }
        );
        assert!(result.warnings.is_empty());
    }

    #[tokio::test]
    async fn hook_failures_are_warnings_not_errors() {
        let dir = tempfile::tempdir().unwrap();
        let config = HooksConfig {
            stop: vec![HookCommand {
                command: dir.path().join("missing").to_string_lossy().to_string(),
                args: vec![],
                timeout_secs: 1,
                enabled: true,
            }],
        };

        let result = run_hooks(
            &config,
            HookEvent::Stop,
            &HookContext::new(dir.path().to_string_lossy(), "ses_test"),
            false,
        )
        .await;

        assert_eq!(result.outcome, HookOutcome::Continue);
        assert_eq!(result.warnings.len(), 1);
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[cfg(not(unix))]
    fn make_executable(_path: &Path) {}

    fn toml_like_parse<T: serde::de::DeserializeOwned>(text: &str) -> T {
        toml::from_str(text).unwrap()
    }
}
