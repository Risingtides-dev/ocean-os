//! `retain` / `recall` — the agent-facing memory verbs over `ocean-memory`.
//!
//! The port map's "cheapest win": the typed SQLite store existed but nothing
//! wired it into the daemon, so no turn could remember anything across
//! sessions. This provider registers two tools through the same capability
//! seam as MCP/LSP:
//!
//! - `retain {text, kind?}` — persist one durable fact (operator scope).
//! - `recall {query?, limit?}` — newest-first case-insensitive substring
//!   search over retained memories (BM25 ranking is a later tier; substring
//!   over a bounded scan is deterministic and good enough to be useful today).
//!
//! Storage discipline follows the crate's contract: sync `rusqlite` behind a
//! `Mutex`, guard never held across an `.await`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ocean_context::{ClaimStatus, Provenance};
use ocean_memory::{
    room_memory_owner, Memory, MemoryId, MemoryKind, MemoryScope, MemoryStore, PrincipalId,
    RoomMemoryAdmission, SessionMemoryScope, SqliteMemoryStore,
};
use ocean_runtime::capability::{CapabilityProvider, ProviderHealth, SessionContext, SharedTool};
use ocean_runtime::types::{AgentTool, AgentToolResult};
use serde_json::{json, Value};

/// Rows scanned per recall (paged; newest first). Bounds worst-case work on a
/// large store while covering far more than a query usually needs.
const RECALL_SCAN_CAP: usize = 500;
/// Default (and max) matches returned.
const RECALL_DEFAULT_LIMIT: usize = 8;
const RECALL_MAX_LIMIT: usize = 25;
/// Cap on one retained fact — memory is for durable FACTS, not dumps; anything
/// bigger belongs in a file or artifact the fact can point at.
const MAX_RETAIN_CHARS: usize = 4000;

type SharedStore = Arc<Mutex<SqliteMemoryStore>>;

/// One process-wide memory-store handle used to issue mutually exclusive
/// operator and admitted-Room tool authorities.
#[derive(Clone)]
pub(crate) struct MemoryToolsFactory {
    store: SharedStore,
}

impl std::fmt::Debug for MemoryToolsFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MemoryToolsFactory")
    }
}

impl MemoryToolsFactory {
    pub(crate) fn open(path: &std::path::Path) -> anyhow::Result<Self> {
        Ok(Self {
            store: Arc::new(Mutex::new(SqliteMemoryStore::open(path)?)),
        })
    }

    pub(crate) fn operator_provider(&self) -> MemoryToolsProvider {
        MemoryToolsProvider {
            store: self.store.clone(),
            owner: PrincipalId::new("operator"),
        }
    }

    pub(crate) fn admit_room(
        &self,
        admission: &impl RoomMemoryAdmission,
    ) -> anyhow::Result<AdmittedRoomMemory> {
        let scope = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .trusted_room_scope(admission)?;
        Ok(AdmittedRoomMemory {
            store: self.store.clone(),
            owner: room_memory_owner(),
            scope,
        })
    }
}

/// Opaque, non-serializable authority for one admitted Room's shared memory.
///
/// Its fields are private and its only constructor is
/// [`MemoryToolsFactory::admit_room`], which requires final admission evidence.
#[derive(Clone)]
pub struct AdmittedRoomMemory {
    store: SharedStore,
    owner: PrincipalId,
    scope: SessionMemoryScope,
}

impl std::fmt::Debug for AdmittedRoomMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AdmittedRoomMemory")
    }
}

impl AdmittedRoomMemory {
    pub(crate) fn tools(&self) -> Vec<SharedTool> {
        scoped_tools(self.store.clone(), self.owner.clone(), self.scope.clone())
    }
}

/// A read-only view of one retained memory for a surface (the TUI `/memory`
/// picker). Flattens the store's rich `Memory` down to what a browser shows:
/// id, kind, the `text` body, and the last-mutation timestamp.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemoryView {
    pub id: String,
    pub kind: String,
    pub text: String,
    pub updated_at: i64,
}

/// List the operator's retained memories, newest first, for a read-only
/// surface. Opens the store at `path` (the daemon's `memory.sqlite`), pages
/// through up to `cap` rows, and returns flattened views. A missing/unopenable
/// store yields an empty list rather than an error — the picker shows "no
/// memories yet", never a failure wall.
pub fn list_memories(path: &std::path::Path, cap: usize) -> Vec<MemoryView> {
    let Ok(store) = SqliteMemoryStore::open(path) else {
        return Vec::new();
    };
    let owner = PrincipalId::new("operator");
    let mut out = Vec::new();
    let mut after: Option<u64> = None;
    while out.len() < cap {
        let page = match store.list_page(&owner, after, Some(100)) {
            Ok(p) => p,
            Err(_) => break,
        };
        if page.memories.is_empty() {
            break;
        }
        for mem in &page.memories {
            out.push(MemoryView {
                id: mem.id.0.clone(),
                kind: mem.kind.as_str().to_string(),
                text: mem
                    .body
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                updated_at: mem.updated_at,
            });
            if out.len() >= cap {
                break;
            }
        }
        match page.next_seq {
            Some(seq) if page.has_more => after = Some(seq),
            _ => break,
        }
    }
    out
}

pub struct MemoryToolsProvider {
    store: SharedStore,
    owner: PrincipalId,
}

impl MemoryToolsProvider {
    #[cfg(test)]
    fn in_memory() -> Self {
        Self {
            store: Arc::new(Mutex::new(SqliteMemoryStore::open_in_memory().unwrap())),
            owner: PrincipalId::new("operator"),
        }
    }
}

#[async_trait]
impl CapabilityProvider for MemoryToolsProvider {
    fn id(&self) -> &str {
        "memory"
    }

    async fn tools(&self, _ctx: &SessionContext) -> Vec<SharedTool> {
        vec![
            Arc::new(RetainTool {
                store: self.store.clone(),
                owner: self.owner.clone(),
                room_scope: None,
            }) as SharedTool,
            Arc::new(RecallTool {
                store: self.store.clone(),
                owner: self.owner.clone(),
                room_scope: None,
            }) as SharedTool,
        ]
    }

    async fn health(&self) -> ProviderHealth {
        ProviderHealth::Ready
    }
}

fn scoped_tools(
    store: SharedStore,
    owner: PrincipalId,
    scope: SessionMemoryScope,
) -> Vec<SharedTool> {
    vec![
        Arc::new(RetainTool {
            store: store.clone(),
            owner: owner.clone(),
            room_scope: Some(scope.clone()),
        }) as SharedTool,
        Arc::new(RecallTool {
            store,
            owner,
            room_scope: Some(scope),
        }) as SharedTool,
    ]
}

struct RetainTool {
    store: SharedStore,
    owner: PrincipalId,
    room_scope: Option<SessionMemoryScope>,
}

#[async_trait]
impl AgentTool for RetainTool {
    fn name(&self) -> &str {
        "retain"
    }
    fn description(&self) -> &str {
        if self.room_scope.is_some() {
            "Persist one durable fact to this Room's shared memory. \
             Use for stable Room decisions and context worth remembering — not transcripts or dumps. \
             kind ∈ {fact, preference, relationship, event, skill} (default fact)."
        } else {
            "Persist one durable fact to long-term memory (survives across sessions). \
             Use for stable facts, preferences, and decisions worth remembering — not transcripts or dumps. \
             kind ∈ {fact, preference, relationship, event, skill} (default fact)."
        }
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "The fact to remember, one self-contained sentence or two"},
                "kind": {"type": "string", "enum": ["fact", "preference", "relationship", "event", "skill"]}
            },
            "required": ["text"]
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or("missing 'text'")?;
        if text.chars().count() > MAX_RETAIN_CHARS {
            return Err(format!(
                "retain holds durable facts, not dumps — {MAX_RETAIN_CHARS} chars max \
                 (write long content to a file and retain a pointer to it)"
            ));
        }
        let kind = args
            .get("kind")
            .and_then(|v| v.as_str())
            .map(|k| match k {
                "preference" => MemoryKind::Preference,
                "relationship" => MemoryKind::Relationship,
                "event" => MemoryKind::Event,
                "skill" => MemoryKind::Skill,
                _ => MemoryKind::Fact,
            })
            .unwrap_or(MemoryKind::Fact);
        let now = unix_secs();
        let mem = Memory {
            id: MemoryId::new(),
            scope: MemoryScope::Operator,
            owner: self.owner.clone(),
            kind,
            body: json!({ "text": text }),
            provenance: Provenance {
                anchors: Vec::new(),
                tickets: Vec::new(),
                commit_sha: String::new(),
            },
            trust: ClaimStatus::Asserted,
            seq: 0, // store assigns
            written_at: now,
            updated_at: now,
            history: Vec::new(),
        };
        let stored = {
            let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
            match self.room_scope.as_ref() {
                Some(scope) => store
                    .scoped(scope, &self.owner)
                    .and_then(|mut scoped| scoped.put(mem)),
                None => store.put(mem),
            }
            .map_err(|e| format!("retain: {e:?}"))?
        };
        Ok(AgentToolResult::text(format!(
            "retained ({}, seq {}): {}",
            stored.kind.as_str(),
            stored.seq,
            text
        )))
    }
}

struct RecallTool {
    store: SharedStore,
    owner: PrincipalId,
    room_scope: Option<SessionMemoryScope>,
}

#[async_trait]
impl AgentTool for RecallTool {
    fn name(&self) -> &str {
        "recall"
    }
    fn concurrency(&self) -> ocean_runtime::types::Concurrency {
        ocean_runtime::types::Concurrency::Shared
    }
    fn description(&self) -> &str {
        if self.room_scope.is_some() {
            "Search this Room's shared memory (facts saved with `retain` in this Room only). \
             Case-insensitive substring match, newest first. Empty query returns the most recent memories."
        } else {
            "Search long-term memory (facts saved with `retain`, across all sessions). \
             Case-insensitive substring match, newest first. Empty query returns the most recent memories."
        }
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Substring to match; empty/omitted = newest memories"},
                "limit": {"type": "integer", "description": "Max results (default 8, max 25)"}
            }
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_lowercase();
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|l| (l as usize).clamp(1, RECALL_MAX_LIMIT))
            .unwrap_or(RECALL_DEFAULT_LIMIT);

        let mut hits: Vec<Memory> = Vec::new();
        let mut scanned = 0usize;
        let mut after: Option<u64> = None;
        loop {
            let page = {
                let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
                match self.room_scope.as_ref() {
                    Some(scope) => store
                        .scoped(scope, &self.owner)
                        .and_then(|scoped| scoped.list_page(after, Some(100))),
                    None => store.list_page(&self.owner, after, Some(100)),
                }
                .map_err(|e| format!("recall: {e:?}"))?
            };
            for mem in &page.memories {
                scanned += 1;
                let text = mem
                    .body
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if query.is_empty() || text.to_lowercase().contains(&query) {
                    hits.push(mem.clone());
                    if hits.len() >= limit {
                        break;
                    }
                }
            }
            if hits.len() >= limit || !page.has_more || scanned >= RECALL_SCAN_CAP {
                break;
            }
            after = page.next_seq;
        }

        if hits.is_empty() {
            return Ok(AgentToolResult::text(if query.is_empty() {
                "(no memories retained yet)".to_string()
            } else {
                format!("(no memories match {query:?})")
            }));
        }
        let mut out = String::new();
        for mem in &hits {
            let text = mem
                .body
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            out.push_str(&format!("[{} #{}] {}\n", mem.kind.as_str(), mem.seq, text));
        }
        Ok(AgentToolResult::text(out))
    }
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestAdmission(String);

    impl RoomMemoryAdmission for TestAdmission {
        fn admitted_room_key(&self) -> &str {
            &self.0
        }
    }

    #[tokio::test]
    async fn retain_then_recall_round_trips() {
        let provider = MemoryToolsProvider::in_memory();
        let tools = provider.tools(&SessionContext::default()).await;
        let retain = tools.iter().find(|t| t.name() == "retain").unwrap();
        let recall = tools.iter().find(|t| t.name() == "recall").unwrap();

        retain
            .execute(
                "1",
                json!({ "text": "John deploys the daemon from main only", "kind": "preference" }),
            )
            .await
            .unwrap();
        retain
            .execute(
                "2",
                json!({ "text": "the health path is /health not /v1/health" }),
            )
            .await
            .unwrap();

        // Substring hit, case-insensitive.
        let r = recall
            .execute("3", json!({ "query": "HEALTH PATH" }))
            .await
            .unwrap();
        let text = r.content[0].as_text().unwrap();
        assert!(text.contains("/health"), "{text}");
        assert!(
            !text.contains("deploys"),
            "non-matching memory must not appear: {text}"
        );

        // Empty query → newest first, both present.
        let all = recall.execute("4", json!({})).await.unwrap();
        let all_text = all.content[0].as_text().unwrap();
        assert!(all_text.contains("deploys") && all_text.contains("/health"));
        // Newest (seq 2) listed before oldest (seq 1).
        assert!(
            all_text.find("/health").unwrap() < all_text.find("deploys").unwrap(),
            "newest-first ordering: {all_text}"
        );

        // No match is a clear miss, not an error.
        let miss = recall
            .execute("5", json!({ "query": "zebra" }))
            .await
            .unwrap();
        assert!(miss.content[0]
            .as_text()
            .unwrap()
            .contains("no memories match"));
    }

    #[tokio::test]
    async fn retain_rejects_dumps() {
        let provider = MemoryToolsProvider::in_memory();
        let tools = provider.tools(&SessionContext::default()).await;
        let retain = tools.iter().find(|t| t.name() == "retain").unwrap();
        let err = retain
            .execute("1", json!({ "text": "x".repeat(MAX_RETAIN_CHARS + 1) }))
            .await
            .expect_err("oversized retain must be rejected");
        assert!(err.contains("durable facts"), "{err}");
    }

    #[tokio::test]
    async fn admitted_room_tools_cannot_see_operator_or_another_room() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let factory = MemoryToolsFactory::open(temp.path()).unwrap();
        let room_a = factory.admit_room(&TestAdmission("room-a".into())).unwrap();
        let room_b = factory.admit_room(&TestAdmission("room-b".into())).unwrap();
        let operator = factory.operator_provider();

        let operator_tools = operator.tools(&SessionContext::default()).await;
        let tools_a = room_a.tools();
        let tools_b = room_b.tools();

        operator_tools
            .iter()
            .find(|tool| tool.name() == "retain")
            .unwrap()
            .execute("operator", json!({"text": "operator secret"}))
            .await
            .unwrap();
        tools_a
            .iter()
            .find(|tool| tool.name() == "retain")
            .unwrap()
            .execute("a", json!({"text": "room a fact"}))
            .await
            .unwrap();
        tools_b
            .iter()
            .find(|tool| tool.name() == "retain")
            .unwrap()
            .execute("b", json!({"text": "room b fact"}))
            .await
            .unwrap();

        let recalled_a = tools_a
            .iter()
            .find(|tool| tool.name() == "recall")
            .unwrap()
            .execute("recall-a", json!({}))
            .await
            .unwrap();
        let text_a = recalled_a.content[0].as_text().unwrap();
        assert!(text_a.contains("room a fact"), "{text_a}");
        assert!(!text_a.contains("room b fact"), "{text_a}");
        assert!(!text_a.contains("operator secret"), "{text_a}");

        let recalled_b = tools_b
            .iter()
            .find(|tool| tool.name() == "recall")
            .unwrap()
            .execute("recall-b", json!({}))
            .await
            .unwrap();
        let text_b = recalled_b.content[0].as_text().unwrap();
        assert!(text_b.contains("room b fact"), "{text_b}");
        assert!(!text_b.contains("room a fact"), "{text_b}");
        assert!(!text_b.contains("operator secret"), "{text_b}");

        let operator_recall = operator_tools
            .iter()
            .find(|tool| tool.name() == "recall")
            .unwrap()
            .execute("recall-operator", json!({}))
            .await
            .unwrap();
        let operator_text = operator_recall.content[0].as_text().unwrap();
        assert!(operator_text.contains("operator secret"), "{operator_text}");
        assert!(!operator_text.contains("room a fact"), "{operator_text}");
        assert!(!operator_text.contains("room b fact"), "{operator_text}");
    }

    #[tokio::test]
    async fn two_admissions_to_one_room_share_memory() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let factory = MemoryToolsFactory::open(temp.path()).unwrap();
        let first = factory
            .admit_room(&TestAdmission("shared-room".into()))
            .unwrap();
        let second = factory
            .admit_room(&TestAdmission("shared-room".into()))
            .unwrap();
        let first_tools = first.tools();
        let second_tools = second.tools();

        first_tools
            .iter()
            .find(|tool| tool.name() == "retain")
            .unwrap()
            .execute("retain", json!({"text": "shared room decision"}))
            .await
            .unwrap();
        let recalled = second_tools
            .iter()
            .find(|tool| tool.name() == "recall")
            .unwrap()
            .execute("recall", json!({}))
            .await
            .unwrap();
        assert!(recalled.content[0]
            .as_text()
            .unwrap()
            .contains("shared room decision"));
    }
}
