//! `plugin.toml` — the plugin manifest and its parser.
//!
//! A manifest declares the plugin's identity (`name`, `version`), the `entry`
//! executable the [`SubprocessPlugin`](crate::SubprocessPlugin) launches, and the
//! tools the plugin advertises. The runtime reads the manifest *before* launching
//! anything, so discovery and permission decisions can happen without spawning a
//! process.
//!
//! Example `plugin.toml`:
//!
//! ```toml
//! name = "hello"
//! version = "0.1.0"
//! entry = "bin/hello-plugin"
//!
//! [[tool]]
//! name = "greet"
//! description = "Return a greeting"
//! input_schema = { type = "object", properties = { who = { type = "string" } } }
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::plugin::PluginTool;

/// Errors from reading or parsing a manifest.
#[derive(Debug)]
pub enum ManifestError {
    /// The manifest file could not be read from disk.
    Io { path: String, reason: String },
    /// The manifest was not valid TOML, or did not match the expected shape.
    Parse(String),
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestError::Io { path, reason } => {
                write!(f, "failed to read plugin manifest `{path}`: {reason}")
            }
            ManifestError::Parse(r) => write!(f, "invalid plugin manifest: {r}"),
        }
    }
}

impl std::error::Error for ManifestError {}

/// One tool entry in the manifest's `[[tool]]` array. `input_schema` is optional
/// in the TOML and defaults to an empty object schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDecl {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema for the tool arguments. Accepts either `input_schema` (snake,
    /// TOML-idiomatic) or `inputSchema` (to match the JSON wire spelling).
    #[serde(default, alias = "inputSchema")]
    pub input_schema: Option<Value>,
}

impl ToolDecl {
    /// Lift this declaration into the runtime-facing [`PluginTool`], defaulting an
    /// absent schema to an empty object (`{}`).
    pub fn into_plugin_tool(self) -> PluginTool {
        PluginTool {
            name: self.name,
            description: self.description,
            input_schema: self.input_schema.unwrap_or_else(|| serde_json::json!({})),
        }
    }
}

/// The parsed `plugin.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Plugin name. Stable identifier; namespaces the plugin's tools.
    pub name: String,
    /// Plugin version string (semver by convention, not enforced here).
    pub version: String,
    /// Path to the entry executable, relative to the manifest's directory (or
    /// absolute). [`SubprocessPlugin`](crate::SubprocessPlugin) spawns this.
    pub entry: String,
    /// Tools the plugin advertises. The manifest is the source of truth for
    /// discovery; a launched [`SubprocessPlugin`](crate::SubprocessPlugin) can
    /// also confirm them live via `tools/list`.
    #[serde(default, rename = "tool")]
    pub tools: Vec<ToolDecl>,
}

impl PluginManifest {
    /// Parse a manifest from a TOML string.
    pub fn parse(toml_src: &str) -> Result<Self, ManifestError> {
        toml::from_str(toml_src).map_err(|e| ManifestError::Parse(e.to_string()))
    }

    /// Read and parse a manifest from a file path.
    pub fn from_path(path: impl AsRef<std::path::Path>) -> Result<Self, ManifestError> {
        let path = path.as_ref();
        let src = std::fs::read_to_string(path).map_err(|e| ManifestError::Io {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        Self::parse(&src)
    }

    /// The declared tools as runtime-facing [`PluginTool`]s.
    pub fn plugin_tools(&self) -> Vec<PluginTool> {
        self.tools
            .iter()
            .cloned()
            .map(ToolDecl::into_plugin_tool)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_manifest() {
        let src = r#"
            name = "hello"
            version = "0.2.1"
            entry = "bin/hello-plugin"

            [[tool]]
            name = "greet"
            description = "Return a greeting"
            input_schema = { type = "object", properties = { who = { type = "string" } } }

            [[tool]]
            name = "ping"
        "#;
        let m = PluginManifest::parse(src).expect("manifest parses");
        assert_eq!(m.name, "hello");
        assert_eq!(m.version, "0.2.1");
        assert_eq!(m.entry, "bin/hello-plugin");
        assert_eq!(m.tools.len(), 2);

        let tools = m.plugin_tools();
        assert_eq!(tools[0].name, "greet");
        assert_eq!(tools[0].description.as_deref(), Some("Return a greeting"));
        assert_eq!(tools[0].input_schema["type"], "object");

        // A tool with no declared schema defaults to an empty object schema.
        assert_eq!(tools[1].name, "ping");
        assert_eq!(tools[1].input_schema, serde_json::json!({}));
    }

    #[test]
    fn accepts_camelcase_input_schema_alias() {
        let src = r#"
            name = "p"
            version = "0"
            entry = "p"
            [[tool]]
            name = "t"
            inputSchema = { type = "object" }
        "#;
        let m = PluginManifest::parse(src).unwrap();
        assert_eq!(
            m.tools[0].input_schema,
            Some(serde_json::json!({"type":"object"}))
        );
    }

    #[test]
    fn missing_required_field_is_a_parse_error() {
        // No `entry`.
        let src = r#"
            name = "p"
            version = "0"
        "#;
        let err = PluginManifest::parse(src).unwrap_err();
        assert!(matches!(err, ManifestError::Parse(_)), "got {err:?}");
    }
}
