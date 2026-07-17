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

/// Complete daemon-side model selection for an ordinary agent turn.
///
/// Precedence is explicit model > resolved role > named-agent model > session
/// pin > global. A role that was actually named but did not resolve is a
/// deliberate global fallback: lower-priority agent/session values must not
/// silently replace it. `model_id` is the hard per-turn override passed to the
/// runtime; `agent_model` retains the runtime's fail-soft folder-agent path.
/// `announced_model` is the exact value for `TurnStarted` before any later
/// provider-readiness reroute (which has its own `ModelRerouted` event).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TurnModelResolution {
    pub(super) model_id: Option<String>,
    pub(super) agent_model: Option<String>,
    pub(super) announced_model: String,
    pub(super) role_unresolved: bool,
}

pub(super) fn resolve_turn_model(
    model_id: Option<&str>,
    role: Option<&str>,
    roles: &HashMap<String, String>,
    agent_model: Option<&str>,
    session_model: Option<&str>,
    global_model: &str,
) -> TurnModelResolution {
    let (request_model, role_unresolved) = resolve_effective_model_id(model_id, role, roles);

    if role_unresolved {
        return TurnModelResolution {
            model_id: None,
            agent_model: None,
            announced_model: global_model.to_string(),
            role_unresolved: true,
        };
    }

    if let Some(model) = request_model {
        if model.trim().is_empty() {
            return TurnModelResolution {
                model_id: None,
                agent_model: None,
                announced_model: global_model.to_string(),
                role_unresolved: false,
            };
        }
        return TurnModelResolution {
            announced_model: model.clone(),
            model_id: Some(model),
            agent_model: None,
            role_unresolved: false,
        };
    }

    if let Some(model) = agent_model {
        if model.trim().is_empty() {
            return TurnModelResolution {
                model_id: None,
                agent_model: None,
                announced_model: global_model.to_string(),
                role_unresolved: false,
            };
        }
        return TurnModelResolution {
            model_id: None,
            agent_model: Some(model.to_string()),
            announced_model: model.to_string(),
            role_unresolved: false,
        };
    }

    if let Some(model) = session_model {
        return TurnModelResolution {
            announced_model: model.to_string(),
            model_id: Some(model.to_string()),
            agent_model: None,
            role_unresolved: false,
        };
    }

    TurnModelResolution {
        model_id: None,
        agent_model: None,
        announced_model: global_model.to_string(),
        role_unresolved: false,
    }
}
