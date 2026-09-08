//! Rooms Phase 2 Stage 2b — the room profile routes and credential-slot status.
//!
//! See `docs/specs/2026-09-08-ocean-rooms-phase2-room-profile-and-contributed-folders-manifest.md`
//! §2.1, §6, §7.
//!
//! # What this module owns
//!
//! 1. **Validation** of a profile write: total, typed, and writes nothing on
//!    refusal. A `remote` that parses as a filesystem path, an unknown resolver
//!    scheme, a slot name that is not an identifier, a duplicate alias — each
//!    is its own 400 code so the Surface can point at the field.
//! 2. **The operator gate** on `PUT`, identical to Phase 1: header-only
//!    credential, 503 when unavailable, replay-safe `decision_id`.
//! 3. **Credential-slot resolution** ([`resolve_slots`]): walk each slot's
//!    resolvers on THIS node and report a status. The value, when one exists,
//!    is never read into this module — `env:` checks presence, `oauth:` checks
//!    that a provider block exists in the daemon's own `auth.json` and whether
//!    its `expires` is in the past. What the daemon serves is `resolved`,
//!    `missing`, `expired`, or `resolver_not_open`; never the credential.
//!
//! # Phase 2b restrictions (manifest §2.1, ruling §11.1)
//!
//! - Every `resource_id`, `default_resource_id`, or `agent_defaults` value
//!   must name a grant in this room that is not revoked (`resource_not_found`
//!   otherwise): a dangling reference would be authority minted by a later
//!   grant without a decision. Before 2c landed these were `phase_not_open`.
//! - `keychain:` is a valid scheme that resolves to `resolver_not_open`.
//! - Tool `installed` status is REPORTED on read, not enforced on write. The
//!   write-time refusal the manifest names lands with the resource-aware tools
//!   in 2d, when there is a single inventory to check against; recorded as a
//!   deviation in the manifest.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path as FsPath;

use axum::{
    extract::{rejection::JsonRejection, Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use chrono::Utc;
use ocean_core::RoomKey;
use ocean_store::{
    CredentialSlot, PutRoomProfileInput, RepoRef, RoomProfile, RoomStoreError, ToolRef, ToolRefKind,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::AppState;
use crate::persistent_rooms::{publish_room_wake, with_rooms};
use crate::room_agent_authority::{
    decision_digest, operator, validate_decision_id, validate_member_id, ApiError,
};

const MAX_REPOS: usize = 32;
const MAX_TOOLS: usize = 32;
const MAX_SLOTS: usize = 32;
const MAX_RESOLVERS: usize = 8;
const MAX_ALLOWED_TOOLS: usize = 128;
const MAX_ALIAS_CHARS: usize = 64;
const MAX_REMOTE_CHARS: usize = 512;
const MAX_PURPOSE_CHARS: usize = 200;
const MAX_NAME_CHARS: usize = 128;
const MAX_AGENT_DEFAULTS: usize = 64;

// ── Request body ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PutProfileBody {
    decision_id: String,
    #[serde(default)]
    repos: Vec<RepoRefBody>,
    #[serde(default)]
    tools: Vec<ToolRefBody>,
    #[serde(default)]
    credential_slots: Vec<CredentialSlotBody>,
    #[serde(default)]
    default_resource_id: Option<String>,
    #[serde(default)]
    agent_defaults: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoRefBody {
    alias: String,
    remote: String,
    #[serde(default)]
    default_branch: Option<String>,
    #[serde(default)]
    resource_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolRefBody {
    kind: String,
    name: String,
    #[serde(default)]
    allowed: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialSlotBody {
    name: String,
    #[serde(default)]
    purpose: String,
    #[serde(default)]
    required: bool,
    resolvers: Vec<String>,
}

/// The canonical content an operator approves. Hashed into `request_digest`;
/// field order is fixed by this struct so the digest is stable.
#[derive(Serialize)]
struct ProfileDecisionDigestInput<'a> {
    room_id: &'a str,
    repos: &'a [RepoRef],
    tools: &'a [ToolRef],
    credential_slots: &'a [CredentialSlot],
    default_resource_id: &'a Option<String>,
    agent_defaults: &'a BTreeMap<String, String>,
}

// ── Validation ────────────────────────────────────────────────────────────────

/// A bounded, single-line, control-free string. Same character policy as
/// `ocean_core::bounded_prose`: a value that can carry a newline or a bracket
/// can forge a row in anything that renders the profile as text.
fn bounded_text(raw: &str, max_chars: usize, code: &'static str) -> Result<String, ApiError> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.chars().count() > max_chars
        || trimmed
            .chars()
            .any(|c| c.is_control() || c == '[' || c == ']')
    {
        return Err(ApiError::bad_request(code));
    }
    Ok(trimmed.to_string())
}

/// `[A-Za-z][A-Za-z0-9_.-]*`, bounded. Aliases, tool names, slot names.
fn identifier(raw: &str, max_chars: usize, code: &'static str) -> Result<String, ApiError> {
    let trimmed = raw.trim();
    let mut chars = trimmed.chars();
    let ok_first = chars.next().is_some_and(|c| c.is_ascii_alphabetic());
    let ok_rest = chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if !ok_first || !ok_rest || trimmed.chars().count() > max_chars {
        return Err(ApiError::bad_request(code));
    }
    Ok(trimmed.to_string())
}

/// A repository remote is a URL or an scp-style `user@host:path`. It is never
/// a filesystem path: a room profile is portable intent, and a local path in
/// it would be a private fact projected as if it were shared.
fn repo_remote(raw: &str) -> Result<String, ApiError> {
    let remote = bounded_text(raw, MAX_REMOTE_CHARS, "invalid_repo_remote")?;
    if remote.chars().any(char::is_whitespace) {
        return Err(ApiError::bad_request("invalid_repo_remote"));
    }
    let looks_like_path = remote.starts_with('/')
        || remote.starts_with('.')
        || remote.starts_with('~')
        || remote.starts_with("file:");
    if looks_like_path {
        return Err(ApiError::bad_request("invalid_repo_remote"));
    }
    let has_url_scheme = remote.split_once("://").is_some_and(|(scheme, rest)| {
        matches!(scheme, "https" | "http" | "ssh" | "git") && !rest.is_empty()
    });
    // scp-style: `git@github.com:org/repo.git` — an `@`, then a host, then a
    // `:` that is NOT introducing a port-looking or URL-looking remainder.
    let scp_style = remote
        .split_once('@')
        .and_then(|(user, rest)| rest.split_once(':').map(|(host, path)| (user, host, path)))
        .is_some_and(|(user, host, path)| {
            !user.is_empty() && !host.is_empty() && !path.is_empty() && !host.contains('/')
        });
    if !has_url_scheme && !scp_style {
        return Err(ApiError::bad_request("invalid_repo_remote"));
    }
    Ok(remote)
}

/// The resolver grammar. Accepts `oauth:<provider>`, `env:<NAME>`, and
/// `keychain:<service>/<account>`; anything else is `invalid_resolver`.
fn resolver(raw: &str) -> Result<String, ApiError> {
    let trimmed = raw.trim();
    let Some((scheme, target)) = trimmed.split_once(':') else {
        return Err(ApiError::bad_request("invalid_resolver"));
    };
    let ok = match scheme {
        "oauth" => {
            !target.is_empty()
                && target.len() <= 64
                && target
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        }
        "env" => {
            !target.is_empty()
                && target.len() <= 128
                && target
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && target
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        "keychain" => target.split_once('/').is_some_and(|(service, account)| {
            !service.is_empty()
                && !account.is_empty()
                && target.len() <= 200
                && !target.chars().any(|c| c.is_control() || c.is_whitespace())
        }),
        _ => false,
    };
    if !ok {
        return Err(ApiError::bad_request("invalid_resolver"));
    }
    Ok(trimmed.to_string())
}

struct ValidatedProfile {
    repos: Vec<RepoRef>,
    tools: Vec<ToolRef>,
    credential_slots: Vec<CredentialSlot>,
    default_resource_id: Option<String>,
    agent_defaults: BTreeMap<String, String>,
}

fn validate(body: PutProfileBody) -> Result<(String, ValidatedProfile), ApiError> {
    let decision_id = validate_decision_id(&body.decision_id)?;

    // Resource references are shape-checked here and existence-checked by the
    // route against live grants (2c) — validation stays pure.
    let default_resource_id = body
        .default_resource_id
        .as_deref()
        .map(|id| identifier(id, MAX_NAME_CHARS, "invalid_resource_id"))
        .transpose()?;
    let mut agent_defaults = BTreeMap::new();
    for (agent, resource_id) in &body.agent_defaults {
        let agent = validate_member_id(agent, "invalid_agent_member_id")?;
        if agent.is_empty() {
            return Err(ApiError::bad_request("invalid_agent_member_id"));
        }
        let resource_id = identifier(resource_id, MAX_NAME_CHARS, "invalid_resource_id")?;
        agent_defaults.insert(agent, resource_id);
    }
    if agent_defaults.len() > MAX_AGENT_DEFAULTS {
        return Err(ApiError::bad_request("profile_too_large"));
    }

    if body.repos.len() > MAX_REPOS
        || body.tools.len() > MAX_TOOLS
        || body.credential_slots.len() > MAX_SLOTS
    {
        return Err(ApiError::bad_request("profile_too_large"));
    }

    let mut aliases = BTreeSet::new();
    let mut repos = Vec::with_capacity(body.repos.len());
    for repo in body.repos {
        let alias = identifier(&repo.alias, MAX_ALIAS_CHARS, "invalid_repo_alias")?;
        if !aliases.insert(alias.clone()) {
            return Err(ApiError::bad_request("duplicate_repo_alias"));
        }
        let remote = repo_remote(&repo.remote)?;
        let default_branch = repo
            .default_branch
            .as_deref()
            .map(|b| bounded_text(b, MAX_NAME_CHARS, "invalid_default_branch"))
            .transpose()?;
        let resource_id = repo
            .resource_id
            .as_deref()
            .map(|id| identifier(id, MAX_NAME_CHARS, "invalid_resource_id"))
            .transpose()?;
        repos.push(RepoRef {
            alias,
            remote,
            default_branch,
            resource_id,
        });
    }

    let mut tool_keys = BTreeSet::new();
    let mut tools = Vec::with_capacity(body.tools.len());
    for tool in body.tools {
        let kind = ToolRefKind::parse(tool.kind.trim())
            .ok_or_else(|| ApiError::bad_request("invalid_tool_kind"))?;
        let name = identifier(&tool.name, MAX_NAME_CHARS, "invalid_tool_name")?;
        if !tool_keys.insert((kind.as_str(), name.clone())) {
            return Err(ApiError::bad_request("duplicate_tool"));
        }
        if tool.allowed.len() > MAX_ALLOWED_TOOLS {
            return Err(ApiError::bad_request("profile_too_large"));
        }
        let mut allowed = tool
            .allowed
            .iter()
            .map(|a| identifier(a, MAX_NAME_CHARS, "invalid_tool_name"))
            .collect::<Result<Vec<_>, _>>()?;
        allowed.sort();
        allowed.dedup();
        tools.push(ToolRef {
            kind,
            name,
            allowed,
        });
    }

    let mut slot_names = BTreeSet::new();
    let mut credential_slots = Vec::with_capacity(body.credential_slots.len());
    for slot in body.credential_slots {
        let name = slot.name.trim();
        let ok_name = name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            && name.len() <= 64;
        if !ok_name {
            return Err(ApiError::bad_request("invalid_credential_slot_name"));
        }
        if !slot_names.insert(name.to_string()) {
            return Err(ApiError::bad_request("duplicate_credential_slot"));
        }
        let purpose = if slot.purpose.trim().is_empty() {
            String::new()
        } else {
            bounded_text(
                &slot.purpose,
                MAX_PURPOSE_CHARS,
                "invalid_credential_slot_purpose",
            )?
        };
        if slot.resolvers.is_empty() || slot.resolvers.len() > MAX_RESOLVERS {
            return Err(ApiError::bad_request("invalid_resolver"));
        }
        let resolvers = slot
            .resolvers
            .iter()
            .map(|r| resolver(r))
            .collect::<Result<Vec<_>, _>>()?;
        credential_slots.push(CredentialSlot {
            name: name.to_string(),
            purpose,
            required: slot.required,
            resolvers,
        });
    }

    Ok((
        decision_id,
        ValidatedProfile {
            repos,
            tools,
            credential_slots,
            default_resource_id,
            agent_defaults,
        },
    ))
}

// ── Credential-slot resolution ────────────────────────────────────────────────

/// A slot's status on THIS node. Never carries a value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct SlotStatus {
    pub(super) name: String,
    pub(super) required: bool,
    /// `resolved` | `missing` | `expired` | `resolver_not_open`
    pub(super) status: &'static str,
    /// The resolver that satisfied the slot, when `resolved`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) resolver: Option<String>,
}

impl SlotStatus {
    pub(super) fn blocks_admission(&self) -> bool {
        self.required && self.status != "resolved"
    }
}

/// What one resolver found. Ordered so the worst of several can be picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Probe {
    Missing,
    NotOpen,
    Expired,
    Resolved,
}

/// The daemon's own `auth.json`, read as an opaque map so this module never
/// touches a token field. Only the top-level provider keys and each block's
/// `expires` are consulted.
fn auth_blocks(config_dir: &FsPath) -> BTreeMap<String, Option<i64>> {
    let path = config_dir.join("auth.json");
    let Ok(raw) = std::fs::read(&path) else {
        return BTreeMap::new();
    };
    let Ok(Value::Object(root)) = serde_json::from_slice::<Value>(&raw) else {
        return BTreeMap::new();
    };
    root.into_iter()
        .filter_map(|(key, block)| match block {
            Value::Object(block) => {
                let expires = block.get("expires").and_then(Value::as_i64);
                Some((key, expires))
            }
            _ => None,
        })
        .collect()
}

fn probe(resolver: &str, auth: &BTreeMap<String, Option<i64>>, now_secs: i64) -> Probe {
    let Some((scheme, target)) = resolver.split_once(':') else {
        return Probe::Missing;
    };
    match scheme {
        "env" => match std::env::var_os(target) {
            Some(v) if !v.is_empty() => Probe::Resolved,
            _ => Probe::Missing,
        },
        "oauth" => match auth.get(target) {
            None => Probe::Missing,
            Some(None) => Probe::Resolved,
            Some(Some(expires)) => {
                // `oauth_refresh.rs` stores ms; older blocks stored seconds.
                let expires_secs = if *expires >= 1_000_000_000_000 {
                    expires / 1_000
                } else {
                    *expires
                };
                if expires_secs > now_secs {
                    Probe::Resolved
                } else {
                    Probe::Expired
                }
            }
        },
        "keychain" => Probe::NotOpen,
        _ => Probe::Missing,
    }
}

/// Resolve every slot's status on this node. The first resolver that resolves
/// wins; otherwise the slot reports the most informative failure it saw
/// (`expired` over `resolver_not_open` over `missing`), so an operator learns
/// "your token lapsed" before "keychain isn't wired yet".
pub(super) fn resolve_slots(slots: &[CredentialSlot], config_dir: &FsPath) -> Vec<SlotStatus> {
    let auth = auth_blocks(config_dir);
    let now_secs = Utc::now().timestamp();
    slots
        .iter()
        .map(|slot| {
            let mut worst = Probe::Missing;
            let mut resolved_by = None;
            for resolver in &slot.resolvers {
                match probe(resolver, &auth, now_secs) {
                    Probe::Resolved => {
                        resolved_by = Some(resolver.clone());
                        break;
                    }
                    other => worst = worst.max(other),
                }
            }
            let status = match (resolved_by.is_some(), worst) {
                (true, _) => "resolved",
                (false, Probe::Expired) => "expired",
                (false, Probe::NotOpen) => "resolver_not_open",
                (false, _) => "missing",
            };
            SlotStatus {
                name: slot.name.clone(),
                required: slot.required,
                status,
                resolver: resolved_by,
            }
        })
        .collect()
}

// ── Projection ────────────────────────────────────────────────────────────────

pub(super) fn profile_projection(profile: &RoomProfile) -> Value {
    json!({
        "room_id": profile.room_id,
        "revision": profile.revision.to_string(),
        "repos": profile.repos,
        "tools": profile.tools.iter().map(|t| json!({
            "kind": t.kind.as_str(),
            "name": t.name,
            "allowed": t.allowed,
            // Reported, not enforced, in 2b (see module docs).
            "installed": "unknown",
        })).collect::<Vec<_>>(),
        "credential_slots": profile.credential_slots,
        "default_resource_id": profile.default_resource_id,
        "agent_defaults": profile.agent_defaults,
        "updated_by": profile.updated_by,
        "updated_at": profile.updated_at,
        "decision_id": profile.decision_id,
    })
}

/// The `{profile, credential_slots}` pair every profile-bearing response
/// carries, so `GET`, `PUT`, and `inspect` cannot disagree on shape.
pub(super) fn profile_with_slots(
    state: &AppState,
    profile: Option<&RoomProfile>,
) -> (Value, Value) {
    match profile {
        Some(profile) => {
            let slots = resolve_slots(&profile.credential_slots, state.runtime.config_dir());
            (profile_projection(profile), json!(slots))
        }
        None => (Value::Null, json!([])),
    }
}

// ── Routes ────────────────────────────────────────────────────────────────────

pub(super) async fn room_profile_get(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> (StatusCode, Json<Value>) {
    let room = RoomKey::new(key.trim());
    if room.as_str().is_empty() {
        return ApiError::bad_request("invalid_room_key").response();
    }
    match with_rooms(&state, |store| store.room_profile(&room)) {
        Ok(profile) => {
            let (profile, slots) = profile_with_slots(&state, profile.as_ref());
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "profile": profile, "credential_slots": slots })),
            )
        }
        Err(error) => ApiError::from(error).response(),
    }
}

pub(super) async fn room_profile_put(
    State(state): State<AppState>,
    Path(key): Path<String>,
    headers: HeaderMap,
    body: Result<Json<PutProfileBody>, JsonRejection>,
) -> (StatusCode, Json<Value>) {
    let result = (|| {
        let principal = operator(&state, &headers)?;
        let Json(body) = body.map_err(|_| ApiError::bad_request("invalid_request"))?;
        let room = RoomKey::new(key.trim());
        if room.as_str().is_empty() {
            return Err(ApiError::bad_request("invalid_room_key"));
        }
        let (decision_id, validated) = validate(body)?;
        // Every referenced resource must be a live grant in THIS room (2c).
        let refs = validated
            .repos
            .iter()
            .filter_map(|r| r.resource_id.clone())
            .chain(validated.default_resource_id.clone())
            .chain(validated.agent_defaults.values().cloned())
            .collect::<Vec<_>>();
        with_rooms(&state, |store| {
            crate::room_resources::check_profile_resource_refs(store, &room, refs)
        })?;
        let digest = decision_digest(&ProfileDecisionDigestInput {
            room_id: room.as_str(),
            repos: &validated.repos,
            tools: &validated.tools,
            credential_slots: &validated.credential_slots,
            default_resource_id: &validated.default_resource_id,
            agent_defaults: &validated.agent_defaults,
        })?;
        let created_before = with_rooms(&state, |store| store.room_profile(&room))
            .map_err(ApiError::from)?
            .is_none();
        let (profile, changed, audit) = with_rooms(&state, |store| {
            store.put_room_profile(
                &room,
                PutRoomProfileInput {
                    repos: validated.repos,
                    tools: validated.tools,
                    credential_slots: validated.credential_slots,
                    default_resource_id: validated.default_resource_id,
                    agent_defaults: validated.agent_defaults,
                    updated_by: principal.id().to_string(),
                    decision_id,
                    request_digest: digest,
                },
                Utc::now(),
            )
        })
        .map_err(ApiError::from)?;
        if let Some(audit) = audit.as_ref() {
            publish_room_wake(&state, &room, audit);
        }
        let (projection, slots) = profile_with_slots(&state, Some(&profile));
        Ok((
            if created_before && changed {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            },
            json!({
                "ok": true,
                "changed": changed,
                "profile": projection,
                "credential_slots": slots,
            }),
        ))
    })();
    match result {
        Ok((status, body)) => (status, Json(body)),
        Err(error) => error.response(),
    }
}

/// Admission-time check (manifest §6): a required slot that does not resolve
/// on this node refuses the turn. Returns the first blocking slot's name so
/// the audit can say WHICH slot, never what it would have held.
pub(super) fn blocking_slot(
    store: &mut ocean_store::SqliteRoomStore,
    room: &RoomKey,
    config_dir: &FsPath,
) -> Result<Option<String>, RoomStoreError> {
    let Some(profile) = store.room_profile(room)? else {
        return Ok(None);
    };
    Ok(resolve_slots(&profile.credential_slots, config_dir)
        .into_iter()
        .find(SlotStatus::blocks_admission)
        .map(|slot| slot.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(name: &str, required: bool, resolvers: &[&str]) -> CredentialSlot {
        CredentialSlot {
            name: name.into(),
            purpose: String::new(),
            required,
            resolvers: resolvers.iter().map(|r| r.to_string()).collect(),
        }
    }

    #[test]
    fn a_remote_must_be_a_url_or_scp_target_and_never_a_path() {
        for ok in [
            "https://github.com/org/repo.git",
            "ssh://git@github.com/org/repo",
            "git@github.com:org/repo.git",
            "http://gitea.local/o/r",
        ] {
            assert_eq!(repo_remote(ok).unwrap(), ok, "{ok}");
        }
        for bad in [
            "/Users/someone/dev/repo",
            "./repo",
            "~/repo",
            "file:///tmp/repo",
            "org/repo",
            "https://",
            "git@github.com",
            "https://x.y/repo with space",
            "ftp://host/repo",
        ] {
            assert_eq!(
                repo_remote(bad).unwrap_err().code(),
                "invalid_repo_remote",
                "{bad}"
            );
        }
    }

    #[test]
    fn resolver_grammar_is_three_schemes_and_nothing_else() {
        for ok in [
            "oauth:claude-code",
            "env:GH_TOKEN",
            "env:_X1",
            "keychain:ocean/GH_TOKEN",
        ] {
            assert_eq!(resolver(ok).unwrap(), ok);
        }
        for bad in [
            "GH_TOKEN",
            "oauth:",
            "oauth:Claude Code",
            "env:1ABC",
            "env:has-dash",
            "keychain:noslash",
            "vault:secret/x",
            "keychain:a/b c",
        ] {
            assert_eq!(
                resolver(bad).unwrap_err().code(),
                "invalid_resolver",
                "{bad}"
            );
        }
    }

    #[test]
    fn env_resolver_reports_presence_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("OCEAN_TEST_SLOT_PRESENT", "not-a-real-secret");
        std::env::remove_var("OCEAN_TEST_SLOT_ABSENT");
        let statuses = resolve_slots(
            &[
                slot("A", true, &["env:OCEAN_TEST_SLOT_PRESENT"]),
                slot("B", true, &["env:OCEAN_TEST_SLOT_ABSENT"]),
                slot(
                    "C",
                    false,
                    &["env:OCEAN_TEST_SLOT_ABSENT", "env:OCEAN_TEST_SLOT_PRESENT"],
                ),
            ],
            tmp.path(),
        );
        assert_eq!(statuses[0].status, "resolved");
        assert_eq!(
            statuses[0].resolver.as_deref(),
            Some("env:OCEAN_TEST_SLOT_PRESENT")
        );
        assert!(!statuses[0].blocks_admission());
        assert_eq!(statuses[1].status, "missing");
        assert!(statuses[1].blocks_admission());
        assert_eq!(
            statuses[2].status, "resolved",
            "second resolver may satisfy"
        );
        assert!(!statuses[2].blocks_admission(), "optional never blocks");
        let rendered = serde_json::to_string(&statuses).unwrap();
        assert!(!rendered.contains("not-a-real-secret"));
        std::env::remove_var("OCEAN_TEST_SLOT_PRESENT");
    }

    #[test]
    fn oauth_resolver_reads_block_presence_and_expiry_never_the_token() {
        let tmp = tempfile::TempDir::new().unwrap();
        let future_ms = (Utc::now().timestamp() + 3_600) * 1_000;
        let past_secs = Utc::now().timestamp() - 3_600;
        std::fs::write(
            tmp.path().join("auth.json"),
            serde_json::to_vec(&json!({
                "claude-code": { "access": "sk-live-SECRET", "expires": future_ms },
                "openai-codex": { "access": "sk-old-SECRET", "expires": past_secs },
                "deepseek": { "api_key": "dk-SECRET" },
                "not-a-block": "string"
            }))
            .unwrap(),
        )
        .unwrap();
        let statuses = resolve_slots(
            &[
                slot("FRESH", true, &["oauth:claude-code"]),
                slot("LAPSED", true, &["oauth:openai-codex"]),
                slot("KEYED", true, &["oauth:deepseek"]),
                slot("NONE", true, &["oauth:kimi"]),
                slot("STRING", true, &["oauth:not-a-block"]),
                slot(
                    "FALLBACK",
                    true,
                    &["oauth:openai-codex", "oauth:claude-code"],
                ),
                slot("KC", true, &["keychain:ocean/X"]),
                slot(
                    "KC_THEN_LAPSED",
                    true,
                    &["keychain:ocean/X", "oauth:openai-codex"],
                ),
            ],
            tmp.path(),
        );
        let by_name: BTreeMap<_, _> = statuses.iter().map(|s| (s.name.as_str(), s)).collect();
        assert_eq!(by_name["FRESH"].status, "resolved");
        assert_eq!(by_name["LAPSED"].status, "expired");
        assert_eq!(
            by_name["KEYED"].status, "resolved",
            "no expiry means a static key"
        );
        assert_eq!(by_name["NONE"].status, "missing");
        assert_eq!(by_name["STRING"].status, "missing");
        assert_eq!(by_name["FALLBACK"].status, "resolved");
        assert_eq!(
            by_name["FALLBACK"].resolver.as_deref(),
            Some("oauth:claude-code")
        );
        assert_eq!(by_name["KC"].status, "resolver_not_open");
        assert_eq!(
            by_name["KC_THEN_LAPSED"].status, "expired",
            "expired outranks not-open"
        );
        let rendered = serde_json::to_string(&statuses).unwrap();
        assert!(!rendered.contains("SECRET"), "{rendered}");
    }

    #[test]
    fn validation_refuses_phase_2c_references_and_bad_shapes_with_typed_codes() {
        let base = |extra: Value| -> PutProfileBody {
            let mut v = json!({ "decision_id": uuid::Uuid::new_v4().to_string() });
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            serde_json::from_value(v).unwrap()
        };
        let code = |body: PutProfileBody| validate(body).map(|_| ()).unwrap_err().code();
        // Resource references are shape-checked here; existence is the route's
        // job against live grants.
        assert!(validate(base(json!({"default_resource_id": "res-1"}))).is_ok());
        assert!(validate(base(json!({"agent_defaults": {"builder": "res-1"}}))).is_ok());
        assert_eq!(
            code(base(json!({"default_resource_id": "[x]"}))),
            "invalid_resource_id"
        );
        assert_eq!(
            code(base(json!({"agent_defaults": {"[click](x)": "res-1"}}))),
            "invalid_agent_member_id"
        );
        assert!(validate(base(
            json!({"repos": [{"alias": "s", "remote": "https://x.y/r", "resource_id": "res-1"}]})
        ))
        .is_ok());
        assert_eq!(
            code(base(
                json!({"repos": [{"alias": "s", "remote": "/local/path"}]})
            )),
            "invalid_repo_remote"
        );
        assert_eq!(
            code(base(json!({"repos": [
                {"alias": "s", "remote": "https://x.y/a"},
                {"alias": "s", "remote": "https://x.y/b"}]}))),
            "duplicate_repo_alias"
        );
        assert_eq!(
            code(base(
                json!({"repos": [{"alias": "[x]", "remote": "https://x.y/a"}]})
            )),
            "invalid_repo_alias"
        );
        assert_eq!(
            code(base(json!({"tools": [{"kind": "shell", "name": "bash"}]}))),
            "invalid_tool_kind"
        );
        assert_eq!(
            code(base(
                json!({"tools": [{"kind": "mcp", "name": "gh"}, {"kind": "mcp", "name": "gh"}]})
            )),
            "duplicate_tool"
        );
        assert_eq!(
            code(base(
                json!({"credential_slots": [{"name": "gh_token", "resolvers": ["env:X"]}]})
            )),
            "invalid_credential_slot_name"
        );
        assert_eq!(
            code(base(
                json!({"credential_slots": [{"name": "X", "resolvers": []}]})
            )),
            "invalid_resolver"
        );
        assert_eq!(
            code(base(
                json!({"credential_slots": [{"name": "X", "resolvers": ["vault:x"]}]})
            )),
            "invalid_resolver"
        );
        assert_eq!(
            code(base(json!({"credential_slots": [
                {"name": "X", "resolvers": ["env:A"]}, {"name": "X", "resolvers": ["env:B"]}]}))),
            "duplicate_credential_slot"
        );
        assert_eq!(
            code(base(json!({"decision_id": "not-a-uuid"}))),
            "invalid_decision_id"
        );

        // And a good body canonicalizes: allowed lists sorted+deduped.
        let (_, ok) = validate(base(json!({
            "repos": [{"alias": "source", "remote": "git@github.com:o/r.git", "default_branch": "main"}],
            "tools": [{"kind": "mcp", "name": "github", "allowed": ["b", "a", "b"]}],
            "credential_slots": [{"name": "GH_TOKEN", "required": true, "resolvers": ["env:GH_TOKEN"]}]
        })))
        .unwrap();
        assert_eq!(ok.tools[0].allowed, vec!["a", "b"]);
        assert_eq!(ok.repos[0].default_branch.as_deref(), Some("main"));
    }
}
