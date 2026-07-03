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
    /// fast    = "deepseek/deepseek-chat"
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
    fn malformed_config_is_an_error() {
        let dir = std::env::temp_dir().join(format!("ocean-cfg-bad-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ocean.toml"), "this is = not valid = toml [[[").unwrap();
        assert!(DaemonConfig::load(&dir).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
