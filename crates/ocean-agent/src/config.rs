//! Daemon configuration loaded from `<config_dir>/ocean.toml`.
//!
//! This is the first real daemon-level config layer (until now the daemon read
//! only `OCEAN_BIND` from the environment). Its first content is the
//! `[[mcp.server]]` array — the list of MCP servers whose tools plug into the
//! agent via the capability registry.
//!
//! The file is **optional**: if it's absent or empty, the daemon runs with
//! built-in tools only (zero behaviour change from before this layer existed).
//! A present-but-malformed file is a hard error at startup — better to fail
//! loudly than silently ignore a misconfigured server.
//!
//! Secrets are never stored here. Each server lists the *names* of the env vars
//! it needs (`env = ["LINEAR_API_KEY"]`); the values are resolved from the
//! daemon's process environment (loaded from `tools.env`) at spawn time.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;
use ocean_hooks::HooksConfig;
use ocean_mcp::McpServerConfig;
use serde::{Deserialize, Serialize};

/// Top-level daemon config. Everything is optional so the file can grow one
/// section at a time.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct DaemonConfig {
    #[serde(default)]
    pub mcp: McpSection,
    #[serde(default)]
    pub hooks: HooksConfig,
    /// Named model *roles* (oh-my-pi-style indirection): a `[roles]` table
    /// mapping a symbolic role name to a concrete model alias. E.g.
    ///
    /// ```toml
    /// [roles]
    /// fast    = "deepseek/deepseek-v4-flash"
    /// deep    = "anthropic/claude-opus-4"
    /// advisor = "anthropic/claude-sonnet-4"
    /// ```
    ///
    /// A turn carrying `role = "fast"` (and no explicit `model_id`) is driven
    /// with the mapped alias. The special `advisor` role, when present, also
    /// activates the post-turn advisor observer. Absent/empty `[roles]` →
    /// behavior is 100% unchanged and the advisor is off (zero cost).
    #[serde(default)]
    pub roles: HashMap<String, String>,
    /// The `[offshore]` table: dispatch agent work to a remote Ocean daemon
    /// (reachable over the tailnet) inside per-job git worktrees. Absent →
    /// the offshore tool family is not registered (zero behavior change).
    #[serde(default)]
    pub offshore: Option<OffshoreSection>,
}

impl DaemonConfig {
    /// Resolve a named role to its configured model alias. `None` when the role
    /// isn't present in `[roles]` (caller falls back to default model behavior).
    pub fn role_model(&self, role: &str) -> Option<&str> {
        self.roles.get(role).map(String::as_str)
    }

    /// The configured `advisor` role's model alias, if any. `Some` iff an
    /// `advisor` entry is present in `[roles]` — the single switch that turns the
    /// post-turn advisor observer on.
    pub fn advisor_model(&self) -> Option<&str> {
        self.role_model("advisor")
    }
}

/// The `[mcp]` table, holding the `[[mcp.server]]` array.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct McpSection {
    #[serde(default)]
    pub server: Vec<McpServerConfig>,
}

/// The `[offshore]` table: the remote Ocean daemon the offshore tool family
/// dispatches to, and the ssh path used to provision its git worktrees.
///
/// ```toml
/// [offshore]
/// remote_url = "http://100.90.205.60:4780"
/// ssh_host   = "smathdaddy@100.90.205.60"
/// # ssh_bin           = "/usr/bin/ssh"   (default)
/// # remote_root       = "offshore"       (default; dir under the remote $HOME)
/// # turn_timeout_secs = 900              (default; dispatch is synchronous)
/// # enabled           = true             (default)
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OffshoreSection {
    /// Base URL of the remote daemon, e.g. `http://100.90.205.60:4780`.
    pub remote_url: String,
    /// ssh destination of the remote box, e.g. `smathdaddy@100.90.205.60`.
    pub ssh_host: String,
    /// ssh binary. `None` → `/usr/bin/ssh`.
    #[serde(default)]
    pub ssh_bin: Option<String>,
    /// Directory under the remote `$HOME` holding `mirrors/` and `jobs/`.
    /// `None` → `offshore`.
    #[serde(default)]
    pub remote_root: Option<String>,
    /// Deadline (seconds) for one synchronous dispatch turn — the remote
    /// daemon's `POST /v1/agent/turns` responds only when the turn finishes.
    /// `None` → 900.
    #[serde(default)]
    pub turn_timeout_secs: Option<u64>,
    /// Kill switch: a present-but-disabled table registers nothing.
    #[serde(default = "default_offshore_enabled")]
    pub enabled: bool,
}

fn default_offshore_enabled() -> bool {
    true
}

impl OffshoreSection {
    /// The ssh binary to run, default applied.
    pub fn ssh_bin(&self) -> &str {
        self.ssh_bin.as_deref().unwrap_or("/usr/bin/ssh")
    }

    /// The remote root under `$HOME`, default applied.
    pub fn remote_root(&self) -> &str {
        self.remote_root.as_deref().unwrap_or("offshore")
    }

    /// The synchronous-dispatch deadline in seconds, default applied.
    pub fn turn_timeout_secs(&self) -> u64 {
        self.turn_timeout_secs.unwrap_or(900)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if !(self.remote_url.starts_with("http://") || self.remote_url.starts_with("https://")) {
            anyhow::bail!(
                "[offshore] remote_url must be an http(s) URL, got `{}`",
                self.remote_url
            );
        }
        if self.ssh_host.trim().is_empty() {
            anyhow::bail!("[offshore] ssh_host must not be empty");
        }
        Ok(())
    }
}

impl DaemonConfig {
    /// Load `<config_dir>/ocean.toml`. Missing file → default (built-ins only).
    /// Present-but-unparseable → error.
    pub fn load(config_dir: &Path) -> anyhow::Result<Self> {
        let path = config_dir.join("ocean.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "no ocean.toml; built-in tools only");
                return Ok(Self::default());
            }
            Err(e) => {
                return Err(e).with_context(|| format!("read {}", path.display()));
            }
        };
        let cfg: DaemonConfig =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        cfg.validate()
            .with_context(|| format!("invalid {}", path.display()))?;
        tracing::info!(
            path = %path.display(),
            mcp_servers = cfg.mcp.server.len(),
            stop_hooks = cfg.hooks.count_for(ocean_hooks::HookEvent::Stop),
            offshore = cfg.offshore.as_ref().is_some_and(|o| o.enabled),
            "loaded daemon config"
        );
        Ok(cfg)
    }

    /// Validate cross-field invariants the type system can't express. Run at
    /// load so a misconfiguration surfaces here — where the operator can act on
    /// it — instead of silently as a confusing per-tool "duplicate" warning at
    /// runtime, or an ambiguous namespaced tool name.
    fn validate(&self) -> anyhow::Result<()> {
        let mut seen = std::collections::HashSet::new();
        for s in &self.mcp.server {
            // Server names namespace tools as `mcp__<name>__<tool>`. Two servers
            // with the same name would collide and silently drop one's tools.
            if !seen.insert(s.name.as_str()) {
                anyhow::bail!("duplicate [[mcp.server]] name `{}`", s.name);
            }
            // The `__` separator and non-identifier chars would make the
            // namespaced name ambiguous (e.g. a server `a__b` tool `c` vs server
            // `a` tool `b__c` both render to `mcp__a__b__c`). Restrict names to a
            // safe charset.
            if s.name.is_empty()
                || s.name.contains("__")
                || !s
                    .name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-')
            {
                anyhow::bail!(
                    "invalid [[mcp.server]] name `{}`: use only ASCII letters, digits, and `-` (no `__`)",
                    s.name
                );
            }
        }
        if let Some(offshore) = &self.offshore {
            offshore.validate()?;
        }
        self.hooks.validate()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_config_is_default_builtins_only() {
        let dir = std::env::temp_dir().join(format!("ocean-cfg-none-{}", uuid::Uuid::new_v4()));
        let cfg = DaemonConfig::load(&dir).unwrap();
        assert!(cfg.mcp.server.is_empty());
    }

    #[test]
    fn loads_mcp_servers_from_ocean_toml() {
        let dir = std::env::temp_dir().join(format!("ocean-cfg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("ocean.toml"),
            r#"
            [[mcp.server]]
            name = "brave"
            command = "npx"
            args = ["-y", "@modelcontextprotocol/server-brave-search"]
            env = ["BRAVE_API_KEY"]

            [[hooks.Stop]]
            command = "/tmp/stop-hook.sh"
            timeout_secs = 9
            "#,
        )
        .unwrap();
        let cfg = DaemonConfig::load(&dir).unwrap();
        assert_eq!(cfg.mcp.server.len(), 1);
        assert_eq!(cfg.mcp.server[0].name, "brave");
        assert_eq!(cfg.mcp.server[0].env, vec!["BRAVE_API_KEY".to_string()]);
        assert_eq!(cfg.hooks.stop.len(), 1);
        assert_eq!(cfg.hooks.stop[0].command, "/tmp/stop-hook.sh");
        assert_eq!(cfg.hooks.stop[0].timeout_secs, 9);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn roles_table_resolves_known_and_unknown() {
        let dir = std::env::temp_dir().join(format!("ocean-cfg-roles-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("ocean.toml"),
            r#"
            [roles]
            fast = "deepseek/deepseek-chat"
            advisor = "anthropic/claude-sonnet-4"
            "#,
        )
        .unwrap();
        let cfg = DaemonConfig::load(&dir).unwrap();
        // Known role → its alias.
        assert_eq!(cfg.role_model("fast"), Some("deepseek/deepseek-chat"));
        // Unknown role → None (caller falls back to default model).
        assert_eq!(cfg.role_model("nope"), None);
        // The advisor switch is derived from the same table.
        assert_eq!(cfg.advisor_model(), Some("anthropic/claude-sonnet-4"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn no_roles_table_means_no_advisor() {
        let cfg = DaemonConfig::default();
        assert_eq!(cfg.role_model("fast"), None);
        assert_eq!(cfg.advisor_model(), None);
    }

    #[test]
    fn loads_offshore_section_with_defaults_applied() {
        let dir = std::env::temp_dir().join(format!("ocean-cfg-off-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("ocean.toml"),
            r#"
            [offshore]
            remote_url = "http://100.90.205.60:4780"
            ssh_host = "smathdaddy@100.90.205.60"
            "#,
        )
        .unwrap();
        let cfg = DaemonConfig::load(&dir).unwrap();
        let offshore = cfg.offshore.expect("offshore section present");
        assert_eq!(offshore.remote_url, "http://100.90.205.60:4780");
        assert_eq!(offshore.ssh_host, "smathdaddy@100.90.205.60");
        // Defaults, applied through the accessors.
        assert_eq!(offshore.ssh_bin(), "/usr/bin/ssh");
        assert_eq!(offshore.remote_root(), "offshore");
        assert_eq!(offshore.turn_timeout_secs(), 900);
        assert!(offshore.enabled, "present table defaults to enabled");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn loads_offshore_section_with_every_field_set() {
        let dir = std::env::temp_dir().join(format!("ocean-cfg-off2-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("ocean.toml"),
            r#"
            [offshore]
            remote_url = "https://tide-net:4780"
            ssh_host = "me@tide-net"
            ssh_bin = "/opt/homebrew/bin/ssh"
            remote_root = "jobs-root"
            turn_timeout_secs = 1200
            enabled = false
            "#,
        )
        .unwrap();
        let cfg = DaemonConfig::load(&dir).unwrap();
        let offshore = cfg.offshore.expect("offshore section present");
        assert_eq!(offshore.ssh_bin(), "/opt/homebrew/bin/ssh");
        assert_eq!(offshore.remote_root(), "jobs-root");
        assert_eq!(offshore.turn_timeout_secs(), 1200);
        assert!(!offshore.enabled);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn absent_offshore_table_is_none() {
        let cfg = DaemonConfig::default();
        assert!(cfg.offshore.is_none());
    }

    #[test]
    fn offshore_with_a_non_http_url_is_an_error() {
        let dir = std::env::temp_dir().join(format!("ocean-cfg-off3-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("ocean.toml"),
            r#"
            [offshore]
            remote_url = "tide-net:4780"
            ssh_host = "me@tide-net"
            "#,
        )
        .unwrap();
        let err = DaemonConfig::load(&dir).unwrap_err();
        assert!(format!("{err:#}").contains("remote_url"), "{err:#}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn offshore_with_an_empty_ssh_host_is_an_error() {
        let dir = std::env::temp_dir().join(format!("ocean-cfg-off4-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("ocean.toml"),
            r#"
            [offshore]
            remote_url = "http://tide-net:4780"
            ssh_host = "  "
            "#,
        )
        .unwrap();
        let err = DaemonConfig::load(&dir).unwrap_err();
        assert!(format!("{err:#}").contains("ssh_host"), "{err:#}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn malformed_config_is_an_error() {
        let dir = std::env::temp_dir().join(format!("ocean-cfg-bad-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ocean.toml"), "this is = not valid = toml [[[").unwrap();
        assert!(DaemonConfig::load(&dir).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
