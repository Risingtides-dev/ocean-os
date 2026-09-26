//! Stalled-consumer QUEUE measurement for the two per-turn hops, slice 1 of
//! `docs/specs/2026-09-26-bounded-turn-event-channel-proposal.md` (§6, §8).
//!
//! Measurement only. It changes no production behavior and adds no bound: the
//! only production-file hooks are two `#[cfg(test)]` fields on
//! [`crate::AgentRuntime`] (a scripted turn provider and the H1 dequeue probe
//! below), which never exist outside this crate's own test build.
//!
//! ```text
//! cargo test --release -p ocean-agent turn_channel_queue -- --ignored --nocapture --test-threads=1
//! ```
//!
//! **What runs.** The real `AgentRuntime::prompt` → `run_prompt` → spawned
//! `run_agent_with_history` loop, the real H1 `unbounded_channel` and its real
//! consumer (the `run_prompt` receive loop, including the synchronous
//! `TurnCheckpoint` save), the real `bash` tool, and a real H2
//! `unbounded_channel` handed in through `PromptControl::with_event_sink`,
//! exactly as the daemon does. The provider is a scripted, paced stream
//! ([`PacedProvider`]). The H2 consumer is this harness standing in for the
//! daemon bridge: it does not `recv` for a fixed stall, then drains. The
//! bridge's own per-event work (`AgentEventBus::emit`, µs–1.3 ms, M §5) is not
//! modelled; a stall is modelled as the bridge not polling at all.
//!
//! **How a queue is measured, without touching the channel.** Both hops are
//! FIFO. At every dequeue the consumer records `rx.len()` (what is still
//! queued) and the event. The queue just before dequeue *i* therefore held
//! events `i ..= i + len_i`, all of which are dequeued later, so its count and
//! bytes are exact after the turn from prefix sums. The maximum over all
//! dequeues is the peak, because a queue only grows between dequeues. `len()`
//! is read just after the dequeue, so an event sent in that instant is counted
//! early: a peak can be over by an event, never under.
//!
//! **Bytes** use the proposal's own §4 accounting method, the in-memory
//! payload estimate the 8 MiB budget is defined in: string and byte lengths
//! plus a 64 B per-event overhead, with `Value` fields walked recursively.
//! `AgentEvent` has no wire encoding of its own (it is not `Serialize`), so
//! typed payloads (`Message`, `Content`, surface patches, Slack ops) are walked
//! through their `serde_json::Value` form. See [`event_bytes`].

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ocean_protocol::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Content, Context, Model,
    Provider, StopReason, StreamOptions, Usage,
};
use ocean_providers::ProviderId;
use ocean_runtime::AgentEvent;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::session_retained_size_measurements::{checkpoint_round, simulate, TurnShape};
use crate::{
    cap_session_history, compact_history, session, AgentRuntime, PromptControl, PromptRequest,
};

const KIB: usize = 1024;
const MIB: usize = 1024 * 1024;

/// Proposal §4 byte budget and count budget, per hop.
const BUDGET_BYTES: usize = 8 * MIB;
const BUDGET_EVENTS: usize = 1_024;

/// Proposal §4 per-event overhead in the payload estimate.
const EVENT_OVERHEAD_BYTES: usize = 64;

/// Realistic streaming: a 24 B text delta (the measurements doc's synthetic
/// delta) every 5 ms, 4.8 KB/s. That is faster than a real provider streams
/// text, so queues here err on the high side.
const DELTA_BYTES: usize = 24;
const DELTA_GAP: Duration = Duration::from_millis(5);

// ---------------------------------------------------------------------------
// The H1 probe (held by `AgentRuntime` in test builds only).
// ---------------------------------------------------------------------------

/// One H1 dequeue: the queue length left behind, and the event dequeued.
#[derive(Debug)]
pub(crate) struct Dequeue {
    queued_after: usize,
    event: AgentEvent,
}

/// Records every H1 dequeue in `run_prompt`'s receive loop. The clone costs
/// the H1 consumer a memcpy per event, which can only lengthen its stalls, so
/// H1 peaks here err high.
#[derive(Debug, Clone, Default)]
pub(crate) struct H1Probe(Arc<std::sync::Mutex<Vec<Dequeue>>>);

impl H1Probe {
    pub(crate) fn record(&self, queued_after: usize, event: &AgentEvent) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Dequeue {
                queued_after,
                event: event.clone(),
            });
    }

    fn take(&self) -> Vec<Dequeue> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

// ---------------------------------------------------------------------------
// Byte accounting: the proposal's §4 in-memory payload estimate.
// ---------------------------------------------------------------------------

/// Strings (object keys included) count their byte length; every number,
/// bool, and null counts 8 B; arrays and objects count their contents.
fn value_bytes(value: &Value) -> usize {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => 8,
        Value::String(text) => text.len(),
        Value::Array(items) => items.iter().map(value_bytes).sum(),
        Value::Object(map) => map.iter().map(|(k, v)| k.len() + value_bytes(v)).sum(),
    }
}

fn typed_bytes<T: serde::Serialize>(payload: &T) -> usize {
    value_bytes(&serde_json::to_value(payload).expect("payload serializes"))
}

fn opt_len(text: &Option<String>) -> usize {
    text.as_deref().map_or(0, str::len)
}

/// The proposal's accounting for one queued `AgentEvent`. Exhaustive, so a new
/// variant must be sized here.
fn event_bytes(event: &AgentEvent) -> usize {
    EVENT_OVERHEAD_BYTES
        + match event {
            AgentEvent::AgentStart { session_id }
            | AgentEvent::TurnStart { session_id }
            | AgentEvent::TurnEnd { session_id }
            | AgentEvent::BrowserActivity { session_id, .. } => opt_len(session_id),
            AgentEvent::AgentEnd {
                session_id,
                messages,
            }
            | AgentEvent::TurnCheckpoint {
                session_id,
                messages,
            } => opt_len(session_id) + typed_bytes(messages),
            AgentEvent::AssistantMessage {
                session_id,
                message,
            }
            | AgentEvent::UserMessage {
                session_id,
                message,
            } => opt_len(session_id) + typed_bytes(message),
            AgentEvent::TextDelta { session_id, delta }
            | AgentEvent::ThinkingDelta { session_id, delta } => opt_len(session_id) + delta.len(),
            AgentEvent::ToolExecutionStart {
                session_id,
                tool_call_id,
                tool_name,
                args,
            } => opt_len(session_id) + tool_call_id.len() + tool_name.len() + value_bytes(args),
            AgentEvent::ToolExecutionEnd {
                session_id,
                tool_call_id,
                tool_name,
                content,
                details,
                ..
            } => {
                opt_len(session_id)
                    + tool_call_id.len()
                    + tool_name.len()
                    + typed_bytes(content)
                    + value_bytes(details)
            }
            AgentEvent::PermissionDenied {
                session_id,
                tool_name,
                reason,
            } => opt_len(session_id) + tool_name.len() + reason.len(),
            AgentEvent::ModelRerouted {
                session_id,
                requested,
                effective,
                reason,
            } => opt_len(session_id) + requested.len() + effective.len() + reason.len(),
            AgentEvent::ProviderRetrying {
                session_id, reason, ..
            } => opt_len(session_id) + reason.len(),
            AgentEvent::Render {
                session_id,
                id,
                kind,
                props,
                ..
            } => opt_len(session_id) + id.len() + kind.len() + value_bytes(props),
            AgentEvent::Unmount { session_id, id } => opt_len(session_id) + id.len(),
            AgentEvent::SurfacePatch {
                session_id,
                canvas_id,
                patches,
            } => opt_len(session_id) + canvas_id.len() + typed_bytes(patches),
            AgentEvent::SlackCanvas { session_id, op } => opt_len(session_id) + typed_bytes(op),
        }
}

/// Peak queue on one hop, reconstructed from its dequeue log.
#[derive(Debug, Default, Clone, Copy)]
struct HopPeak {
    peak_events: usize,
    peak_bytes: usize,
    /// Every event that crossed the hop during the turn.
    total_events: usize,
    total_bytes: usize,
    largest_event: usize,
    /// The largest queue found at a dequeue that directly follows a
    /// `TurnCheckpoint` dequeue: the backlog one synchronous checkpoint save
    /// left behind. H1 only; zero elsewhere.
    after_save_events: usize,
    after_save_bytes: usize,
}

/// `after_save[i]` marks dequeue `i` as directly following a checkpoint save.
fn hop_peak_marked(log: &[(usize, usize)], after_save: &[bool]) -> HopPeak {
    let mut peak = hop_peak(log);
    let mut prefix = vec![0usize];
    for (_, bytes) in log {
        prefix.push(prefix.last().unwrap() + bytes);
    }
    for (i, (queued_after, _)) in log.iter().enumerate() {
        if after_save.get(i).copied().unwrap_or(false) {
            let end = (i + queued_after + 1).min(log.len());
            peak.after_save_events = peak.after_save_events.max(end - i);
            peak.after_save_bytes = peak.after_save_bytes.max(prefix[end] - prefix[i]);
        }
    }
    peak
}

fn hop_peak(log: &[(usize, usize)]) -> HopPeak {
    let mut prefix = Vec::with_capacity(log.len() + 1);
    prefix.push(0usize);
    for (_, bytes) in log {
        prefix.push(prefix.last().unwrap() + bytes);
    }
    let mut peak = HopPeak {
        total_events: log.len(),
        total_bytes: *prefix.last().unwrap(),
        largest_event: log.iter().map(|(_, bytes)| *bytes).max().unwrap_or(0),
        ..HopPeak::default()
    };
    for (i, (queued_after, _)) in log.iter().enumerate() {
        let end = (i + queued_after + 1).min(log.len());
        peak.peak_events = peak.peak_events.max(end - i);
        peak.peak_bytes = peak.peak_bytes.max(prefix[end] - prefix[i]);
    }
    peak
}

fn sized(log: Vec<(usize, AgentEvent)>) -> Vec<(usize, usize)> {
    log.into_iter()
        .map(|(queued_after, event)| (queued_after, event_bytes(&event)))
        .collect()
}

// ---------------------------------------------------------------------------
// The scripted, paced provider.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Round {
    deltas: usize,
    gap: Duration,
    /// Bash commands the round ends with (issued together, as one assistant
    /// message). Empty ends the turn.
    bash: Vec<String>,
}

/// Streams each scripted round as `deltas` text deltas of [`DELTA_BYTES`], one
/// every `gap`, then the terminal `Done`. A round past the script ends the turn
/// with no text.
struct PacedProvider {
    rounds: Vec<Round>,
    calls: AtomicUsize,
}

impl PacedProvider {
    fn new(rounds: Vec<Round>) -> Self {
        Self {
            rounds,
            calls: AtomicUsize::new(0),
        }
    }
}

fn paced_message(content: Vec<Content>, stop: StopReason) -> AssistantMessage {
    AssistantMessage {
        content,
        api: "fake".into(),
        provider: "fake".into(),
        model: ocean_runtime::FAKE_TOOL_MODEL.into(),
        usage: Usage::default(),
        stop_reason: stop,
        error_message: None,
        timestamp: ocean_protocol::now_ms(),
    }
}

#[async_trait]
impl Provider for PacedProvider {
    async fn stream(
        &self,
        _model: &Model,
        _context: &Context,
        _options: &StreamOptions,
    ) -> ocean_protocol::Result<AssistantMessageEventStream> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let round = self.rounds.get(index).cloned().unwrap_or(Round {
            deltas: 0,
            gap: Duration::ZERO,
            bash: Vec::new(),
        });
        let mut content = Vec::new();
        if round.deltas > 0 {
            content.push(Content::text("t".repeat(round.deltas * DELTA_BYTES)));
        }
        for (k, command) in round.bash.iter().enumerate() {
            content.push(Content::ToolCall {
                id: format!("call_{index:03}_{k}"),
                name: "bash".into(),
                arguments: json!({ "command": command }),
            });
        }
        let stop = if round.bash.is_empty() {
            StopReason::Stop
        } else {
            StopReason::ToolUse
        };
        let done = AssistantMessageEvent::Done {
            reason: stop,
            message: paced_message(content, stop),
        };
        let stream = futures::stream::unfold(
            (0usize, round.deltas, round.gap, Some(done)),
            |(sent, deltas, gap, mut done)| async move {
                if sent < deltas {
                    if !gap.is_zero() {
                        tokio::time::sleep(gap).await;
                    }
                    let delta = AssistantMessageEvent::TextDelta {
                        content_index: 0,
                        delta: "t".repeat(DELTA_BYTES),
                    };
                    Some((Ok(delta), (sent + 1, deltas, gap, done)))
                } else {
                    done.take()
                        .map(|done| (Ok(done), (sent + 1, deltas, gap, None)))
                }
            },
        );
        Ok(Box::pin(stream))
    }
}

/// A bash command that prints exactly `bytes` bytes of `x`.
fn print_bytes(bytes: usize) -> String {
    format!("head -c {bytes} /dev/zero | tr '\\000' x")
}

// ---------------------------------------------------------------------------
// One measured turn.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum H2Stall {
    None,
    For(Duration),
    /// The bridge does not poll until the turn has returned: the whole relayed
    /// turn queues on H2. The ceiling for any stall.
    WholeTurn,
}

impl H2Stall {
    fn label(self) -> String {
        match self {
            H2Stall::None => "none".into(),
            H2Stall::For(d) => format!("{} ms", d.as_millis()),
            H2Stall::WholeTurn => "whole turn".into(),
        }
    }
}

struct TurnSpec {
    scenario: &'static str,
    rounds: Vec<Round>,
    h2_stall: H2Stall,
    /// Preloaded steady-state transcript: (context window, simulated turns).
    preload: Option<(u32, usize)>,
}

struct TurnResult {
    scenario: &'static str,
    h2_stall: String,
    context_window: u32,
    /// The preloaded session file before and after the turn (0 when none).
    session_file_bytes: (u64, u64),
    h1: HopPeak,
    h2: HopPeak,
    wall: Duration,
}

fn provider_config(context_window: u32) -> ocean_providers::ProviderConfig {
    ocean_providers::ProviderConfig {
        selection: ocean_providers::ModelSelection {
            provider: ProviderId::Fake,
            model: ocean_runtime::FAKE_TOOL_MODEL.into(),
            base_url: "fake://local".into(),
            context_window,
            max_output_tokens: 16_000,
        },
        credential: None,
        account_id: None,
    }
}

async fn run_turn(spec: TurnSpec) -> TurnResult {
    let context_window = spec.preload.map_or(200_000, |(window, _)| window);
    let config_dir = std::env::temp_dir().join(format!(
        "ocean-agent-queuemeas-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let state = crate::state_from_provider_config(provider_config(context_window)).unwrap();
    let probe = H1Probe::default();
    let runtime = AgentRuntime {
        config_dir: config_dir.clone(),
        state: Arc::new(std::sync::RwLock::new(state)),
        capabilities: Arc::new(crate::CapabilityRegistry::builtin_only()),
        memory_factory: None,
        session_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        hooks: ocean_hooks::HooksConfig::default(),
        provider_quarantine: Arc::new(ocean_providers::ProviderQuarantine::default()),
        test_env: None,
        test_compact_provider: None,
        test_dispatch_status: std::collections::HashMap::new(),
        test_turn_provider: Some(crate::TestCompactProvider(Arc::new(PacedProvider::new(
            spec.rounds,
        )))),
        test_h1_probe: Some(probe.clone()),
    };

    // Scenario 2 resumes a session already at the proposal's file size, so
    // every TurnCheckpoint save writes a file that large.
    let preloaded = spec.preload.map(|(window, turns)| {
        // Exactly the file `checkpoint_save_stall_at_steady_state` saves: the
        // steady-state transcript, compacted as at turn start, plus one round.
        let shape = TurnShape::with_output(32 * KIB);
        let mut base = simulate(shape, turns, window, false).final_messages;
        compact_history(&mut base, window);
        base.extend(checkpoint_round(shape, 0));
        let base = cap_session_history(base);
        let id = ocean_core::SessionId::new_v4();
        let mut stored = session::Session::new_with_id(id, &runtime.snapshot().model);
        stored.replace_messages(base);
        // Bound to the turn's cwd up front, so the turn's saves rewrite this
        // same file instead of migrating it.
        stored.bind_workspace(&config_dir);
        let path = session::save(&config_dir, &stored).expect("preload save");
        let bytes = std::fs::metadata(&path).expect("preload file").len();
        (id, path, bytes)
    });
    let session_id = preloaded.as_ref().map(|(id, _, _)| *id);

    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
    let (turn_done_tx, turn_done_rx) = tokio::sync::oneshot::channel::<()>();
    let stall = spec.h2_stall;
    let bridge = tokio::spawn(async move {
        match stall {
            H2Stall::None => {}
            H2Stall::For(d) => tokio::time::sleep(d).await,
            H2Stall::WholeTurn => {
                let _ = turn_done_rx.await;
            }
        }
        let mut log = Vec::new();
        while let Some(event) = rx.recv().await {
            log.push((rx.len(), event));
        }
        log
    });

    let started = std::time::Instant::now();
    let res = runtime
        .prompt(
            PromptRequest {
                prompt: "measure the turn channel".into(),
                images: None,
                request_id: None,
                session_id,
                create_if_missing: session_id.is_none(),
                max_turns: None,
                yolo: true,
                cwd: config_dir.to_string_lossy().into_owned(),
                project_id: None,
                client_type: None,
                decision_token: None,
            },
            // Voice-profile shape: artifact spill OFF (the `PromptControl`
            // default), so live tool output is not replaced by a 16 KB head.
            PromptControl::yolo(true).with_event_sink(tx),
        )
        .await;
    let wall = started.elapsed();
    let _ = turn_done_tx.send(());
    assert!(res.ok, "measured turn failed: {}", res.stderr);
    let h2_log = bridge.await.expect("bridge task");

    // The file every checkpoint save of the preloaded session rewrote.
    let session_file_bytes = preloaded.as_ref().map_or((0, 0), |(_, path, before)| {
        (
            *before,
            std::fs::metadata(path).expect("session file").len(),
        )
    });

    let h1_log: Vec<(usize, AgentEvent)> = probe
        .take()
        .into_iter()
        .map(|d| (d.queued_after, d.event))
        .collect();
    // Sanity: H2 carries exactly the relayed subsequence of H1, in order.
    let relayed_on_h1 = h1_log.iter().filter(|(_, e)| e.is_wire_relayed()).count();
    assert_eq!(relayed_on_h1, h2_log.len(), "H2 lost or gained events");

    let after_save: Vec<bool> = std::iter::once(false)
        .chain(
            h1_log
                .iter()
                .map(|(_, e)| matches!(e, AgentEvent::TurnCheckpoint { .. })),
        )
        .collect();

    let _ = std::fs::remove_dir_all(&config_dir);
    TurnResult {
        scenario: spec.scenario,
        h2_stall: spec.h2_stall.label(),
        context_window,
        session_file_bytes,
        h1: hop_peak_marked(&sized(h1_log), &after_save),
        h2: hop_peak(&sized(h2_log)),
        wall,
    }
}

fn fmt_bytes(bytes: usize) -> String {
    if bytes >= MIB {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn binds(peak: &HopPeak) -> String {
    match (
        peak.peak_bytes > BUDGET_BYTES,
        peak.peak_events > BUDGET_EVENTS,
    ) {
        (false, false) => "no".into(),
        (true, false) => format!("**bytes** ({} B)", peak.peak_bytes),
        (false, true) => "**count**".into(),
        (true, true) => format!("**both** ({} B)", peak.peak_bytes),
    }
}

fn print_results(results: &[TurnResult]) {
    println!("\nPer-turn queue peaks (proposal §4 byte estimate; budget 8 MiB / 1,024 per hop)");
    println!(
        "| scenario | H2 stall | session file | turn wall | H1 peak events | H1 peak bytes | H1 after a save | H1 binds? | H2 peak events | H2 peak bytes | H2 binds? | H2 turn total | largest event |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    for r in results {
        println!(
            "| {} | {} | {} | {:.2} s | {} | {} | {} ev / {} | {} | {} | {} | {} | {} ev / {} | {} |",
            r.scenario,
            r.h2_stall,
            if r.session_file_bytes.0 == 0 {
                "–".to_string()
            } else {
                format!(
                    "{} → {} B ({}k window)",
                    r.session_file_bytes.0,
                    r.session_file_bytes.1,
                    r.context_window / 1000
                )
            },
            r.wall.as_secs_f64(),
            r.h1.peak_events,
            fmt_bytes(r.h1.peak_bytes),
            r.h1.after_save_events,
            fmt_bytes(r.h1.after_save_bytes),
            binds(&r.h1),
            r.h2.peak_events,
            fmt_bytes(r.h2.peak_bytes),
            binds(&r.h2),
            r.h2.total_events,
            fmt_bytes(r.h2.total_bytes),
            fmt_bytes(r.h1.largest_event.max(r.h2.largest_event)),
        );
    }
}

// ---------------------------------------------------------------------------
// Scenarios.
// ---------------------------------------------------------------------------

/// Scenario 1: four rounds of 100 realistic deltas (24 B / 5 ms), each of the
/// first three ending in a small bash call, so ~2 s of streaming.
fn streaming_rounds() -> Vec<Round> {
    let mut rounds: Vec<Round> = (0..3)
        .map(|_| Round {
            deltas: 100,
            gap: DELTA_GAP,
            bash: vec!["printf ok".into()],
        })
        .collect();
    rounds.push(Round {
        deltas: 100,
        gap: DELTA_GAP,
        bash: Vec::new(),
    });
    rounds
}

/// Scenario 2: eight rounds of 50 deltas, each ending in a small bash call, so
/// the H1 consumer runs eight `TurnCheckpoint` saves at the preloaded size
/// while the next round streams. The rounds are small on purpose: a 32 KiB
/// round would push the file past the size point (older results are already
/// elided), and a small one keeps every save within ~1 % of it.
fn checkpoint_rounds(gap: Duration) -> Vec<Round> {
    let mut rounds: Vec<Round> = (0..8)
        .map(|_| Round {
            deltas: 50,
            gap,
            bash: vec!["printf ok".into()],
        })
        .collect();
    rounds.push(Round {
        deltas: 50,
        gap,
        bash: Vec::new(),
    });
    rounds
}

/// Scenario 3: 20 deltas, then `tools` bash calls that each print 2 MiB (the
/// bash capture cap) in one assistant message, then 20 closing deltas.
fn large_output_rounds(tools: usize) -> Vec<Round> {
    vec![
        Round {
            deltas: 20,
            gap: DELTA_GAP,
            bash: (0..tools).map(|_| print_bytes(2 * MIB)).collect(),
        },
        Round {
            deltas: 20,
            gap: DELTA_GAP,
            bash: Vec::new(),
        },
    ]
}

/// Slice 1 of the bounded turn event channel proposal: peak queued events and
/// bytes on H1 and H2 for scripted turns with a stalled consumer. Timing- and
/// machine-dependent, so `#[ignore]`d; run in release for the recorded figures:
///
/// ```text
/// cargo test --release -p ocean-agent turn_channel_queue -- --ignored --nocapture --test-threads=1
/// ```
///
/// A multi-thread runtime, as in the daemon: the H1 consumer's synchronous
/// save blocks its own worker while the agent loop keeps producing on another.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "timing measurement (~30 s of paced turns); run with --release --ignored --nocapture"]
async fn turn_channel_queue_peaks_under_stalled_consumers() {
    let mut results = Vec::new();

    // 1. Stall the H2 consumer (the daemon bridge) while realistic deltas stream.
    for stall in [
        H2Stall::None,
        H2Stall::For(Duration::from_millis(100)),
        H2Stall::For(Duration::from_secs(1)),
        H2Stall::WholeTurn,
    ] {
        results.push(
            run_turn(TurnSpec {
                scenario: "1 deltas, H2 stalled",
                rounds: streaming_rounds(),
                h2_stall: stall,
                preload: None,
            })
            .await,
        );
    }

    // 2. Stall the H1 consumer through real TurnCheckpoint saves at the
    //    proposal's two file sizes (539,534 B and 1,029,539 B), at the
    //    realistic delta rate and at a 1 ms burst rate.
    for (window, turns) in [(300_000, 21), (600_000, 23)] {
        for (label, gap) in [
            ("2 checkpoint save, 5 ms deltas", DELTA_GAP),
            ("2 checkpoint save, 1 ms deltas", Duration::from_millis(1)),
        ] {
            results.push(
                run_turn(TurnSpec {
                    scenario: label,
                    rounds: checkpoint_rounds(gap),
                    h2_stall: H2Stall::None,
                    preload: Some((window, turns)),
                })
                .await,
            );
        }
    }

    // 3. A 2 MiB bash result (and four in one segment), spill off.
    for (label, tools) in [("3 one 2 MiB bash", 1), ("3 four 2 MiB bash", 4)] {
        for stall in [
            H2Stall::None,
            H2Stall::For(Duration::from_millis(100)),
            H2Stall::For(Duration::from_secs(1)),
            H2Stall::WholeTurn,
        ] {
            results.push(
                run_turn(TurnSpec {
                    scenario: label,
                    rounds: large_output_rounds(tools),
                    h2_stall: stall,
                    preload: None,
                })
                .await,
            );
        }
    }

    print_results(&results);

    // Invariants the table relies on.
    for r in &results {
        assert!(r.h2.peak_events >= 1 && r.h1.peak_events >= 1);
        if r.h2_stall == "whole turn" {
            // A bridge that never polled holds the whole relayed turn.
            assert_eq!(r.h2.peak_events, r.h2.total_events, "{}", r.scenario);
            assert_eq!(r.h2.peak_bytes, r.h2.total_bytes, "{}", r.scenario);
        }
        if r.scenario.starts_with("3 ") {
            // Spill off: the live ToolExecutionEnd carries the whole 2 MiB.
            assert!(r.h2.largest_event >= 2 * MIB, "{}", r.scenario);
        }
        if let Some(expected) = match r.context_window {
            300_000 => Some(539_534u64),
            600_000 => Some(1_029_539u64),
            _ => None,
        } {
            // The preload is the proposal's size (±1 %), and so is the file
            // after the turn's last checkpoint save (±2 %).
            let (before, after) = r.session_file_bytes;
            assert!(
                before.abs_diff(expected) * 100 <= expected,
                "preloaded file {before} B, expected about {expected} B"
            );
            assert!(
                after.abs_diff(expected) * 50 <= expected,
                "session file after the turn {after} B, expected about {expected} B"
            );
        }
    }
}

#[test]
fn hop_peak_reconstructs_the_queue_from_dequeue_lengths() {
    // Dequeue 0 left 2 behind (queue held 0,1,2), dequeue 1 left 1 (1,2),
    // dequeue 2 left 1 (2,3: event 3 arrived meanwhile), dequeue 3 left 0.
    let log = [(2, 10), (1, 20), (1, 30), (0, 40)];
    let peak = hop_peak(&log);
    assert_eq!(peak.peak_events, 3);
    assert_eq!(peak.peak_bytes, 70); // max(10+20+30, 20+30, 30+40, 40)
    assert_eq!(peak.total_events, 4);
    assert_eq!(peak.total_bytes, 100);
    assert_eq!(peak.largest_event, 40);
    // Only dequeue 2 follows a save: its queue held events 2 and 3.
    let marked = hop_peak_marked(&log, &[false, false, true, false]);
    assert_eq!((marked.after_save_events, marked.after_save_bytes), (2, 70));
}

#[test]
fn event_bytes_follows_the_proposal_estimate() {
    let delta = AgentEvent::TextDelta {
        session_id: Some("s".repeat(36)),
        delta: "t".repeat(24),
    };
    assert_eq!(event_bytes(&delta), 64 + 36 + 24);
    let args = json!({ "command": "ls", "n": 1 });
    let start = AgentEvent::ToolExecutionStart {
        session_id: None,
        tool_call_id: "c".into(),
        tool_name: "bash".into(),
        args,
    };
    // keys "command"(7) + "ls"(2) + "n"(1) + number(8)
    assert_eq!(event_bytes(&start), 64 + 1 + 4 + 7 + 2 + 1 + 8);
}
