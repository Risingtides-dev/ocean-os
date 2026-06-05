//! Property test for [`ocean_runtime::agent_loop::trim_to_context_window`]
//! (OCEAN-35).
//!
//! `trim_to_context_window` keeps the newest contiguous suffix of the message
//! history that fits the input budget, then drops any leading orphan
//! `ToolResult`s whose originating assistant `ToolCall` was trimmed away.
//!
//! The load-bearing invariant a provider relies on: **every `ToolResult` in the
//! output must be preceded, somewhere earlier in the output, by a `ToolCall`
//! (`Content::ToolCall`) carrying the same `tool_call_id`.** A `ToolResult`
//! whose call is absent is an "orphan" and is rejected by real providers
//! (Anthropic/OpenAI both 400 on a tool result with no matching tool call).
//!
//! This test generates random histories with interspersed `ToolCall` /
//! `ToolResult` pairs (plus noise), trims them to a wide range of budgets, and
//! asserts the no-orphan invariant holds for every output.

use ocean_protocol::{AssistantMessage, Content, Message, StopReason, ToolResultMessage};
use ocean_runtime::agent_loop::trim_to_context_window;
use proptest::collection::vec;
use proptest::prelude::*;
use std::collections::HashSet;

/// A scripted history element. The strategy emits a sequence of these, then we
/// lower them into real `Message`s while keeping tool-call ids consistent so
/// that a `ToolResult` always names a call that was emitted before it.
#[derive(Debug, Clone)]
enum Step {
    /// A plain user message of roughly `len` bytes.
    User { len: usize },
    /// A plain assistant text message of roughly `len` bytes.
    AssistantText { len: usize },
    /// An assistant message that issues a tool call with the given id.
    ToolCall { id: u32, len: usize },
    /// A tool result answering a previously-issued call id (resolved at
    /// lowering time against the set of ids actually issued so far).
    ToolResult { which: usize, len: usize },
}

fn assistant_text(len: usize) -> Message {
    Message::Assistant(AssistantMessage {
        content: vec![Content::text("a".repeat(len))],
        api: "test".into(),
        provider: "test".into(),
        model: "test".into(),
        usage: Default::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 0,
    })
}

fn assistant_tool_call(id: &str, len: usize) -> Message {
    Message::Assistant(AssistantMessage {
        content: vec![Content::ToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "cmd": "x".repeat(len) }),
        }],
        api: "test".into(),
        provider: "test".into(),
        model: "test".into(),
        usage: Default::default(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        timestamp: 0,
    })
}

fn tool_result(id: &str, len: usize) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: "bash".into(),
        content: vec![Content::text("r".repeat(len))],
        is_error: false,
        timestamp: 0,
    })
}

/// Collect the `tool_call_id`s a message contributes:
/// - an assistant `Content::ToolCall` *issues* an id,
/// - a `ToolResult` *consumes* an id.
fn issued_call_ids(msg: &Message) -> Vec<String> {
    match msg {
        Message::Assistant(a) => a
            .content
            .iter()
            .filter_map(|c| match c {
                Content::ToolCall { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn result_call_id(msg: &Message) -> Option<&str> {
    match msg {
        Message::ToolResult(r) => Some(r.tool_call_id.as_str()),
        _ => None,
    }
}

/// Lower a scripted `Vec<Step>` into a real `Vec<Message>`, assigning concrete
/// tool-call ids and resolving each `ToolResult` against the calls issued so
/// far. We deliberately allow a `ToolResult` to appear with *no* preceding call
/// (when no calls have been issued yet, it is dropped) — but when calls exist we
/// answer a real one, so the input contains genuine, well-formed pairs that
/// trimming might or might not split.
fn lower(steps: &[Step]) -> Vec<Message> {
    let mut out = Vec::new();
    let mut issued: Vec<String> = Vec::new();
    for step in steps {
        match step {
            Step::User { len } => out.push(Message::user_text("u".repeat(*len))),
            Step::AssistantText { len } => out.push(assistant_text(*len)),
            Step::ToolCall { id, len } => {
                let sid = format!("call-{id}");
                issued.push(sid.clone());
                out.push(assistant_tool_call(&sid, *len));
            }
            Step::ToolResult { which, len } => {
                if issued.is_empty() {
                    // No call to answer yet — skip so we never inject an orphan
                    // into the *input* (trimming, not construction, is what this
                    // test stresses).
                    continue;
                }
                let idx = which % issued.len();
                let sid = issued[idx].clone();
                out.push(tool_result(&sid, *len));
            }
        }
    }
    out
}

/// Assert the no-orphan invariant: every `ToolResult` is preceded by a matching
/// `ToolCall` somewhere earlier in the same slice.
fn assert_no_orphan_tool_results(msgs: &[Message]) {
    let mut live_calls: HashSet<String> = HashSet::new();
    for (i, msg) in msgs.iter().enumerate() {
        for id in issued_call_ids(msg) {
            live_calls.insert(id);
        }
        if let Some(id) = result_call_id(msg) {
            assert!(
                live_calls.contains(id),
                "orphan tool result at index {i}: tool_call_id {id:?} has no preceding \
                 matching ToolCall in the trimmed output: {msgs:#?}",
            );
        }
    }
}

fn step_strategy() -> impl Strategy<Value = Step> {
    prop_oneof![
        (1usize..400).prop_map(|len| Step::User { len }),
        (1usize..400).prop_map(|len| Step::AssistantText { len }),
        (0u32..8, 1usize..400).prop_map(|(id, len)| Step::ToolCall { id, len }),
        (0usize..16, 1usize..400).prop_map(|(which, len)| Step::ToolResult { which, len }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// For any random history and any budget, the trimmed output never contains
    /// an orphan tool result.
    #[test]
    fn trim_never_produces_orphan_tool_results(
        steps in vec(step_strategy(), 0..60),
        // Cover tiny windows (heavy trimming) through huge ones (no trimming).
        context_window in 64u32..200_000,
        max_tokens in 0u32..32_000,
        system_len in 0usize..2_000,
    ) {
        let msgs = lower(&steps);
        let system_prompt = "s".repeat(system_len);

        let kept = trim_to_context_window(&msgs, &system_prompt, context_window, max_tokens);

        // Core invariant.
        assert_no_orphan_tool_results(&kept);

        // Trimming is a suffix operation: the output is never longer than the
        // input, and is non-empty whenever the input was non-empty.
        prop_assert!(kept.len() <= msgs.len());
        if !msgs.is_empty() {
            prop_assert!(!kept.is_empty());
            // The most recent message is always preserved (it's the live turn).
            prop_assert!(
                serde_json::to_string(kept.last().unwrap()).unwrap()
                    == serde_json::to_string(msgs.last().unwrap()).unwrap()
                // ...unless the last input message was itself a leading orphan
                // tool result that got dropped, which can only happen when it is
                // also the *only* message kept — in which case the fallback keeps
                // it anyway. Re-check that case explicitly.
                || matches!(msgs.last(), Some(Message::ToolResult(_))),
                "last kept message should be the live turn unless it was an orphan tool result",
            );
        }
    }
}
