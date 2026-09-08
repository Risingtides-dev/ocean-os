//! Rooms Phase 2 Stage 2d — resource-aware `room_list` and `room_read` tools.
//!
//! See `docs/specs/2026-09-08-ocean-rooms-phase2-room-profile-and-contributed-folders-manifest.md`
//! §4, §7, §8, and Gate 0 Decision 8 (budgets) and Decision 12 (generations).
//!
//! The shape is the one `room_history.rs` established: the daemon mints an
//! opaque, non-serializable handle from final admission evidence and a
//! daemon-owned authority; the tools carry that handle; the authority receives
//! the fixed scope on EVERY call and re-validates the binding generation and
//! the grant generation immediately before the operation (Decision 12). Model
//! arguments name a `resource_id` and a relative path and nothing else — they
//! can never select the room, the agent, the generation, or an absolute path.
//!
//! Confinement is on the canonical RESULT ([`confine`]): the relative path is
//! lexically normal (no `..`, no `.`, not absolute), joined to the root the
//! authority returned, canonicalized, and refused unless the result is under
//! that root. A symlink inside the folder that points outside is an escape,
//! whatever its name says.
//!
//! Budgets are Decision 8's ceilings, lowered where the prompt would not
//! survive them: one listing is at most 2,000 entries and never recursive;
//! one file read is refused above 8 MiB and returned in bounded chunks with a
//! `next_offset`; every operation has a 30-second deadline. Content that
//! exceeds a budget fails with a typed result — it is never silently expanded
//! into the transcript.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ocean_runtime::{capability::SharedTool, AgentTool, AgentToolResult, Concurrency};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// Decision 8: one directory listing, non-recursive.
pub const MAX_LIST_ENTRIES: usize = 2_000;
/// Decision 8: one file read.
pub const MAX_READ_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// A chunk small enough to sit in a prompt; the tool pages with `next_offset`.
pub const DEFAULT_READ_CHUNK_BYTES: usize = 64 * 1024;
pub const MAX_READ_CHUNK_BYTES: usize = 512 * 1024;
/// Decision 8: bounded file operation deadline.
pub const OPERATION_DEADLINE: Duration = Duration::from_secs(30);
const MAX_ENTRY_NAME_CHARS: usize = 255;

/// Admission evidence for the resource handle. Identical to the history
/// admission — the same binding admits both — so one implementation serves
/// both handles.
pub use crate::room_history::RoomHistoryAdmission as RoomResourceAdmission;

/// Immutable scope passed to the daemon-owned authority on every call.
#[derive(Clone, PartialEq, Eq)]
pub struct RoomResourceScope {
    room_key: String,
    agent_member_id: String,
    binding_generation: u64,
}

impl std::fmt::Debug for RoomResourceScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoomResourceScope")
            .field("room_key", &self.room_key)
            .field("agent_member_id", &self.agent_member_id)
            .field("binding_generation", &self.binding_generation)
            .finish()
    }
}

impl RoomResourceScope {
    pub fn room_key(&self) -> &str {
        &self.room_key
    }
    pub fn agent_member_id(&self) -> &str {
        &self.agent_member_id
    }
    pub fn binding_generation(&self) -> u64 {
        self.binding_generation
    }
}

/// What an operation needs. Mirrors the grant ladder without depending on the
/// store crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomResourceOp {
    List,
    Read,
}

impl RoomResourceOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Read => "read",
        }
    }
}

/// What the authority hands back for one admitted operation. `local_root`
/// stays inside the tool and is never echoed in a result.
#[derive(Debug, Clone)]
pub struct ResolvedResource {
    pub local_root: PathBuf,
    pub grant_generation: u64,
}

/// Typed refusals the authority may return. Each maps to a fixed code so the
/// model sees a stable, content-free reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomResourceError {
    /// No grant with that id in this room.
    NotFound,
    /// The grant is suspended, revoked, or expired.
    NotAvailable,
    /// The grant does not authorize this agent.
    AgentNotAuthorized,
    /// The grant's access mode does not cover the operation.
    ModeNotGranted,
    /// The binding generation the handle was minted under is no longer
    /// current, or the grant generation changed under an in-flight call.
    StaleGeneration,
    /// The operation belongs to a later phase (`write`, `execute`).
    PhaseNotOpen,
    /// The authority could not answer.
    Unavailable(String),
}

impl RoomResourceError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound => "resource_not_found",
            Self::NotAvailable => "resource_not_available",
            Self::AgentNotAuthorized => "agent_not_authorized_for_resource",
            Self::ModeNotGranted => "access_mode_not_granted",
            Self::StaleGeneration => "stale_generation",
            Self::PhaseNotOpen => "phase_not_open",
            Self::Unavailable(_) => "resource_authority_unavailable",
        }
    }
}

impl std::fmt::Display for RoomResourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(detail) => write!(f, "{}: {detail}", self.code()),
            other => f.write_str(other.code()),
        }
    }
}

impl std::error::Error for RoomResourceError {}

/// One fixed-schema audit fact (manifest §8, Decision 13). Relative paths are
/// digested, never stored; no content, no root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomResourceAuditFact {
    pub resource_id: String,
    pub grant_generation: Option<u64>,
    pub op: RoomResourceOp,
    /// SHA-256 hex of the normalized relative path (`""` for the root).
    pub relative_path_digest: String,
    pub bytes: u64,
    pub entries: u64,
    /// `ok` | a [`RoomResourceError::code`] | a local refusal code.
    pub outcome: String,
}

/// Daemon-owned authority the tools consult on every call.
#[async_trait]
pub trait RoomResourceAuthority: Send + Sync {
    /// Re-validate the scope's binding and the grant, then hand back the root
    /// for exactly one operation.
    async fn resolve(
        &self,
        scope: &RoomResourceScope,
        resource_id: &str,
        op: RoomResourceOp,
    ) -> Result<ResolvedResource, RoomResourceError>;

    /// Record one audit fact. Failures are logged by the tool, never surfaced
    /// as content, and never block a result already computed.
    async fn record(&self, scope: &RoomResourceScope, fact: RoomResourceAuditFact);
}

/// One catalog line the tool description shows the model: the alias it will
/// recognise, the opaque id it must pass, and what it may do there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomResourceCatalogEntry {
    pub resource_id: String,
    pub display_name: String,
    pub access_mode: String,
}

/// Opaque, non-serializable authority for one admitted resource reader.
#[derive(Clone)]
pub struct AdmittedRoomResources {
    scope: RoomResourceScope,
    authority: Arc<dyn RoomResourceAuthority>,
    catalog: Arc<Vec<RoomResourceCatalogEntry>>,
}

impl std::fmt::Debug for AdmittedRoomResources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdmittedRoomResources")
            .field("scope", &self.scope)
            .field("catalog", &self.catalog.len())
            .finish_non_exhaustive()
    }
}

impl AdmittedRoomResources {
    pub(crate) fn from_admission(
        admission: &impl RoomResourceAdmission,
        authority: Arc<dyn RoomResourceAuthority>,
        catalog: Vec<RoomResourceCatalogEntry>,
    ) -> anyhow::Result<Self> {
        let room_key = admission.admitted_room_key();
        let agent_member_id = admission.admitted_agent_member_id();
        anyhow::ensure!(
            !room_key.is_empty(),
            "room resource admission has no Room key"
        );
        anyhow::ensure!(
            !agent_member_id.is_empty(),
            "room resource admission has no agent member"
        );
        anyhow::ensure!(
            admission.admitted_generation() > 0,
            "room resource admission has no authority generation"
        );
        Ok(Self {
            scope: RoomResourceScope {
                room_key: room_key.to_string(),
                agent_member_id: agent_member_id.to_string(),
                binding_generation: admission.admitted_generation(),
            },
            authority,
            catalog: Arc::new(catalog),
        })
    }

    pub fn scope(&self) -> &RoomResourceScope {
        &self.scope
    }

    pub fn catalog(&self) -> &[RoomResourceCatalogEntry] {
        &self.catalog
    }

    /// The two reserved tools. Names are reserved in the ambient toolset the
    /// same way `room_history` is.
    pub fn tools(&self) -> Vec<SharedTool> {
        let description_suffix = catalog_text(&self.catalog);
        vec![
            Arc::new(RoomListTool {
                authority: self.clone(),
                description: format!(
                    "List one directory inside a folder this Room has contributed to you. \
                     Non-recursive, at most {MAX_LIST_ENTRIES} entries, names only. \
                     The Room, agent, and authority are fixed by admission; you choose a \
                     resource_id from the catalog and a relative path.{description_suffix}"
                ),
            }) as SharedTool,
            Arc::new(RoomReadTool {
                authority: self.clone(),
                description: format!(
                    "Read a UTF-8 text file inside a folder this Room has contributed to you, \
                     in bounded chunks (default {DEFAULT_READ_CHUNK_BYTES} bytes; page with \
                     next_offset). Files over {MAX_READ_FILE_BYTES} bytes and binary files are \
                     refused with a typed reason. The Room, agent, and authority are fixed by \
                     admission.{description_suffix}"
                ),
            }) as SharedTool,
        ]
    }
}

pub const ROOM_LIST_TOOL: &str = "room_list";
pub const ROOM_READ_TOOL: &str = "room_read";

fn catalog_text(catalog: &[RoomResourceCatalogEntry]) -> String {
    if catalog.is_empty() {
        return " No folders are currently contributed.".to_string();
    }
    let mut out = String::from(" Contributed folders (resource_id: alias [access]):");
    for entry in catalog {
        out.push_str(&format!(
            " {}: {} [{}];",
            entry.resource_id, entry.display_name, entry.access_mode
        ));
    }
    out
}

// ── Confinement ───────────────────────────────────────────────────────────────

/// Why a relative path was refused under a root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfineRefusal {
    /// Absolute, or contains `..`/`.`, or a component the OS treats as
    /// special. Refused lexically before touching the filesystem.
    NotRelative,
    NotFound,
    /// Canonicalized outside the root (a symlink escape).
    Escapes,
}

impl ConfineRefusal {
    pub fn code(self) -> &'static str {
        match self {
            Self::NotRelative => "invalid_relative_path",
            Self::NotFound => "path_not_found",
            Self::Escapes => "path_escapes_root",
        }
    }
}

/// Resolve `relative` under `canonical_root` and prove the result stays
/// inside. `relative` may be empty (the root itself). The check is on the
/// canonicalized RESULT, after symlink resolution — a prefix check on the
/// joined string would accept `link-to-home/.ssh` when `link-to-home` points
/// out of the folder.
pub fn confine(canonical_root: &Path, relative: &str) -> Result<PathBuf, ConfineRefusal> {
    let rel = Path::new(relative);
    if rel.is_absolute() {
        return Err(ConfineRefusal::NotRelative);
    }
    for component in rel.components() {
        match component {
            Component::Normal(_) => {}
            _ => return Err(ConfineRefusal::NotRelative),
        }
    }
    let joined = canonical_root.join(rel);
    let resolved = match std::fs::canonicalize(&joined) {
        Ok(resolved) => resolved,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ConfineRefusal::NotFound)
        }
        Err(_) => return Err(ConfineRefusal::Escapes),
    };
    if resolved != canonical_root && !resolved.starts_with(canonical_root) {
        return Err(ConfineRefusal::Escapes);
    }
    Ok(resolved)
}

/// Lexical normalization for the audit digest: `a//b/` → `a/b`.
fn normalized_relative(relative: &str) -> String {
    Path::new(relative)
        .components()
        .filter_map(|c| match c {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn path_digest(relative: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalized_relative(relative).as_bytes());
    format!("{:x}", hasher.finalize())
}

// ── Tools ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    resource_id: String,
    #[serde(default)]
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    resource_id: String,
    path: String,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    max_bytes: Option<usize>,
}

fn refusal(code: &str, detail: Option<String>) -> AgentToolResult {
    let mut body = json!({ "ok": false, "error": code });
    if let Some(detail) = detail {
        body["detail"] = json!(detail);
    }
    AgentToolResult::text(body.to_string())
}

/// Validate the id shape the daemon mints (`res-<hex>`), so a model cannot
/// turn the argument into a path or a query.
fn valid_resource_id(raw: &str) -> bool {
    raw.len() <= 128
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !raw.is_empty()
}

struct RoomListTool {
    authority: AdmittedRoomResources,
    description: String,
}

#[async_trait]
impl AgentTool for RoomListTool {
    fn name(&self) -> &str {
        ROOM_LIST_TOOL
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "resource_id": {
                    "type": "string",
                    "description": "A resource_id from the contributed-folder catalog"
                },
                "path": {
                    "type": "string",
                    "description": "Relative directory inside the folder; empty for its root"
                }
            },
            "required": ["resource_id"],
            "additionalProperties": false
        })
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let args: ListArgs =
            serde_json::from_value(args).map_err(|_| "invalid room_list arguments".to_string())?;
        if !valid_resource_id(&args.resource_id) {
            return Ok(refusal("resource_not_found", None));
        }
        let scope = &self.authority.scope;
        let authority = &self.authority.authority;
        let digest = path_digest(&args.path);
        let mut fact = RoomResourceAuditFact {
            resource_id: args.resource_id.clone(),
            grant_generation: None,
            op: RoomResourceOp::List,
            relative_path_digest: digest,
            bytes: 0,
            entries: 0,
            outcome: String::new(),
        };
        let resolved = match authority
            .resolve(scope, &args.resource_id, RoomResourceOp::List)
            .await
        {
            Ok(resolved) => resolved,
            Err(error) => {
                fact.outcome = error.code().to_string();
                authority.record(scope, fact).await;
                return Ok(refusal(error.code(), None));
            }
        };
        fact.grant_generation = Some(resolved.grant_generation);
        let root = resolved.local_root.clone();
        let relative = args.path.clone();
        let listing = tokio::time::timeout(
            OPERATION_DEADLINE,
            tokio::task::spawn_blocking(move || list_dir(&root, &relative)),
        )
        .await;
        let outcome = match listing {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(("list_failed", None)),
            Err(_) => Err(("deadline_exceeded", None)),
        };
        match outcome {
            Ok((entries, truncated)) => {
                fact.entries = entries.len() as u64;
                fact.outcome = "ok".into();
                authority.record(scope, fact).await;
                Ok(AgentToolResult::text(
                    json!({
                        "ok": true,
                        "resource_id": args.resource_id,
                        "path": normalized_relative(&args.path),
                        "entries": entries,
                        "truncated": truncated,
                        "grant_generation": resolved.grant_generation.to_string(),
                    })
                    .to_string(),
                ))
            }
            Err((code, detail)) => {
                fact.outcome = code.to_string();
                authority.record(scope, fact).await;
                Ok(refusal(code, detail))
            }
        }
    }
}

type ListOutcome = Result<(Vec<Value>, bool), (&'static str, Option<String>)>;

fn list_dir(root: &Path, relative: &str) -> ListOutcome {
    let dir = confine(root, relative).map_err(|refusal| (refusal.code(), None))?;
    let meta = std::fs::symlink_metadata(&dir).map_err(|_| ("path_not_found", None))?;
    if !meta.is_dir() {
        return Err(("not_a_directory", None));
    }
    let read = std::fs::read_dir(&dir).map_err(|_| ("list_failed", None))?;
    let mut entries = Vec::new();
    let mut truncated = false;
    for entry in read {
        let Ok(entry) = entry else { continue };
        if entries.len() >= MAX_LIST_ENTRIES {
            truncated = true;
            break;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let name: String = name.chars().take(MAX_ENTRY_NAME_CHARS).collect();
        // symlink_metadata: report a link AS a link and never follow it here.
        let (kind, size) = match entry.metadata() {
            Ok(meta) if meta.file_type().is_symlink() => ("symlink", None),
            Ok(meta) if meta.is_dir() => ("dir", None),
            Ok(meta) if meta.is_file() => ("file", Some(meta.len())),
            Ok(_) => ("other", None),
            Err(_) => ("other", None),
        };
        entries.push(json!({ "name": name, "kind": kind, "size": size }));
    }
    entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok((entries, truncated))
}

struct RoomReadTool {
    authority: AdmittedRoomResources,
    description: String,
}

#[async_trait]
impl AgentTool for RoomReadTool {
    fn name(&self) -> &str {
        ROOM_READ_TOOL
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "resource_id": {
                    "type": "string",
                    "description": "A resource_id from the contributed-folder catalog"
                },
                "path": {
                    "type": "string",
                    "description": "Relative file path inside the folder"
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Byte offset to start from (use next_offset to continue)"
                },
                "max_bytes": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_READ_CHUNK_BYTES,
                    "description": "Bytes to return in this chunk"
                }
            },
            "required": ["resource_id", "path"],
            "additionalProperties": false
        })
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let args: ReadArgs =
            serde_json::from_value(args).map_err(|_| "invalid room_read arguments".to_string())?;
        if !valid_resource_id(&args.resource_id) {
            return Ok(refusal("resource_not_found", None));
        }
        let scope = &self.authority.scope;
        let authority = &self.authority.authority;
        let mut fact = RoomResourceAuditFact {
            resource_id: args.resource_id.clone(),
            grant_generation: None,
            op: RoomResourceOp::Read,
            relative_path_digest: path_digest(&args.path),
            bytes: 0,
            entries: 0,
            outcome: String::new(),
        };
        let resolved = match authority
            .resolve(scope, &args.resource_id, RoomResourceOp::Read)
            .await
        {
            Ok(resolved) => resolved,
            Err(error) => {
                fact.outcome = error.code().to_string();
                authority.record(scope, fact).await;
                return Ok(refusal(error.code(), None));
            }
        };
        fact.grant_generation = Some(resolved.grant_generation);
        let root = resolved.local_root.clone();
        let relative = args.path.clone();
        let offset = args.offset.unwrap_or(0);
        let max_bytes = args
            .max_bytes
            .unwrap_or(DEFAULT_READ_CHUNK_BYTES)
            .clamp(1, MAX_READ_CHUNK_BYTES);
        let read = tokio::time::timeout(
            OPERATION_DEADLINE,
            tokio::task::spawn_blocking(move || read_chunk(&root, &relative, offset, max_bytes)),
        )
        .await;
        let outcome = match read {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(("read_failed", None)),
            Err(_) => Err(("deadline_exceeded", None)),
        };
        match outcome {
            Ok(chunk) => {
                fact.bytes = chunk.bytes.len() as u64;
                fact.outcome = "ok".into();
                authority.record(scope, fact).await;
                let text = String::from_utf8_lossy(&chunk.bytes).into_owned();
                Ok(AgentToolResult::text(
                    json!({
                        "ok": true,
                        "resource_id": args.resource_id,
                        "path": normalized_relative(&args.path),
                        "offset": offset.to_string(),
                        "bytes": chunk.bytes.len(),
                        "file_size": chunk.file_size.to_string(),
                        "next_offset": chunk.next_offset.map(|n| n.to_string()),
                        "content": text,
                        "grant_generation": resolved.grant_generation.to_string(),
                    })
                    .to_string(),
                ))
            }
            Err((code, detail)) => {
                fact.outcome = code.to_string();
                authority.record(scope, fact).await;
                Ok(refusal(code, detail))
            }
        }
    }
}

struct ReadChunk {
    bytes: Vec<u8>,
    file_size: u64,
    next_offset: Option<u64>,
}

type ReadOutcome = Result<ReadChunk, (&'static str, Option<String>)>;

fn read_chunk(root: &Path, relative: &str, offset: u64, max_bytes: usize) -> ReadOutcome {
    use std::io::{Read as _, Seek as _, SeekFrom};
    if relative.trim().is_empty() {
        return Err(("invalid_relative_path", None));
    }
    let file = confine(root, relative).map_err(|refusal| (refusal.code(), None))?;
    let meta = std::fs::metadata(&file).map_err(|_| ("path_not_found", None))?;
    if !meta.is_file() {
        return Err(("not_a_file", None));
    }
    let file_size = meta.len();
    if file_size > MAX_READ_FILE_BYTES {
        return Err((
            "file_too_large",
            Some(format!(
                "{file_size} bytes exceeds the {MAX_READ_FILE_BYTES} byte read budget"
            )),
        ));
    }
    if offset > file_size {
        return Err(("offset_out_of_range", None));
    }
    // Binary check on the FIRST chunk only: a file that starts as text is text.
    let mut handle = std::fs::File::open(&file).map_err(|_| ("read_failed", None))?;
    if offset == 0 {
        let mut probe = vec![0u8; 8_192.min(file_size as usize)];
        handle
            .read_exact(&mut probe)
            .map_err(|_| ("read_failed", None))?;
        if probe.contains(&0u8) {
            return Err(("binary_not_supported", None));
        }
        handle
            .seek(SeekFrom::Start(0))
            .map_err(|_| ("read_failed", None))?;
    } else {
        handle
            .seek(SeekFrom::Start(offset))
            .map_err(|_| ("read_failed", None))?;
    }
    let want = max_bytes.min((file_size - offset) as usize);
    let mut bytes = vec![0u8; want];
    handle
        .read_exact(&mut bytes)
        .map_err(|_| ("read_failed", None))?;
    // Never split a UTF-8 sequence across chunks: back off to a boundary.
    let mut end = bytes.len();
    while end > 0 && std::str::from_utf8(&bytes[..end]).is_err() {
        end -= 1;
        if bytes.len() - end > 3 {
            break;
        }
    }
    if end == 0 && !bytes.is_empty() {
        return Err(("binary_not_supported", None));
    }
    bytes.truncate(end);
    let next = offset + bytes.len() as u64;
    Ok(ReadChunk {
        bytes,
        file_size,
        next_offset: (next < file_size).then_some(next),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct TestAdmission;
    impl RoomResourceAdmission for TestAdmission {
        fn admitted_room_key(&self) -> &str {
            "hq"
        }
        fn admitted_agent_member_id(&self) -> &str {
            "builder"
        }
        fn admitted_generation(&self) -> u64 {
            3
        }
    }

    struct FakeAuthority {
        root: PathBuf,
        grant_generation: u64,
        refuse: Option<RoomResourceError>,
        facts: Mutex<Vec<RoomResourceAuditFact>>,
        seen_scopes: Mutex<Vec<RoomResourceScope>>,
    }

    #[async_trait]
    impl RoomResourceAuthority for FakeAuthority {
        async fn resolve(
            &self,
            scope: &RoomResourceScope,
            resource_id: &str,
            _op: RoomResourceOp,
        ) -> Result<ResolvedResource, RoomResourceError> {
            self.seen_scopes.lock().unwrap().push(scope.clone());
            if let Some(refuse) = &self.refuse {
                return Err(refuse.clone());
            }
            if resource_id != "res-1" {
                return Err(RoomResourceError::NotFound);
            }
            Ok(ResolvedResource {
                local_root: self.root.clone(),
                grant_generation: self.grant_generation,
            })
        }

        async fn record(&self, _scope: &RoomResourceScope, fact: RoomResourceAuditFact) {
            self.facts.lock().unwrap().push(fact);
        }
    }

    fn fixture(
        refuse: Option<RoomResourceError>,
    ) -> (tempfile::TempDir, Arc<FakeAuthority>, AdmittedRoomResources) {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap().join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("hello.txt"), "hello, room\nline two\n").unwrap();
        std::fs::write(root.join("sub/bin.dat"), [0u8, 159, 146, 150]).unwrap();
        let authority = Arc::new(FakeAuthority {
            root: root.clone(),
            grant_generation: 7,
            refuse,
            facts: Mutex::new(Vec::new()),
            seen_scopes: Mutex::new(Vec::new()),
        });
        let admitted = AdmittedRoomResources::from_admission(
            &TestAdmission,
            authority.clone(),
            vec![RoomResourceCatalogEntry {
                resource_id: "res-1".into(),
                display_name: "source".into(),
                access_mode: "read".into(),
            }],
        )
        .unwrap();
        (tmp, authority, admitted)
    }

    async fn call(admitted: &AdmittedRoomResources, name: &str, args: Value) -> Value {
        let tool = admitted
            .tools()
            .into_iter()
            .find(|t| t.name() == name)
            .expect("tool");
        let result = tool.execute("call-1", args).await.unwrap();
        let text = match &result.content[0] {
            ocean_protocol::Content::Text { text } => text.clone(),
            other => panic!("unexpected content {other:?}"),
        };
        serde_json::from_str(&text).unwrap()
    }

    #[tokio::test]
    async fn list_is_scoped_confined_non_recursive_and_audited() {
        let (_tmp, authority, admitted) = fixture(None);
        let out = call(&admitted, ROOM_LIST_TOOL, json!({"resource_id": "res-1"})).await;
        assert_eq!(out["ok"], json!(true), "{out}");
        let names: Vec<&str> = out["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["hello.txt", "sub"]);
        assert_eq!(out["entries"][1]["kind"], json!("dir"));
        assert_eq!(out["grant_generation"], json!("7"));
        assert!(
            !out.to_string().contains("/root"),
            "no root in output: {out}"
        );

        {
            let scopes = authority.seen_scopes.lock().unwrap();
            assert_eq!(scopes[0].room_key(), "hq");
            assert_eq!(scopes[0].agent_member_id(), "builder");
            assert_eq!(scopes[0].binding_generation(), 3);
        }

        for (path, code) in [
            ("../", "invalid_relative_path"),
            ("/etc", "invalid_relative_path"),
            ("nope", "path_not_found"),
            ("hello.txt", "not_a_directory"),
        ] {
            let out = call(
                &admitted,
                ROOM_LIST_TOOL,
                json!({"resource_id": "res-1", "path": path}),
            )
            .await;
            assert_eq!(out["ok"], json!(false), "{path}");
            assert_eq!(out["error"], json!(code), "{path}");
        }
        let out = call(&admitted, ROOM_LIST_TOOL, json!({"resource_id": "res-9"})).await;
        assert_eq!(out["error"], json!("resource_not_found"));
        let out = call(&admitted, ROOM_LIST_TOOL, json!({"resource_id": "../x"})).await;
        assert_eq!(out["error"], json!("resource_not_found"));

        let facts = authority.facts.lock().unwrap();
        assert_eq!(facts[0].outcome, "ok");
        assert_eq!(facts[0].entries, 2);
        assert_eq!(facts[0].grant_generation, Some(7));
        assert_eq!(facts[0].op, RoomResourceOp::List);
        assert_eq!(facts[0].relative_path_digest, path_digest(""));
        assert!(facts.iter().any(|f| f.outcome == "invalid_relative_path"));
        assert!(facts.iter().any(|f| f.outcome == "resource_not_found"));
    }

    #[tokio::test]
    async fn read_is_chunked_utf8_safe_and_refuses_binary_and_escapes() {
        let (_tmp, authority, admitted) = fixture(None);
        let out = call(
            &admitted,
            ROOM_READ_TOOL,
            json!({"resource_id": "res-1", "path": "hello.txt", "max_bytes": 6}),
        )
        .await;
        assert_eq!(out["ok"], json!(true), "{out}");
        assert_eq!(out["content"], json!("hello,"));
        assert_eq!(out["next_offset"], json!("6"));
        assert_eq!(out["file_size"], json!("21"));
        let out = call(
            &admitted,
            ROOM_READ_TOOL,
            json!({"resource_id": "res-1", "path": "hello.txt", "offset": 6}),
        )
        .await;
        assert_eq!(out["content"], json!(" room\nline two\n"));
        assert_eq!(out["next_offset"], serde_json::Value::Null);

        let out = call(
            &admitted,
            ROOM_READ_TOOL,
            json!({"resource_id": "res-1", "path": "sub/bin.dat"}),
        )
        .await;
        assert_eq!(out["error"], json!("binary_not_supported"));
        let out = call(
            &admitted,
            ROOM_READ_TOOL,
            json!({"resource_id": "res-1", "path": "sub"}),
        )
        .await;
        assert_eq!(out["error"], json!("not_a_file"));
        let out = call(
            &admitted,
            ROOM_READ_TOOL,
            json!({"resource_id": "res-1", "path": "../etc/passwd"}),
        )
        .await;
        assert_eq!(out["error"], json!("invalid_relative_path"));
        let out = call(
            &admitted,
            ROOM_READ_TOOL,
            json!({"resource_id": "res-1", "path": "hello.txt", "offset": 99}),
        )
        .await;
        assert_eq!(out["error"], json!("offset_out_of_range"));

        let facts = authority.facts.lock().unwrap();
        assert_eq!(facts[0].bytes, 6);
        assert_eq!(facts[0].op, RoomResourceOp::Read);
        assert_eq!(facts[0].relative_path_digest, path_digest("hello.txt"));
        assert!(facts.iter().any(|f| f.outcome == "binary_not_supported"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_escape_is_refused_on_the_resolved_path() {
        let (tmp, _authority, admitted) = fixture(None);
        let root = std::fs::canonicalize(tmp.path()).unwrap().join("root");
        let outside = std::fs::canonicalize(tmp.path()).unwrap().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "no").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        let out = call(&admitted, ROOM_LIST_TOOL, json!({"resource_id": "res-1"})).await;
        let escape = out["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "escape")
            .unwrap();
        assert_eq!(
            escape["kind"],
            json!("symlink"),
            "listed as a link, never followed"
        );
        let out = call(
            &admitted,
            ROOM_LIST_TOOL,
            json!({"resource_id": "res-1", "path": "escape"}),
        )
        .await;
        assert_eq!(out["error"], json!("path_escapes_root"));
        let out = call(
            &admitted,
            ROOM_READ_TOOL,
            json!({"resource_id": "res-1", "path": "escape/secret.txt"}),
        )
        .await;
        assert_eq!(out["error"], json!("path_escapes_root"));
    }

    #[tokio::test]
    async fn an_authority_refusal_is_the_result_and_is_audited_before_any_io() {
        let (_tmp, authority, admitted) = fixture(Some(RoomResourceError::StaleGeneration));
        let out = call(&admitted, ROOM_LIST_TOOL, json!({"resource_id": "res-1"})).await;
        assert_eq!(out["ok"], json!(false));
        assert_eq!(out["error"], json!("stale_generation"));
        let facts = authority.facts.lock().unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].outcome, "stale_generation");
        assert_eq!(facts[0].grant_generation, None);
    }

    #[test]
    fn a_listing_is_capped_at_the_decision_8_ceiling() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        for i in 0..(MAX_LIST_ENTRIES + 5) {
            std::fs::write(root.join(format!("f{i:05}")), b"").unwrap();
        }
        let (entries, truncated) = list_dir(&root, "").unwrap();
        assert_eq!(entries.len(), MAX_LIST_ENTRIES);
        assert!(truncated);
    }

    #[test]
    fn the_catalog_is_in_the_description_and_from_admission_checks_evidence() {
        let (_tmp, _authority, admitted) = fixture(None);
        let tools = admitted.tools();
        assert_eq!(tools.len(), 2);
        assert!(tools[0].description().contains("res-1: source [read]"));
        assert_eq!(tools[0].name(), ROOM_LIST_TOOL);
        assert_eq!(tools[1].name(), ROOM_READ_TOOL);

        struct Bad;
        impl RoomResourceAdmission for Bad {
            fn admitted_room_key(&self) -> &str {
                "hq"
            }
            fn admitted_agent_member_id(&self) -> &str {
                "builder"
            }
            fn admitted_generation(&self) -> u64 {
                0
            }
        }
        let err = AdmittedRoomResources::from_admission(
            &Bad,
            Arc::new(FakeAuthority {
                root: PathBuf::from("/"),
                grant_generation: 1,
                refuse: None,
                facts: Mutex::new(Vec::new()),
                seen_scopes: Mutex::new(Vec::new()),
            }),
            vec![],
        )
        .unwrap_err();
        assert!(err.to_string().contains("generation"));
    }
}
