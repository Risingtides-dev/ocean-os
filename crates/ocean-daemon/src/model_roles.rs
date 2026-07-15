use std::{collections::HashMap, path::Path};

use ocean_agent_sdk::AdvisorControl;

// Model roles (oh-my-pi-style indirection) loaded once from `ocean.toml`'s
// `[roles]` table. A malformed config here is non-fatal for roles — the
// daemon already validated + loaded the same file for MCP/hooks at runtime
// construction, so a parse error would have surfaced there; if it somehow
// doesn't parse now we log and fall back to an empty table (roles + advisor
// simply off), never blocking startup.
pub(super) fn load_model_roles(config_dir: &Path) -> HashMap<String, String> {
    match ocean_agent::DaemonConfig::load(config_dir) {
        Ok(cfg) => {
            if !cfg.roles.is_empty() {
                tracing::info!(
                    role_count = cfg.roles.len(),
                    advisor = cfg.advisor_model().is_some(),
                    "loaded model roles from ocean.toml [roles]"
                );
            }
            cfg.roles
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to load [roles] from ocean.toml; roles disabled");
            HashMap::new()
        }
    }
}

/// Decide which model alias the post-turn advisor runs on, given the per-turn
/// override and the global `[roles]` table. Precedence:
///
/// - override `enabled:false` → `None` (suppress even a configured global role)
/// - override `enabled:true`  → the override's `model`, else the global
///   `advisor` role; `None` when neither exists (nothing to run on)
/// - no override → the global `advisor` role (today's behavior)
///
/// Pure so the precedence is unit-testable without a full turn.
pub(super) fn resolve_advisor_alias(
    override_ctl: Option<&AdvisorControl>,
    roles: &HashMap<String, String>,
) -> Option<String> {
    match override_ctl {
        Some(ctl) if !ctl.enabled => None,
        Some(ctl) => ctl
            .model
            .clone()
            .filter(|m| !m.trim().is_empty())
            .or_else(|| roles.get("advisor").cloned()),
        None => roles.get("advisor").cloned(),
    }
}

/// Resolve the EFFECTIVE per-turn model from an explicit `model_id`, an optional
/// symbolic `role`, and the loaded `[roles]` table. Pure so the precedence rules
/// are unit-testable without a full turn:
///
/// - An explicit `model_id` ALWAYS wins (role is ignored entirely).
/// - Otherwise a known `role` resolves to its configured alias.
/// - An unknown role (or no role) yields `None` → the runtime's global model.
///
/// The `bool` is `true` when a role was given but did NOT resolve — the caller
/// logs a warning for that case (a typo'd role silently using the global model
/// would be surprising).
pub(super) fn resolve_effective_model_id(
    model_id: Option<&str>,
    role: Option<&str>,
    roles: &HashMap<String, String>,
) -> (Option<String>, bool) {
    match (model_id, role) {
        (Some(m), _) => (Some(m.to_string()), false),
        (None, Some(r)) => match roles.get(r) {
            Some(alias) => (Some(alias.clone()), false),
            None => (None, true),
        },
        (None, None) => (None, false),
    }
}
