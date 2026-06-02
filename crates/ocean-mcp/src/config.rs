//! Configuration for MCP servers — the `[[mcp.server]]` array folded into
//! Ocean's daemon config TOML.
//!
//! Secrets are referenced **by environment-variable name only**. The manifest
//! never holds a secret value; at spawn time, `ocean-mcp` resolves each name
//! from the daemon's process environment (which the daemon loads from
//! `~/.config/ocean-rs/tools.env`) and injects it into the child. A missing
//! env var is a per-server warning, not a manifest error — the server simply
//! doesn't start.

use serde::{Deserialize, Serialize};

/// Which transport to use for a server. Only `stdio` is implemented today;
/// the enum exists so a manifest can already name `http` and we add the
/// transport behind it without a config migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransportKind {
    #[default]
    Stdio,
    /// Reserved — not yet implemented; configuring it is a startup warning.
    Http,
}

/// One MCP server entry from `[[mcp.server]]`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpServerConfig {
    /// Stable short name; namespaces this server's tools as `mcp__<name>__<tool>`
    /// and identifies it in logs. Required, unique.
    pub name: String,

    /// Transport kind. Defaults to stdio.
    #[serde(default)]
    pub transport: McpTransportKind,

    /// Executable to launch (stdio transport). e.g. `npx`.
    #[serde(default)]
    pub command: Option<String>,

    /// Arguments to the executable. e.g. `["-y", "@modelcontextprotocol/server-brave-search"]`.
    #[serde(default)]
    pub args: Vec<String>,

    /// Names of environment variables this server needs (secrets). Resolved by
    /// name from the daemon's process env at spawn and injected into the child.
    /// NEVER put secret *values* here — names only.
    #[serde(default)]
    pub env: Vec<String>,

    /// Set false to keep an entry in the manifest but skip launching it.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl McpServerConfig {
    /// Resolve this server's declared env-var names against `lookup` (normally
    /// `std::env::var`). Returns the `(name, value)` pairs to inject and the
    /// list of names that were missing. Values are never logged by this code.
    pub fn resolve_env<F>(&self, mut lookup: F) -> (Vec<(String, String)>, Vec<String>)
    where
        F: FnMut(&str) -> Option<String>,
    {
        let mut resolved = Vec::new();
        let mut missing = Vec::new();
        for name in &self.env {
            match lookup(name) {
                Some(v) if !v.is_empty() => resolved.push((name.clone(), v)),
                _ => missing.push(name.clone()),
            }
        }
        (resolved, missing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mcp_server_section_from_toml() {
        // The shape as it appears folded into the daemon config.
        let toml = r#"
            [[mcp.server]]
            name = "brave"
            command = "npx"
            args = ["-y", "@modelcontextprotocol/server-brave-search"]
            env = ["BRAVE_API_KEY"]

            [[mcp.server]]
            name = "linear"
            command = "npx"
            args = ["-y", "@tacticlaunch/mcp-linear"]
            env = ["LINEAR_API_KEY"]
            enabled = false
        "#;

        #[derive(Deserialize)]
        struct Wrapper {
            mcp: Mcp,
        }
        #[derive(Deserialize)]
        struct Mcp {
            server: Vec<McpServerConfig>,
        }

        let parsed: Wrapper = toml::from_str(toml).unwrap();
        let servers = parsed.mcp.server;
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "brave");
        assert_eq!(servers[0].command.as_deref(), Some("npx"));
        assert_eq!(servers[0].transport, McpTransportKind::Stdio);
        assert!(servers[0].enabled, "enabled defaults to true");
        assert_eq!(servers[0].env, vec!["BRAVE_API_KEY".to_string()]);
        assert!(!servers[1].enabled, "explicit enabled=false respected");
    }

    #[test]
    fn resolve_env_separates_present_from_missing_and_never_inlines() {
        let cfg = McpServerConfig {
            name: "x".into(),
            transport: McpTransportKind::Stdio,
            command: Some("npx".into()),
            args: vec![],
            env: vec![
                "PRESENT_KEY".into(),
                "MISSING_KEY".into(),
                "EMPTY_KEY".into(),
            ],
            enabled: true,
        };
        let (resolved, missing) = cfg.resolve_env(|name| match name {
            "PRESENT_KEY" => Some("secret-value".into()),
            "EMPTY_KEY" => Some(String::new()),
            _ => None,
        });
        assert_eq!(
            resolved,
            vec![("PRESENT_KEY".into(), "secret-value".into())]
        );
        // Empty value is treated as missing (a blank secret is not a secret).
        assert!(missing.contains(&"MISSING_KEY".to_string()));
        assert!(missing.contains(&"EMPTY_KEY".to_string()));
    }
}
