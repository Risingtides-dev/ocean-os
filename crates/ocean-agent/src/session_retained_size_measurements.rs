//! Retained-size MEASUREMENTS for persisted session transcripts (ROADMAP
//! "Reliability and scale").
//!
//! Measurement only: this module calls the real persistence-shaping functions
//! (`compact_history`, `cap_session_history`, `session::save`) on a synthetic
//! workload and reports the serialized bytes a session holds. It changes no
//! bound. Results are recorded in
//! `docs/specs/2026-09-25-retained-size-and-slow-client-measurements.md`.
//!
//! ```text
//! cargo test -p ocean-agent session_retained_size_measurements -- --nocapture --test-threads=1
//! cargo test -p ocean-agent session_retained_size_measurements -- --ignored --nocapture --test-threads=1
//! ```
//!
//! A session is NOT held in daemon memory between turns: each turn loads the
//! file, mutates it, and saves the whole file (pretty JSON) at the accepted-user
//! boundary, at every provider-valid round checkpoint, and at turn end. So the
//! "retained" figure is the file size, which is also the per-turn resident peak
//! of the deserialized session and the unit of every save's write volume.

use ocean_protocol::{AssistantMessage, Content, Message, Model, StopReason, ToolResultMessage};

use crate::{cap_session_history, compact_history, session};

/// Mirror of `ocean-runtime`'s private `MAX_TOOL_RESULT_BYTES` (agent_loop.rs):
/// the runtime truncates each tool-result text block to this many bytes before
/// the message reaches the transcript. The live `ToolCallFinished` event is not
/// truncated. Mirrored here because the runtime keeps it private.
const RUNTIME_MAX_TOOL_RESULT_BYTES: usize = 32 * 1024;

const KIB: usize = 1024;
const MIB: usize = 1024 * 1024;

/// Same truncation and marker shape as the runtime's `cap_tool_content`.
fn runtime_capped(output_bytes: usize) -> String {
    if output_bytes <= RUNTIME_MAX_TOOL_RESULT_BYTES {
        return "x".repeat(output_bytes);
    }
    let elided = output_bytes - RUNTIME_MAX_TOOL_RESULT_BYTES;
    let mut text = "x".repeat(RUNTIME_MAX_TOOL_RESULT_BYTES);
    text.push_str(&format!(
        "\n\n[… {elided} bytes truncated to fit context; full output shown in UI …]"
    ));
    text
}

fn model(context_window: u32) -> Model {
    Model {
        id: "measure-model".into(),
        name: "measure-model".into(),
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        base_url: String::new(),
        reasoning: false,
        supports_images: false,
        context_window,
        max_tokens: 16_000,
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TurnShape {
    user_bytes: usize,
    tools: usize,
    args_bytes: usize,
    output_bytes: usize,
    final_text_bytes: usize,
}

impl TurnShape {
    pub(crate) const fn with_output(output_bytes: usize) -> Self {
        Self {
            user_bytes: 400,
            tools: 4,
            args_bytes: 256,
            output_bytes,
            final_text_bytes: 1_600,
        }
    }
}

fn assistant(content: Vec<Content>, stop_reason: StopReason) -> Message {
    Message::Assistant(AssistantMessage {
        content,
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        model: "measure-model".into(),
        usage: Default::default(),
        stop_reason,
        error_message: None,
        timestamp: 1_758_800_000_000,
    })
}

fn pretty_bytes(session: &session::Session) -> usize {
    serde_json::to_string_pretty(session)
        .expect("session serializes")
        .len()
}

pub(crate) struct Report {
    context_window: u32,
    output_bytes: usize,
    turns: usize,
    /// File size (pretty JSON, exactly what `session::save` writes) after
    /// each turn's final save.
    end_of_turn_bytes: Vec<usize>,
    messages_at_end: usize,
    elided_total: usize,
    /// Sum of every save's size across all turns, when measured exactly.
    written_bytes: Option<usize>,
    saves: usize,
    /// The persisted transcript after the last turn's final save.
    pub(crate) final_messages: Vec<Message>,
}

/// Run `turns` turns through the same load → compact → accept-save → checkpoint
/// saves → final-save sequence `run_turn_inner` uses, with one tool call per
/// provider round.
pub(crate) fn simulate(
    shape: TurnShape,
    turns: usize,
    context_window: u32,
    exact_saves: bool,
) -> Report {
    let model = model(context_window);
    let mut session = session::Session::new_with_id(ocean_core::SessionId::new_v4(), &model);
    let mut end_of_turn_bytes = Vec::with_capacity(turns);
    let mut written = 0usize;
    let mut saves = 0usize;
    let mut elided_total = 0usize;
    let mut save = |session: &mut session::Session, messages: Vec<Message>| {
        session.replace_messages(cap_session_history(messages));
        saves += 1;
        if exact_saves {
            written += pretty_bytes(session);
        }
    };
    for turn in 0..turns {
        let mut history = session.messages.clone();
        elided_total += compact_history(&mut history, context_window);
        history.push(Message::user_text("u".repeat(shape.user_bytes)));
        save(&mut session, history.clone());
        for round in 0..shape.tools {
            let id = format!("toolu_{turn:06}_{round:02}");
            history.push(assistant(
                vec![Content::ToolCall {
                    id: id.clone(),
                    name: "bash".into(),
                    arguments: serde_json::json!({ "command": "a".repeat(shape.args_bytes) }),
                }],
                StopReason::ToolUse,
            ));
            history.push(Message::ToolResult(ToolResultMessage {
                tool_call_id: id,
                tool_name: "bash".into(),
                content: vec![Content::text(runtime_capped(shape.output_bytes))],
                is_error: false,
                timestamp: 1_758_800_000_000,
            }));
            save(&mut session, history.clone());
        }
        history.push(assistant(
            vec![Content::text("t".repeat(shape.final_text_bytes))],
            StopReason::Stop,
        ));
        save(&mut session, history);
        end_of_turn_bytes.push(pretty_bytes(&session));
    }
    Report {
        context_window,
        output_bytes: shape.output_bytes,
        turns,
        end_of_turn_bytes,
        messages_at_end: session.messages.len(),
        elided_total,
        written_bytes: exact_saves.then_some(written),
        saves,
        final_messages: session.messages,
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

fn print_reports(title: &str, reports: &[Report]) {
    println!("\n{title}");
    println!(
        "| context window | tool output K | turns | file after turn 1 | after turn 10 | after turn 20 | max over run | final | messages | tool results elided | saves | bytes written (all saves) |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|");
    for report in reports {
        let at = |turn: usize| {
            report
                .end_of_turn_bytes
                .get(turn - 1)
                .map_or("–".to_string(), |bytes| fmt_bytes(*bytes))
        };
        println!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            report.context_window,
            fmt_bytes(report.output_bytes),
            report.turns,
            at(1),
            at(10),
            at(20),
            fmt_bytes(report.end_of_turn_bytes.iter().copied().max().unwrap_or(0)),
            fmt_bytes(*report.end_of_turn_bytes.last().unwrap_or(&0)),
            report.messages_at_end,
            report.elided_total,
            report.saves,
            report
                .written_bytes
                .map_or("not measured".into(), fmt_bytes),
        );
    }
}

#[test]
fn persisted_session_bytes_by_turns_and_tool_output_size() {
    let mut reports = Vec::new();
    for window in [200_000, 1_000_000] {
        for output in [KIB, 32 * KIB] {
            reports.push(simulate(TurnShape::with_output(output), 25, window, false));
        }
    }
    print_reports(
        "Persisted session file (pretty JSON) — 4 tool rounds per turn, one save per boundary",
        &reports,
    );

    // The file on disk is exactly what we measured.
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut session =
        session::Session::new_with_id(ocean_core::SessionId::new_v4(), &model(200_000));
    session.replace_messages(vec![Message::user_text("hello")]);
    let path = session::save(tmp.path(), &session).expect("save");
    assert_eq!(
        std::fs::metadata(path).expect("metadata").len() as usize,
        pretty_bytes(&session)
    );

    for report in &reports {
        // The 200-message persistence cap always binds by turn 20 (10 messages
        // per turn), so the file stops growing with turn count.
        assert_eq!(report.messages_at_end, 200);
        let max = report.end_of_turn_bytes.iter().copied().max().unwrap();
        assert!(max < 4 * MIB, "file stayed under 4 MiB, got {max}");
    }
    let by = |window: u32, output: usize| {
        reports
            .iter()
            .find(|r| r.context_window == window && r.output_bytes == output)
            .unwrap()
    };
    // A smaller window compacts sooner, so its file is smaller.
    assert!(
        by(200_000, 32 * KIB).end_of_turn_bytes.last()
            < by(1_000_000, 32 * KIB).end_of_turn_bytes.last()
    );
}

/// Exact write volume (serialize at every save point, turns × 6 saves) plus
/// 2 MiB tool outputs, which the runtime's 32 KiB cap should make persist like
/// 32 KiB ones.
#[test]
#[ignore = "serializes ~1 GiB in a debug build; run with --ignored --nocapture"]
fn persisted_session_write_volume_exact() {
    let mut reports = Vec::new();
    for window in [200_000, 1_000_000] {
        for output in [KIB, 32 * KIB, 2 * MIB] {
            reports.push(simulate(TurnShape::with_output(output), 60, window, true));
        }
    }
    print_reports("Exact write volume over 60 turns", &reports);
    for window in [200_000, 1_000_000] {
        let max = |output: usize| {
            reports
                .iter()
                .find(|r| r.context_window == window && r.output_bytes == output)
                .and_then(|r| r.end_of_turn_bytes.iter().copied().max())
                .unwrap()
        };
        let (small, huge) = (max(32 * KIB), max(2 * MIB));
        assert!(huge.abs_diff(small) * 20 < small, "{huge} vs {small}");
    }
}

// ---------------------------------------------------------------------------
// H1 checkpoint save stall (bounded turn event channel proposal, slice 1).
// ---------------------------------------------------------------------------

/// One provider round's `TurnCheckpoint` delta: an assistant tool call and its
/// (runtime-capped) result, exactly the shape `simulate` persists per round.
pub(crate) fn checkpoint_round(shape: TurnShape, sample: usize) -> Vec<Message> {
    let id = format!("toolu_stall_{sample:04}");
    vec![
        assistant(
            vec![Content::ToolCall {
                id: id.clone(),
                name: "bash".into(),
                arguments: serde_json::json!({ "command": "a".repeat(shape.args_bytes) }),
            }],
            StopReason::ToolUse,
        ),
        Message::ToolResult(ToolResultMessage {
            tool_call_id: id,
            tool_name: "bash".into(),
            content: vec![Content::text(runtime_capped(shape.output_bytes))],
            is_error: false,
            timestamp: 1_758_800_000_000,
        }),
    ]
}

struct StallReport {
    context_window: u32,
    file_bytes: usize,
    messages: usize,
    samples: Vec<std::time::Duration>,
}

impl StallReport {
    fn percentile(&self, pct: usize) -> std::time::Duration {
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        sorted[(sorted.len() * pct / 100).min(sorted.len() - 1)]
    }
}

/// Time the work `run_prompt`'s H1 receive loop does inline for one
/// `TurnCheckpoint` (`crates/ocean-agent/src/lib.rs`, the `TurnCheckpoint`
/// arm): extend the in-turn prefix, `cap_session_history(clone)`,
/// `replace_messages`, and `session::save` (pretty JSON, write, fsync, durable
/// rename). While this runs the loop does not `recv`, so it is the H1
/// consumer stall per checkpoint.
///
/// Each sample starts from the same steady-state transcript (the 200-message
/// cap bound, `compact_history` applied as at turn start) plus one new round,
/// so every sample saves a file of the same size. The file saw-tooths with the
/// turn count (see §6 of the measurements doc), so a size point is a
/// (context window, turns) pair.
fn measure_checkpoint_stall(context_window: u32, turns: usize, samples: usize) -> StallReport {
    let shape = TurnShape::with_output(32 * KIB);
    let mut base = simulate(shape, turns, context_window, false).final_messages;
    compact_history(&mut base, context_window);
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut session =
        session::Session::new_with_id(ocean_core::SessionId::new_v4(), &model(context_window));
    // Warm the file and directory once so the first sample is not a create.
    session.replace_messages(base.clone());
    session::save(tmp.path(), &session).expect("warm save");

    let mut timings = Vec::with_capacity(samples);
    let mut file_bytes = 0;
    let mut messages = 0;
    for sample in 0..samples {
        let mut checkpoint_messages = base.clone();
        let delta = checkpoint_round(shape, sample);
        let started = std::time::Instant::now();
        checkpoint_messages.extend(delta);
        let persisted = cap_session_history(checkpoint_messages.clone());
        session.replace_messages(persisted);
        let path = session::save(tmp.path(), &session).expect("checkpoint save");
        timings.push(started.elapsed());
        file_bytes = std::fs::metadata(path).expect("metadata").len() as usize;
        messages = session.messages.len();
    }
    StallReport {
        context_window,
        file_bytes,
        messages,
        samples: timings,
    }
}

/// Slice 1 of `docs/specs/2026-09-26-bounded-turn-event-channel-proposal.md`:
/// the H1 consumer stall per `TurnCheckpoint` at the proposal's two
/// steady-state file sizes, 539,534 B (300k window, 21 turns) and 1,029,539 B
/// (600k window, 23 turns), both at the 200-message cap with 32 KiB tool
/// results. Timing only, so it is `#[ignore]`d; run it in release for the
/// recorded figures:
///
/// ```text
/// cargo test --release -p ocean-agent checkpoint_save_stall -- --ignored --nocapture --test-threads=1
/// ```
#[test]
#[ignore = "timing measurement (30 fsynced saves per size); run with --release --ignored --nocapture"]
fn checkpoint_save_stall_at_steady_state() {
    const SAMPLES: usize = 30;
    // (context window, turns, expected file bytes)
    const POINTS: [(u32, usize, usize); 2] = [(300_000, 21, 539_534), (600_000, 23, 1_029_539)];
    let reports: Vec<StallReport> = POINTS
        .into_iter()
        .map(|(window, turns, _)| measure_checkpoint_stall(window, turns, SAMPLES))
        .collect();
    println!(
        "\nH1 checkpoint stall: cap_session_history(clone) + session::save + fsync, {SAMPLES} saves each"
    );
    println!("| context window | file | messages | p50 | p90 | max |");
    println!("|---|---|---|---|---|---|");
    for report in &reports {
        println!(
            "| {} | {} ({} B) | {} | {:.1} ms | {:.1} ms | {:.1} ms |",
            report.context_window,
            fmt_bytes(report.file_bytes),
            report.file_bytes,
            report.messages,
            report.percentile(50).as_secs_f64() * 1e3,
            report.percentile(90).as_secs_f64() * 1e3,
            report.percentile(100).as_secs_f64() * 1e3,
        );
    }
    for (report, (_, _, expected)) in reports.iter().zip(POINTS) {
        assert_eq!(report.samples.len(), SAMPLES);
        // The size point is the one the proposal records (±1 %).
        assert!(
            report.file_bytes.abs_diff(expected) * 100 <= expected,
            "file {} B, expected about {expected} B",
            report.file_bytes
        );
        // The 200-message cap binds; one orphan leading tool result may be
        // dropped after the cut.
        assert!(
            (199..=200).contains(&report.messages),
            "the 200-message cap binds, got {}",
            report.messages
        );
    }
}
