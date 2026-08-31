//! Opaque, admission-scoped retrieval of durable Room transcript history.
//!
//! The daemon owns persistence and authorization. This module owns only the
//! agent-facing read tool and the typed seam that prevents a model/tool call
//! from selecting a Room, agent identity, or authority generation.

use std::sync::Arc;

use async_trait::async_trait;
use ocean_runtime::{
    types::{AgentTool, AgentToolResult, Concurrency},
    SharedTool,
};
use serde::Deserialize;
use serde_json::{json, Value};

const DEFAULT_PAGE_LIMIT: usize = 25;
const MAX_PAGE_LIMIT: usize = 50;
const MAX_ROW_TEXT_CHARS: usize = 2_000;
const MAX_AUTHOR_ID_CHARS: usize = 256;

/// Admission evidence required to mint Room-history retrieval authority.
///
/// Implement this only on the daemon's private, final admission type. Request
/// bodies and public Room/member projections are not admission evidence.
pub trait RoomHistoryAdmission {
    fn admitted_room_key(&self) -> &str;
    fn admitted_agent_member_id(&self) -> &str;
    fn admitted_generation(&self) -> u64;
}

/// Immutable scope passed to the daemon-owned history backend on every read.
///
/// Fields are private so neither callers nor tool arguments can construct or
/// rewrite authority. The daemon backend receives read-only accessors only.
#[derive(Clone, PartialEq, Eq)]
pub struct RoomHistoryScope {
    room_key: String,
    agent_member_id: String,
    generation: u64,
}

impl std::fmt::Debug for RoomHistoryScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoomHistoryScope")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl RoomHistoryScope {
    pub fn room_key(&self) -> &str {
        &self.room_key
    }

    pub fn agent_member_id(&self) -> &str {
        &self.agent_member_id
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// One bounded, backwards page requested from the daemon-owned backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoomHistoryRequest {
    before_seq: Option<u64>,
    limit: usize,
}

impl RoomHistoryRequest {
    pub fn before_seq(self) -> Option<u64> {
        self.before_seq
    }

    pub fn limit(self) -> usize {
        self.limit
    }
}

/// Minimal author classification retained in model-visible Room history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomHistoryAuthorKind {
    Human,
    Agent,
    System,
    Bot,
    Tool,
}

impl RoomHistoryAuthorKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
            Self::System => "system",
            Self::Bot => "bot",
            Self::Tool => "tool",
        }
    }
}

/// One content-minimal durable transcript row supplied by the daemon backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomHistoryRow {
    pub seq: u64,
    pub author_id: String,
    pub author_kind: RoomHistoryAuthorKind,
    pub text: String,
}

/// One newest-first page from the daemon backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomHistoryPage {
    pub rows: Vec<RoomHistoryRow>,
    pub has_more: bool,
}

/// Fixed, non-content-bearing backend failures safe to surface to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomHistorySourceError {
    AuthorityChanged,
    Unavailable,
    Internal,
}

impl std::fmt::Display for RoomHistorySourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::AuthorityChanged => "room_history_authority_changed",
            Self::Unavailable => "room_history_unavailable",
            Self::Internal => "room_history_internal",
        })
    }
}

impl std::error::Error for RoomHistorySourceError {}

/// Daemon-owned, read-only durable Room-history source.
///
/// The source receives the immutable scope minted into the handle plus one
/// bounded backwards-page request. It receives no model-provided Room key.
#[async_trait]
pub trait RoomHistorySource: Send + Sync {
    async fn page(
        &self,
        scope: &RoomHistoryScope,
        request: RoomHistoryRequest,
    ) -> Result<RoomHistoryPage, RoomHistorySourceError>;
}

/// Opaque, non-serializable authority for one admitted Room-history reader.
///
/// Its fields are private and its only constructor is
/// [`AdmittedRoomHistory::from_admission`], called exclusively through
/// `AgentRuntime::admit_room_history`.
#[derive(Clone)]
pub struct AdmittedRoomHistory {
    scope: RoomHistoryScope,
    source: Arc<dyn RoomHistorySource>,
}

impl std::fmt::Debug for AdmittedRoomHistory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdmittedRoomHistory")
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl AdmittedRoomHistory {
    pub(crate) fn from_admission(
        admission: &impl RoomHistoryAdmission,
        source: Arc<dyn RoomHistorySource>,
    ) -> anyhow::Result<Self> {
        let room_key = admission.admitted_room_key();
        let agent_member_id = admission.admitted_agent_member_id();
        anyhow::ensure!(
            !room_key.is_empty(),
            "room history admission has no Room key"
        );
        anyhow::ensure!(
            !agent_member_id.is_empty(),
            "room history admission has no agent member"
        );
        anyhow::ensure!(
            admission.admitted_generation() > 0,
            "room history admission has no authority generation"
        );
        Ok(Self {
            scope: RoomHistoryScope {
                room_key: room_key.to_string(),
                agent_member_id: agent_member_id.to_string(),
                generation: admission.admitted_generation(),
            },
            source,
        })
    }

    pub(crate) fn tool(&self) -> SharedTool {
        Arc::new(RoomHistoryTool {
            authority: self.clone(),
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoomHistoryArgs {
    #[serde(default)]
    before_seq: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

struct RoomHistoryTool {
    authority: AdmittedRoomHistory,
}

#[async_trait]
impl AgentTool for RoomHistoryTool {
    fn name(&self) -> &str {
        "room_history"
    }

    fn description(&self) -> &str {
        "Read a bounded page of this admitted Room's durable transcript, newest first. \
         Use next_before_seq to page backward. The Room, agent, and authority generation \
         are fixed by admission and cannot be selected in arguments."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "before_seq": {
                    "type": "string",
                    "pattern": "^(0|[1-9][0-9]*)$",
                    "description": "Exclusive sequence cursor returned as next_before_seq by the previous page"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_PAGE_LIMIT,
                    "description": "Rows to return (default 25, max 50)"
                }
            },
            "additionalProperties": false
        })
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let args: RoomHistoryArgs = serde_json::from_value(args)
            .map_err(|_| "invalid room_history arguments".to_string())?;
        let before_seq = args
            .before_seq
            .as_deref()
            .map(parse_canonical_seq)
            .transpose()?;
        let limit = args
            .limit
            .unwrap_or(DEFAULT_PAGE_LIMIT)
            .clamp(1, MAX_PAGE_LIMIT);
        let request = RoomHistoryRequest { before_seq, limit };
        let page = self
            .authority
            .source
            .page(&self.authority.scope, request)
            .await
            .map_err(|error| error.to_string())?;

        let has_more = page.has_more || page.rows.len() > limit;
        let rows = page
            .rows
            .into_iter()
            .take(limit)
            .map(|row| {
                json!({
                    "seq": row.seq.to_string(),
                    "author_id": truncate_chars(&row.author_id, MAX_AUTHOR_ID_CHARS),
                    "author_kind": row.author_kind.as_str(),
                    "text": truncate_chars(&row.text, MAX_ROW_TEXT_CHARS),
                })
            })
            .collect::<Vec<_>>();
        let next_before_seq = has_more
            .then(|| rows.last().and_then(|row| row.get("seq")).cloned())
            .flatten();
        Ok(AgentToolResult::text(
            json!({
                "rows": rows,
                "next_before_seq": next_before_seq,
            })
            .to_string(),
        ))
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    value.chars().take(max_chars).collect()
}

fn parse_canonical_seq(raw: &str) -> Result<u64, String> {
    if raw.is_empty()
        || !raw.bytes().all(|byte| byte.is_ascii_digit())
        || (raw.len() > 1 && raw.starts_with('0'))
    {
        return Err("invalid room_history before_seq".to_string());
    }
    raw.parse()
        .map_err(|_| "invalid room_history before_seq".to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct TestAdmission;

    impl RoomHistoryAdmission for TestAdmission {
        fn admitted_room_key(&self) -> &str {
            "room-a"
        }

        fn admitted_agent_member_id(&self) -> &str {
            "agent-a"
        }

        fn admitted_generation(&self) -> u64 {
            7
        }
    }

    #[derive(Default)]
    struct RecordingSource {
        calls: Mutex<Vec<(RoomHistoryScope, RoomHistoryRequest)>>,
    }

    #[async_trait]
    impl RoomHistorySource for RecordingSource {
        async fn page(
            &self,
            scope: &RoomHistoryScope,
            request: RoomHistoryRequest,
        ) -> Result<RoomHistoryPage, RoomHistorySourceError> {
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((scope.clone(), request));
            Ok(RoomHistoryPage {
                rows: vec![RoomHistoryRow {
                    seq: 41,
                    author_id: "human-a".into(),
                    author_kind: RoomHistoryAuthorKind::Human,
                    text: "durable Room fact".into(),
                }],
                has_more: true,
            })
        }
    }

    #[tokio::test]
    async fn arguments_cannot_select_a_room_and_backend_receives_exact_scope() {
        let source = Arc::new(RecordingSource::default());
        let handle = AdmittedRoomHistory::from_admission(&TestAdmission, source.clone()).unwrap();
        let tool = handle.tool();
        assert!(tool.parameters()["properties"].get("room_id").is_none());
        assert_eq!(
            tool.execute("forged", json!({"room_id":"room-b"}))
                .await
                .unwrap_err(),
            "invalid room_history arguments"
        );
        assert!(source.calls.lock().unwrap().is_empty());
        for cursor in ["00", "01", "+1", "-1", " 1"] {
            assert_eq!(
                tool.execute("non-canonical", json!({"before_seq":cursor}))
                    .await
                    .unwrap_err(),
                "invalid room_history before_seq"
            );
        }
        assert!(source.calls.lock().unwrap().is_empty());

        let result = tool
            .execute("exact", json!({"before_seq":"42","limit":10}))
            .await
            .unwrap();
        let calls = source.calls.lock().unwrap();
        let (scope, request) = &calls[0];
        assert_eq!(scope.room_key(), "room-a");
        assert_eq!(scope.agent_member_id(), "agent-a");
        assert_eq!(scope.generation(), 7);
        assert_eq!(request.before_seq(), Some(42));
        assert_eq!(request.limit(), 10);
        let text = result.content[0].as_text().unwrap();
        assert!(text.contains("durable Room fact"), "{text}");
        assert!(!text.contains("room-a"), "scope must not echo: {text}");
        assert!(!text.contains("agent-a"), "scope must not echo: {text}");
    }

    #[tokio::test]
    async fn page_limit_is_defaulted_and_clamped_before_backend_call() {
        let source = Arc::new(RecordingSource::default());
        let handle = AdmittedRoomHistory::from_admission(&TestAdmission, source.clone()).unwrap();
        let tool = handle.tool();
        tool.execute("default", json!({})).await.unwrap();
        tool.execute("max", json!({"limit":10000})).await.unwrap();
        let calls = source.calls.lock().unwrap();
        assert_eq!(calls[0].1.limit(), DEFAULT_PAGE_LIMIT);
        assert_eq!(calls[1].1.limit(), MAX_PAGE_LIMIT);
    }

    #[test]
    fn invalid_admission_cannot_mint_a_handle() {
        struct EmptyAdmission;
        impl RoomHistoryAdmission for EmptyAdmission {
            fn admitted_room_key(&self) -> &str {
                ""
            }
            fn admitted_agent_member_id(&self) -> &str {
                "agent-a"
            }
            fn admitted_generation(&self) -> u64 {
                7
            }
        }
        assert!(AdmittedRoomHistory::from_admission(
            &EmptyAdmission,
            Arc::new(RecordingSource::default())
        )
        .is_err());
    }

    #[test]
    fn author_kinds_cover_the_complete_durable_room_vocabulary() {
        assert_eq!(RoomHistoryAuthorKind::Human.as_str(), "human");
        assert_eq!(RoomHistoryAuthorKind::Agent.as_str(), "agent");
        assert_eq!(RoomHistoryAuthorKind::System.as_str(), "system");
        assert_eq!(RoomHistoryAuthorKind::Bot.as_str(), "bot");
        assert_eq!(RoomHistoryAuthorKind::Tool.as_str(), "tool");
        assert_eq!(parse_canonical_seq("0").unwrap(), 0);
        let max = u64::MAX.to_string();
        assert_eq!(parse_canonical_seq(&max).unwrap(), u64::MAX);
    }
}
