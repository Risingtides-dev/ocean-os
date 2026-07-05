//! `LonghouseProvider` — adapts the daemon-owned Longhouse council machinery to
//! the runtime's [`CapabilityProvider`] seam, so **agents** (not just the
//! operator over HTTP) can convene councils and read the board as tools.
//!
//! Mirrors `ocean-mcp`'s `McpProvider` and `ocean-plugin`'s `PluginProvider`:
//! this crate already owns the engine/registry, depends *up* on `ocean-runtime`
//! for the trait, and exposes its ops as namespaced [`AgentTool`]s. Nothing in
//! `ocean-runtime` depends back on this crate, so there is no cycle.
//!
//! ## What it exposes
//!
//! Tools are namespaced `longhouse__<op>` so they can never collide with a
//! built-in or another provider:
//!
//! * **`longhouse__board_read`** — read the durable [`LonghouseRegistry`]: list
//!   every tracked topic, or fetch one topic's full snapshot by id. Pure, no LLM.
//! * **`longhouse__convene`** — staff a real cheap-model council on a question,
//!   run the propose → endorse/inhibit rounds, let the daemon-side
//!   [`QuorumEngine`](crate::quorum::QuorumEngine) decide convergence, and tee
//!   every emitted `LonghouseEvent` into the **same** registry the HTTP routes
//!   serve (so the topic survives and `board_read` can see it afterward).
//!
//! ## What it deliberately does NOT expose (yet)
//!
//! * `board_post` (posting a single mark) and `claim_outcome` operate on the
//!   *ephemeral* `QuorumEngine` that `convene()` owns for the duration of one
//!   council — there is no persisted, daemon-held engine to post a lone mark to
//!   or to claim an outcome against between turns. Marks are produced by the
//!   council's LLM workers inside `convene`, not posted ad hoc. Exposing those
//!   as standalone tools would require a persisted engine on shared state, which
//!   does not exist today; we wire the tractable ops rather than fabricate an
//!   API. See the module-level note in `convene.rs`.
//!
//! Like MCP/plugin tools, every Longhouse tool reports
//! `requires_permission() == true` so it is gated by the daemon's
//! `PermissionPolicy` exactly like `bash`/`write`/`edit` — agents never bypass
//! the gate (post-OCEAN-54).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ocean_runtime::capability::{CapabilityProvider, ProviderHealth, SessionContext, SharedTool};
use ocean_runtime::types::{AgentTool, AgentToolResult};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::convene::{convene, ConveneRequest, SystemClock};
use crate::registry::LonghouseRegistry;
use ocean_agent_sdk::Federation;

/// Shared handle to the daemon's read-side topic registry. The daemon holds the
/// authoritative `Arc<Mutex<LonghouseRegistry>>` on `AppState` and serves the
/// `GET /v1/longhouse/topics*` routes off it; the provider holds a **clone of
/// the same Arc** so a council convened via the tool, and the topics an agent
/// reads, are the very same registry the operator's HTTP surface sees.
pub type LonghouseRegistryHandle = Arc<Mutex<LonghouseRegistry>>;

/// A capability provider backed by the Longhouse council machinery.
pub struct LonghouseProvider {
    id: String,
    tools: Vec<SharedTool>,
}

impl LonghouseProvider {
    /// Build the provider over a shared registry handle. The handle is the same
    /// `Arc` the daemon serves its HTTP topic routes off, so tool-driven
    /// councils and operator-driven councils share one observable board.
    pub fn new(registry: LonghouseRegistryHandle) -> Self {
        let tools: Vec<SharedTool> = vec![
            Arc::new(BoardReadTool {
                registry: registry.clone(),
            }),
            Arc::new(ConveneTool { registry }),
        ];
        Self {
            id: "longhouse".to_string(),
            tools,
        }
    }
}

#[async_trait]
impl CapabilityProvider for LonghouseProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn tools(&self, _ctx: &SessionContext) -> Vec<SharedTool> {
        // Cheap Arc clones of a fixed, in-process toolset — no network/handshake,
        // so this is safe to call once per turn.
        self.tools.clone()
    }

    async fn health(&self) -> ProviderHealth {
        // Always serviceable: the registry is in-process and the convene path is
        // best-effort (a council with no resolvable models aborts cleanly).
        ProviderHealth::Ready
    }
}

/// `longhouse__board_read` — read the durable topic registry. With no args (or
/// `topic_id` omitted/null) it lists every tracked topic, newest first. With a
/// `topic_id`, it returns that one topic's full snapshot, or a typed not-found.
struct BoardReadTool {
    registry: LonghouseRegistryHandle,
}

#[async_trait]
impl AgentTool for BoardReadTool {
    fn name(&self) -> &str {
        "longhouse__board_read"
    }

    fn description(&self) -> &str {
        "Read the Longhouse board: list every tracked council topic (members, \
         marks, tallies, leader, lifecycle state), or fetch one topic's full \
         snapshot by its `topic_id`. Read-only — never decides quorum."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "topic_id": {
                    "type": "string",
                    "description": "Optional topic UUID. Omit to list all topics; \
                                    provide it to read one topic's snapshot."
                }
            },
            "additionalProperties": false
        })
    }

    fn requires_permission(&self) -> bool {
        true
    }

    async fn execute(&self, _tool_call_id: &str, args: Value) -> Result<AgentToolResult, String> {
        let reg = self
            .registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // A present, non-null `topic_id` selects a single topic.
        let topic_arg = args.get("topic_id").and_then(|v| v.as_str());
        if let Some(raw) = topic_arg {
            let id = Uuid::parse_str(raw.trim())
                .map_err(|_| format!("`topic_id` is not a valid UUID: {raw:?}"))?;
            return match reg.topic(&id) {
                Some(snapshot) => {
                    let body = json!({ "ok": true, "topic": snapshot });
                    Ok(AgentToolResult::text(pretty(&body)))
                }
                None => {
                    let body = json!({
                        "ok": false,
                        "error": format!("no longhouse topic with id '{id}'"),
                    });
                    Ok(AgentToolResult::text(pretty(&body)))
                }
            };
        }

        let topics = reg.topics();
        let body = json!({ "ok": true, "count": topics.len(), "topics": topics });
        Ok(AgentToolResult::text(pretty(&body)))
    }
}

/// `longhouse__convene` — staff and run a real cheap-model council on a
/// question. Streams nothing back to the agent directly (events go to the
/// registry, observable via `longhouse__board_read` and the deck); returns the
/// final outcome: the topic id, whether it converged, and the proposal texts.
struct ConveneTool {
    registry: LonghouseRegistryHandle,
}

#[async_trait]
impl AgentTool for ConveneTool {
    fn name(&self) -> &str {
        "longhouse__convene"
    }

    fn description(&self) -> &str {
        "Convene a Longhouse council on a question: staff cheap-model workers, \
         run propose -> endorse/inhibit rounds, and let the daemon's quorum \
         engine decide convergence. The deliberation is recorded to the board \
         (read it back with longhouse__board_read). Returns the topic id, \
         whether it converged, and the candidate proposals."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question or task the council deliberates."
                },
                "federation": {
                    "type": "string",
                    "description": "Which department room hosts it.",
                    "enum": ["dev", "sales", "content", "campaign", "commons"]
                },
                "models": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional model aliases; one worker per alias. \
                                    Defaults to a mixed deepseek + kimi council."
                }
            },
            "required": ["question"],
            "additionalProperties": false
        })
    }

    fn requires_permission(&self) -> bool {
        true
    }

    async fn execute(&self, _tool_call_id: &str, args: Value) -> Result<AgentToolResult, String> {
        let question = args
            .get("question")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "`question` is required and must be a non-empty string".to_string())?;

        let federation = parse_federation(args.get("federation").and_then(|v| v.as_str()));

        let mut req = ConveneRequest::new(question, federation);
        if let Some(models) = args.get("models").and_then(|v| v.as_array()) {
            let parsed: Vec<String> = models
                .iter()
                .filter_map(|m| m.as_str().map(|s| s.to_string()))
                .collect();
            if !parsed.is_empty() {
                req.models = parsed;
            }
        }

        // Run the council, teeing every emitted event into the shared registry —
        // exactly as the daemon's `longhouse_convene` HTTP handler does, minus
        // the live SSE bus (the agent observes the result via the return value
        // and `board_read`). The closure is fully synchronous, so the std Mutex
        // guard is never held across an await.
        let clock = SystemClock;
        let registry = self.registry.clone();
        let outcome = convene(req, &clock, |ev| {
            let mut reg = registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            reg.ingest(&ev);
        })
        .await;

        let proposals: serde_json::Map<String, Value> = outcome
            .proposals
            .iter()
            .map(|(id, text)| (id.to_string(), Value::String(text.clone())))
            .collect();

        let body = json!({
            "ok": true,
            "topic_id": outcome.topic_id,
            "board_id": outcome.board_id,
            "converged": outcome.decision.is_some(),
            "decision": outcome.decision,
            "tallies": outcome.tallies,
            "proposals": proposals,
        });
        Ok(AgentToolResult::text(pretty(&body)))
    }
}

/// Map a federation string to the `Federation` enum, defaulting to `Commons`.
/// Mirrors the daemon's `parse_federation` so tool and HTTP callers agree.
fn parse_federation(s: Option<&str>) -> Federation {
    match s.map(|v| v.trim().to_lowercase()).as_deref() {
        Some("dev") => Federation::Dev,
        Some("sales") => Federation::Sales,
        Some("content") => Federation::Content,
        Some("campaign") => Federation::Campaign,
        _ => Federation::Commons,
    }
}

/// Render a JSON body as pretty text for the tool result, falling back to a
/// compact string if (impossibly) serialization fails.
fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocean_agent_sdk::{
        AgentRole, ConveneTrigger, Federation, LonghouseEvent, LonghouseMember, Mark, MarkKind,
    };
    use ocean_runtime::capability::{CapabilityRegistry, SessionContext};
    use std::path::PathBuf;

    fn uid(n: u8) -> Uuid {
        let mut b = [0u8; 16];
        b[15] = n;
        Uuid::from_bytes(b)
    }

    /// Flatten a tool result's text content for assertions.
    fn result_text(r: &AgentToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| c.as_text())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn ctx() -> SessionContext {
        SessionContext {
            cwd: PathBuf::from("/tmp"),
            session_id: Some("test".into()),
            hashline: false,
            artifacts: false,
        }
    }

    /// Seed one topic into a fresh registry so board_read has something to find.
    fn seeded_registry() -> (LonghouseRegistryHandle, Uuid) {
        let mut reg = LonghouseRegistry::new();
        let topic = uid(1);
        let author = uid(10);
        let proposal = uid(50);
        reg.ingest(&LonghouseEvent::TopicConvened {
            topic_id: topic,
            board_id: uid(200),
            federation: Federation::Dev,
            trigger: ConveneTrigger::UserRequest,
            title: "seeded topic".into(),
            deadline_ms: 120_000,
        });
        reg.ingest(&LonghouseEvent::Convened {
            topic_id: topic,
            members: vec![LonghouseMember {
                agent_id: author,
                federation: Federation::Dev,
                role: AgentRole::Courier,
                model: "deepseek-v4-flash".into(),
                label: Some("Dev Courier".into()),
            }],
        });
        reg.ingest(&LonghouseEvent::MarkPosted {
            topic_id: topic,
            mark: Mark {
                mark_id: proposal,
                author,
                kind: MarkKind::Proposal,
                target: None,
                summary: "do the thing".into(),
            },
        });
        (Arc::new(Mutex::new(reg)), topic)
    }

    /// The registry the provider exposes carries the namespaced longhouse tools.
    #[tokio::test]
    async fn provider_lists_namespaced_longhouse_tools() {
        let (reg, _topic) = seeded_registry();
        let provider = LonghouseProvider::new(reg);
        let names: Vec<String> = provider
            .tools(&ctx())
            .await
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(
            names.contains(&"longhouse__board_read".to_string()),
            "board_read missing, got {names:?}"
        );
        assert!(
            names.contains(&"longhouse__convene".to_string()),
            "convene missing, got {names:?}"
        );
        // Every Longhouse tool is permission-gated like bash/write/edit.
        for tool in provider.tools(&ctx()).await {
            assert!(
                tool.requires_permission(),
                "{} must require permission",
                tool.name()
            );
        }
    }

    /// Building the registry with the provider surfaces the longhouse__* tools
    /// alongside the built-ins — the wiring `build_capability_registry` does.
    #[tokio::test]
    async fn registry_with_longhouse_exposes_tools() {
        use ocean_runtime::capability::BuiltinProvider;
        let (reg, _topic) = seeded_registry();
        let registry = CapabilityRegistry::new(vec![
            Arc::new(BuiltinProvider::new()),
            Arc::new(LonghouseProvider::new(reg)),
        ]);
        let names: Vec<String> = registry
            .tools_for_session(&ctx())
            .await
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.contains(&"longhouse__board_read".to_string()));
        assert!(names.contains(&"longhouse__convene".to_string()));
        // Built-ins still present (and first).
        assert!(names.iter().any(|n| n == "bash"));
    }

    /// board_read round-trips through the engine's registry: invoking the tool
    /// returns the seeded topic, by id and in the list.
    #[tokio::test]
    async fn board_read_round_trips_through_registry() {
        let (reg, topic) = seeded_registry();
        let provider = LonghouseProvider::new(reg);
        let board_read = provider
            .tools(&ctx())
            .await
            .into_iter()
            .find(|t| t.name() == "longhouse__board_read")
            .expect("board_read present");

        // List all — the seeded topic shows up.
        let listed = board_read
            .execute("c1", json!({}))
            .await
            .expect("list succeeds");
        let listed_text = result_text(&listed);
        assert!(listed_text.contains("seeded topic"), "got: {listed_text}");
        assert!(
            listed_text.contains(&topic.to_string()),
            "got: {listed_text}"
        );

        // Read one by id — same topic, by snapshot.
        let one = board_read
            .execute("c2", json!({ "topic_id": topic.to_string() }))
            .await
            .expect("read-by-id succeeds");
        let one_text = result_text(&one);
        assert!(one_text.contains("seeded topic"), "got: {one_text}");
        assert!(one_text.contains("\"ok\": true"), "got: {one_text}");

        // Unknown id — typed not-found, not an error.
        let missing = board_read
            .execute("c3", json!({ "topic_id": uid(99).to_string() }))
            .await
            .expect("unknown id is a soft miss, not a hard error");
        assert!(result_text(&missing).contains("no longhouse topic"));
    }

    /// convene with an unresolvable model aborts cleanly (no credential, no LLM
    /// call) yet still lands the topic in the SHARED registry — proving the tool
    /// drives the same engine + registry the HTTP route does, and that a
    /// convened topic is then visible via board_read.
    #[tokio::test]
    async fn convene_tool_lands_topic_in_shared_registry() {
        let reg: LonghouseRegistryHandle = Arc::new(Mutex::new(LonghouseRegistry::new()));
        let provider = LonghouseProvider::new(reg.clone());
        let tools = provider.tools(&ctx()).await;
        let convene_tool = tools
            .iter()
            .find(|t| t.name() == "longhouse__convene")
            .expect("convene present");

        // A model alias that won't resolve to a credential → council aborts
        // cleanly without ever calling an LLM, but still emits TopicConvened →
        // the registry records the topic.
        let out = convene_tool
            .execute(
                "c1",
                json!({
                    "question": "should we ship the thing?",
                    "federation": "dev",
                    "models": ["definitely-not-a-real-model-xyz"]
                }),
            )
            .await
            .expect("convene returns an outcome even with no resolvable models");
        let out_text = result_text(&out);
        assert!(out_text.contains("\"ok\": true"), "got: {out_text}");

        // The topic now lives in the shared registry — read it back through the
        // board_read tool to prove the round-trip end to end.
        let board_read = tools
            .iter()
            .find(|t| t.name() == "longhouse__board_read")
            .unwrap();
        let listed = board_read.execute("c2", json!({})).await.unwrap();
        let listed_text = result_text(&listed);
        assert!(
            listed_text.contains("should we ship the thing?"),
            "convened topic should appear on the board: {listed_text}"
        );
        assert_eq!(reg.lock().unwrap().len(), 1, "exactly one topic recorded");
    }
}
