//! The Ocean OKF profile — the concept-type registry.
//!
//! This is the **standard**. For every kind of knowledge artifact Ocean already
//! ships, it declares what a concept of that kind must carry, the typed links it
//! may assert, and how legacy on-disk shapes map onto the canonical fields. It
//! is typed Rust — compiled and tested next to the primitives it governs — so it
//! cannot drift out of sync the way a spec doc in `docs/` would. Changing the
//! standard is a reviewed code change, never a silent edit to prose nobody runs.
//!
//! It is grounded entirely in primitives that exist today:
//!
//! | concept | backs (shipped primitive) |
//! |---------|---------------------------|
//! | `handoff` | [`crate::Handoff`] (this crate) |
//! | `claim`   | [`crate::Claim`] (this crate) |
//! | `memory`  | `ocean-memory`'s `Memory` |
//! | `agent`   | `ocean-agent` folder-as-agent `AgentDef` |
//! | `event`   | the repo-root `events.md` devlog ledger |
//! | `note`    | the OCEAN Obsidian vault notes |
//!
//! Enforcement is by **diagnostic, never rejection**. A concept that misses a
//! field is a work-item for a normalizer, not an error to bounce — the producers
//! are agents emitting imperfect artifacts, so the standard heals rather than
//! gates. The only thing [`validate`] can ever do is *describe* what is missing.

use std::collections::BTreeMap;

/// Already-parsed frontmatter: field name → value. Format-agnostic on purpose —
/// the rules are identical whether the source was YAML (vault, memory files) or
/// TOML (handoffs). Nested values (e.g. memory's `metadata.type`) are
/// addressable with [`get_path`].
pub type Fields = BTreeMap<String, serde_json::Value>;

/// One rule in the profile: the OKF face of a real Ocean primitive.
#[derive(Debug, Clone, Copy)]
pub struct ConceptType {
    /// Canonical `type` value: `"handoff"`, `"claim"`, `"memory"`, `"agent"`,
    /// `"event"`, `"note"`. This is the routing discriminator; it is the caller
    /// (the loader) that decides a concept's type — often structurally, from the
    /// source — so it is passed to [`validate`] rather than read from a field.
    pub name: &'static str,
    /// The shipped primitive this is the OKF face of. Documentation, not enforced.
    pub backs: &'static str,
    /// Frontmatter fields (beyond `type`) a conforming concept must carry.
    pub required: &'static [&'static str],
    /// Fields that should be present but whose absence is only advisory.
    pub recommended: &'static [&'static str],
    /// `legacy.path => canonical` migrations a normalizer applies. A required
    /// field that is absent but reachable through one of these is reported as
    /// *needs-migration* (a warning), not *missing* (an error).
    pub maps_from: &'static [(&'static str, &'static str)],
    /// The typed-link predicates a concept of this type may assert. OKF leaves
    /// link meaning to prose; Ocean types it so the graph is queryable.
    pub links: &'static [&'static str],
}

/// THE STANDARD. The registry of every concept type the runtime recognizes,
/// grounded in shipped primitives. Static + compiled — adding or altering a type
/// is a reviewed, tested code change, never a silent doc edit.
pub const PROFILE: &[ConceptType] = &[
    ConceptType {
        name: "handoff",
        backs: "ocean-context::Handoff (claim.rs) — on-disk TOML frontmatter (store.rs)",
        required: &["session_id", "repo", "branch", "commit_anchor"],
        // scope_ring/written_at are engine-owned and absent from legacy handoffs
        // (see store.rs old-format test) — recommended, never a hard error.
        recommended: &["parent_session", "scope_ring", "written_at"],
        maps_from: &[],
        links: &["continues", "supersedes"],
    },
    ConceptType {
        name: "claim",
        backs: "ocean-context::Claim (claim.rs)",
        required: &["text", "status"],
        // commit_sha lives under provenance and a draft claim lacks it until
        // anchored — advisory, so reverifiability is surfaced without bouncing WIP.
        recommended: &["confidence", "knowledge_tier", "commit_sha"],
        maps_from: &[("ticket", "tickets")], // mirrors Provenance's serde alias
        links: &["anchored_to", "reverifies", "borrowed_from"],
    },
    ConceptType {
        name: "memory",
        // Validated artifact is the on-disk YAML memory file (the metadata-nested
        // shape); it is INGESTED into ocean-memory::Memory. `kind` is authored as
        // `metadata.type`, hence the mapping below.
        backs: "ocean-memory::Memory (on-disk YAML frontmatter → SQLite row)",
        required: &["kind"],
        recommended: &["scope", "owner", "trust", "title"],
        maps_from: &[
            ("metadata.node_type", "type"),
            ("metadata.type", "kind"),
            ("name", "title"),
        ],
        links: &["relates_to", "supersedes"],
    },
    ConceptType {
        name: "agent",
        backs: "ocean-agent folder-as-agent AgentDef (agentdir.rs)",
        required: &[], // identity is the folder name; a bare instructions.md is valid
        recommended: &["description", "model", "tools", "capabilities"],
        maps_from: &[],
        links: &["has_skill", "has_subagent", "uses_tool", "uses_capability"],
    },
    ConceptType {
        name: "event",
        backs: "events.md devlog ledger entry",
        required: &["time", "agent"],
        // worktree is required-iff-not-main (unexpressible in a flat list) →
        // recommended. The devlog's own `type:` is the event CATEGORY; mapped so
        // it doesn't collide with the OKF concept type (assigned by the loader).
        recommended: &["worktree", "area", "category"],
        maps_from: &[("type", "category")],
        links: &["about", "supersedes"],
    },
    ConceptType {
        name: "note",
        backs: "OCEAN Obsidian vault note",
        required: &["title"],
        recommended: &["tags", "created", "updated"],
        maps_from: &[],
        links: &["relates_to"],
    },
];

/// Look up a concept type by its `type` name.
pub fn concept_type(name: &str) -> Option<&'static ConceptType> {
    PROFILE.iter().find(|c| c.name == name)
}

/// Severity of a [`Diagnostic`]. Neither blocks: even `Error` means "needs work
/// before a consumer can rely on it", never "reject the write".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warn,
    Error,
}

/// A non-fatal conformance finding — a work-item carried with the concept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
}

/// Resolve a possibly-dotted path (`"metadata.type"`) against parsed fields.
pub fn get_path<'a>(fields: &'a Fields, path: &str) -> Option<&'a serde_json::Value> {
    let mut parts = path.split('.');
    let mut cur = fields.get(parts.next()?)?;
    for p in parts {
        cur = cur.get(p)?;
    }
    Some(cur)
}

/// Whether a value counts as present: a non-empty string, a non-empty array, or
/// any non-null scalar/object. Empty strings and `null` are treated as absent.
fn is_present(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => false,
        serde_json::Value::String(s) => !s.trim().is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        _ => true,
    }
}

/// Validate parsed `fields` declared as concept `type_name` against [`PROFILE`].
///
/// Emits diagnostics; never rejects. An unknown type is tolerated (a warning,
/// per OKF). A required field that is absent but reachable through a `maps_from`
/// source is reported as `needs-migration` (warn), not `missing-required`
/// (error) — that distinction is exactly what drives the normalizer: heal the
/// migratable ones, surface the genuinely-absent ones.
pub fn validate(type_name: &str, fields: &Fields) -> Vec<Diagnostic> {
    let Some(ct) = concept_type(type_name) else {
        return vec![Diagnostic {
            severity: Severity::Warn,
            code: "unknown-type",
            message: format!("`{type_name}` is not a registered concept type"),
        }];
    };

    let mut out = Vec::new();
    for &field in ct.required {
        if fields.get(field).is_some_and(is_present) {
            continue;
        }
        // Absent directly — is it reachable via a migration source?
        let via = ct
            .maps_from
            .iter()
            .find(|(_, canonical)| *canonical == field)
            .and_then(|(legacy, _)| get_path(fields, legacy).filter(|v| is_present(v)).map(|_| *legacy));
        out.push(match via {
            Some(legacy) => Diagnostic {
                severity: Severity::Warn,
                code: "needs-migration",
                message: format!(
                    "required `{field}` is absent but available at `{legacy}`; a migration pass will lift it"
                ),
            },
            None => Diagnostic {
                severity: Severity::Error,
                code: "missing-required",
                message: format!("`{type_name}` requires `{field}`"),
            },
        });
    }
    out
}

/// Apply a concept type's `maps_from` migrations to a parsed field-map, then
/// validate. For each `(legacy, canonical)`: if `legacy` (possibly dotted)
/// resolves to a present value and `canonical` is not already set, lift it up to
/// `canonical`. Returns the normalized fields plus the post-migration
/// diagnostics — so a `needs-migration` warning becomes a conforming concept.
///
/// This is the **format-independent core of the loader**. Whatever parser
/// produced `fields` — YAML for vault/memory, TOML for handoffs, the devlog
/// block for events — normalization and validation are identical; the
/// per-source parsers are thin adapters layered on top. An unknown type is a
/// no-op rename plus the tolerated `unknown-type` warning.
pub fn normalize(type_name: &str, mut fields: Fields) -> (Fields, Vec<Diagnostic>) {
    if let Some(ct) = concept_type(type_name) {
        for &(legacy, canonical) in ct.maps_from {
            // Never clobber a canonical the author already set.
            if fields.get(canonical).is_some_and(is_present) {
                continue;
            }
            // Resolve + clone before mutating, so the read borrow ends first.
            if let Some(v) = get_path(&fields, legacy).filter(|v| is_present(v)).cloned() {
                fields.insert(canonical.to_string(), v);
            }
        }
    }
    let diags = validate(type_name, &fields);
    (fields, diags)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields(v: serde_json::Value) -> Fields {
        v.as_object().expect("test fields must be a JSON object").clone().into_iter().collect()
    }

    #[test]
    fn shipped_handoff_already_conforms() {
        // A handoff's real TOML frontmatter already carries every required field,
        // so the standard ratifies an existing primitive — zero diagnostics.
        let f = fields(json!({
            "session_id": "s-1", "repo": "ocean-os", "branch": "main", "commit_anchor": "abc1234"
        }));
        assert!(validate("handoff", &f).is_empty());
    }

    #[test]
    fn real_memory_frontmatter_reports_needs_migration_not_error() {
        // The real `metadata`-nested memory shape: `kind` is absent at the top
        // level but reachable at `metadata.type`. The standard must heal, not bounce.
        let f = fields(json!({
            "name": "campaign-hub-real-data",
            "description": "verified real data",
            "metadata": { "node_type": "memory", "type": "project" }
        }));
        let d = validate("memory", &f);
        assert!(d.iter().all(|x| x.severity == Severity::Warn), "no hard errors: {d:?}");
        assert!(d.iter().any(|x| x.code == "needs-migration"));
    }

    #[test]
    fn memory_missing_everything_is_a_hard_error() {
        let f = fields(json!({ "description": "a fact with no kind anywhere" }));
        let d = validate("memory", &f);
        assert!(d.iter().any(|x| x.code == "missing-required" && x.severity == Severity::Error));
    }

    #[test]
    fn event_type_collision_maps_to_category() {
        // A devlog entry: its `type:` is the CATEGORY; the concept type is "event"
        // (assigned by the loader). With time+agent present it's clean, and the
        // inner `type` is understood as `category` via maps_from.
        let f = fields(json!({ "time": "[3:36am] [06-26-26]", "agent": "claude", "type": "feature-request" }));
        assert!(validate("event", &f).is_empty());
        assert_eq!(concept_type("event").unwrap().maps_from, &[("type", "category")]);
    }

    #[test]
    fn unknown_type_is_tolerated_as_a_warning() {
        let d = validate("not-a-real-type", &fields(json!({})));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].code, "unknown-type");
        assert_eq!(d[0].severity, Severity::Warn);
    }

    #[test]
    fn dotted_path_resolves_nested_fields() {
        let f = fields(json!({ "metadata": { "type": "project" } }));
        assert_eq!(get_path(&f, "metadata.type").and_then(|v| v.as_str()), Some("project"));
        assert!(get_path(&f, "metadata.missing").is_none());
        assert!(get_path(&f, "absent.path").is_none());
    }

    #[test]
    fn normalize_lifts_real_memory_to_conformant() {
        // The real metadata-nested memory shape, fed through the normalizer:
        // metadata.{node_type,type} and name are lifted to canonical top-level
        // type/kind/title, healing the needs-migration warning into conformance.
        let raw = fields(json!({
            "name": "campaign-hub-real-data",
            "metadata": { "node_type": "memory", "type": "project" }
        }));
        let (norm, diags) = normalize("memory", raw);
        assert_eq!(norm.get("kind").and_then(|v| v.as_str()), Some("project"));
        assert_eq!(norm.get("type").and_then(|v| v.as_str()), Some("memory"));
        assert_eq!(norm.get("title").and_then(|v| v.as_str()), Some("campaign-hub-real-data"));
        assert!(diags.iter().all(|d| d.severity != Severity::Error), "healed: {diags:?}");
    }

    #[test]
    fn normalize_never_clobbers_an_author_set_canonical() {
        let raw = fields(json!({
            "type": "memory", "kind": "user",
            "metadata": { "node_type": "agent", "type": "project" }
        }));
        let (norm, _) = normalize("memory", raw);
        assert_eq!(norm.get("type").and_then(|v| v.as_str()), Some("memory"));
        assert_eq!(norm.get("kind").and_then(|v| v.as_str()), Some("user"));
    }
}
