//! Minimizer M2 command-output projection (provider-request only).
//!
//! Governed by
//! `docs/specs/2026-07-16-ocean-minimizer-command-capture-runtime-integration-design.md`.
//! This module derives M1 invocation identity from explicitly tokenized
//! built-in Bash `argv`, recognizes the exact generated `\n[exit N]` envelope,
//! runs the fixed `ocean-minimizer` filters, applies the net-savings rule, and
//! pins the exact raw Ocean tool text for the active run. It never changes the
//! ordinary tool result: live events, checkpoints, and saved history stay raw,
//! and every uncertainty returns `None` (fail open, no artifact).
//!
//! Nothing here logs, labels metrics with, or copies command/output content.

use ocean_minimizer::{Disposition, Invocation, Program};
use ocean_protocol::Content;
use serde_json::Value;

use crate::artifacts::{PinBudget, SharedArtifacts};
use crate::capability::SPILL_THRESHOLD_BYTES;
use crate::types::{AgentToolResult, ProviderProjection};

/// A minimized projection must save strictly more than this many bytes of
/// capture before it is used (design §3.7 fixed footer budget).
pub(crate) const FOOTER_BUDGET_BYTES: usize = 256;

/// Exact generated capture-cap markers from `BashTool`; their presence makes
/// the capture uncertain, so it always passes through.
const CAP_MARKERS: [&str; 2] = [
    "\n[stdout capped at 2MiB; the command ran to completion]",
    "\n[stderr capped at 2MiB; the command ran to completion]",
];

/// Widest possible artifact id (`a` + `u64::MAX`), used to prove net savings
/// before an artifact is created.
const WIDEST_ARTIFACT_ID: &str = "a18446744073709551615";

/// Map validated direct argv to an M1 invocation. Only an executable token that
/// is exactly one of the six bare M1 names is eligible; paths and every other
/// program are ineligible.
pub(crate) fn invocation_for_argv(argv: &[String]) -> Option<Invocation> {
    let (program, args) = argv.split_first()?;
    let program = match program.as_str() {
        "cargo" => Program::Cargo,
        "git" => Program::Git,
        "gh" => Program::Gh,
        "npm" => Program::Npm,
        "npx" => Program::Npx,
        "pytest" => Program::Pytest,
        _ => return None,
    };
    Some(Invocation::new(program, args.iter().cloned()))
}

/// Eligible M1 invocation for one built-in Bash call's authorized arguments.
/// `command` calls are never eligible.
pub(crate) fn invocation_for_args(args: &Value) -> Option<Invocation> {
    invocation_for_argv(&crate::tools::bash::direct_argv(args)?)
}

/// Split an exact generated envelope into `(capture, exit_code, suffix)`.
///
/// Only the final `\n[exit N]` suffix is parsed, and only when it round-trips
/// byte-for-byte through the generator's format. A capture that contains an
/// exact cap marker is uncertain and rejected.
pub(crate) fn split_envelope(raw: &str) -> Option<(&str, i32, &str)> {
    let start = raw.rfind("\n[exit ")?;
    let suffix = &raw[start..];
    let code: i32 = suffix
        .strip_prefix("\n[exit ")?
        .strip_suffix(']')?
        .parse()
        .ok()?;
    if format!("\n[exit {code}]") != suffix {
        return None;
    }
    let capture = &raw[..start];
    if CAP_MARKERS.iter().any(|marker| capture.contains(marker)) {
        return None;
    }
    Some((capture, code, suffix))
}

fn footer(
    input_lines: usize,
    output_lines: usize,
    input_bytes: usize,
    output_bytes: usize,
    id: &str,
) -> String {
    format!(
        "[output minimized: {input_lines}→{output_lines} lines, {input_bytes}→{output_bytes} bytes \
         · full output: read artifact://{id}]"
    )
}

/// Attempt the provider-only projection for one completed built-in Bash argv
/// result. Returns `None` (and creates no artifact) on every uncertainty.
pub(crate) fn project(
    invocation: &Invocation,
    result: &AgentToolResult,
    store: &SharedArtifacts,
    budget: &PinBudget,
) -> Option<ProviderProjection> {
    let [Content::Text { text: raw }] = result.content.as_slice() else {
        return None;
    };
    if raw.len() > SPILL_THRESHOLD_BYTES {
        return None;
    }
    let (capture, exit_code, suffix) = split_envelope(raw)?;
    let minimized = invocation.minimize(capture, exit_code);
    if minimized.disposition != Disposition::Minimized {
        return None;
    }
    let accounting = minimized.accounting;
    let saved = capture.len().checked_sub(minimized.text.len())?;
    if saved <= FOOTER_BUDGET_BYTES {
        return None;
    }
    let render = |id: &str| {
        format!(
            "{}{suffix}\n{}",
            minimized.text,
            footer(
                accounting.input_lines,
                accounting.output_lines,
                accounting.input_bytes,
                accounting.output_bytes,
                id,
            )
        )
    };
    // Prove net savings with the widest possible id before creating anything.
    if render(WIDEST_ARTIFACT_ID).len() >= raw.len() {
        return None;
    }
    let lease = budget.pin(store, "bash", raw.clone())?;
    let text = render(lease.id());
    debug_assert!(text.len() < raw.len());
    Some(ProviderProjection {
        content: vec![Content::text(text)],
        lease,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_bare_m1_names_from_direct_argv_are_eligible() {
        for (name, program) in [
            ("cargo", Program::Cargo),
            ("git", Program::Git),
            ("gh", Program::Gh),
            ("npm", Program::Npm),
            ("npx", Program::Npx),
            ("pytest", Program::Pytest),
        ] {
            let invocation = invocation_for_args(&json!({"argv": [name, "x", "y z"]})).unwrap();
            assert_eq!(invocation, Invocation::new(program, ["x", "y z"]));
        }
        for argv in [
            json!(["/usr/bin/git", "status"]),
            json!(["./cargo", "test"]),
            json!(["bin/pytest"]),
            json!(["Git", "status"]),
            json!(["git ", "status"]),
            json!(["bash", "-lc", "git status"]),
            json!(["env", "git", "status"]),
            json!(["sh", "-c", "cargo test"]),
            json!([]),
            json!([""]),
            json!(["git", 1]),
        ] {
            assert!(
                invocation_for_args(&json!({ "argv": argv })).is_none(),
                "{argv} must be ineligible"
            );
        }
    }

    #[test]
    fn command_strings_are_never_eligible() {
        for command in [
            "git status",
            "cargo test --workspace",
            "git status | cat",
            "cargo test > out.txt",
            "git $(echo status)",
            "FOO=1 git status",
            "command git status",
            "\\git status",
            "git status &&",
            "'unterminated",
        ] {
            assert!(invocation_for_args(&json!({ "command": command })).is_none());
            assert!(
                invocation_for_args(&json!({ "command": command, "argv": ["git", "status"] }))
                    .is_none(),
                "both modes is a validation error, never eligible"
            );
        }
        assert!(invocation_for_args(&json!({})).is_none());
    }

    #[test]
    fn envelope_recognition_is_exact() {
        assert_eq!(
            split_envelope("a\n\n[exit 0]"),
            Some(("a\n", 0, "\n[exit 0]"))
        );
        assert_eq!(split_envelope("\n[exit -1]"), Some(("", -1, "\n[exit -1]")));
        assert_eq!(
            split_envelope("x\n[exit 1]\ny\n[exit 101]"),
            Some(("x\n[exit 1]\ny", 101, "\n[exit 101]")),
            "only the final suffix is parsed"
        );
        for bad in [
            "no suffix",
            "a\n[exit 0] ",
            "a\n[exit 00]",
            "a\n[exit +1]",
            "a\n[exit ]",
            "a\n[exit x]",
            "a[exit 0]",
            "a\n[exit 99999999999]",
        ] {
            assert!(split_envelope(bad).is_none(), "{bad:?}");
        }
        for marker in CAP_MARKERS {
            assert!(split_envelope(&format!("a{marker}\n[exit 0]")).is_none());
        }
    }

    // -- wrapper-level behavior (crate-private policy constructor) ----------

    use crate::artifacts::new_shared;
    use crate::capability::SpillingTool;
    use crate::types::{AgentTool, ToolExecutionResult};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn fixture(path: &str) -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../ocean-minimizer/tests/fixtures")
                .join(path),
        )
        .expect("M1 fixture")
    }

    struct Canned {
        text: String,
        runs: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl AgentTool for Canned {
        fn name(&self) -> &str {
            "bash"
        }
        fn description(&self) -> &str {
            "canned"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        async fn execute(&self, _id: &str, _args: Value) -> Result<AgentToolResult, String> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok(AgentToolResult::text(self.text.clone()))
        }
    }

    fn wrapper(
        text: String,
        budget: PinBudget,
    ) -> (SpillingTool, SharedArtifacts, Arc<AtomicUsize>) {
        let store = new_shared();
        let runs = Arc::new(AtomicUsize::new(0));
        let tool = SpillingTool::with_command_minimization(
            Arc::new(Canned {
                text,
                runs: runs.clone(),
            }),
            store.clone(),
            budget,
        );
        (tool, store, runs)
    }

    async fn run(tool: &SpillingTool, args: Value) -> ToolExecutionResult {
        tool.execute_for_run("call_1", args).await.expect("ok")
    }

    #[tokio::test]
    async fn eligible_argv_gets_exact_projection_and_pinned_raw_artifact() {
        let raw = format!("{}\n[exit 0]", fixture("git/status-long.raw"));
        let (tool, store, runs) = wrapper(raw.clone(), PinBudget::default());
        let execution = run(&tool, json!({"argv": ["git", "status"]})).await;
        assert_eq!(runs.load(Ordering::SeqCst), 1, "inner runs exactly once");
        assert_eq!(execution.result().content, vec![Content::text(raw.clone())]);
        let id = execution
            .recovery_artifact_id()
            .expect("projection")
            .to_string();
        let expected_min = fixture("git/status-long.min");
        let expected = format!(
            "{expected_min}\n[exit 0]\n[output minimized: 15→6 lines, {}→{} bytes · full output: read artifact://{id}]",
            raw.len() - "\n[exit 0]".len(),
            expected_min.len(),
        );
        assert_eq!(
            execution.provider_content().unwrap(),
            &[Content::text(expected.clone())][..]
        );
        assert!(expected.len() < raw.len(), "net savings");
        {
            let s = store.lock().unwrap();
            assert_eq!(
                s.get(&id).unwrap().text,
                raw,
                "artifact is the exact raw text"
            );
            assert!(s.is_pinned(&id));
        }
        drop(execution);
        assert_eq!(store.lock().unwrap().pinned_len(), 0, "lease released");
    }

    #[tokio::test]
    async fn direct_execute_never_exposes_a_projection_and_releases_the_pin() {
        let raw = format!("{}\n[exit 0]", fixture("git/status-long.raw"));
        let (tool, store, _) = wrapper(raw.clone(), PinBudget::default());
        let out = tool
            .execute("call_1", json!({"argv": ["git", "status"]}))
            .await
            .unwrap();
        assert_eq!(out.content, vec![Content::text(raw)]);
        assert_eq!(store.lock().unwrap().pinned_len(), 0);
    }

    #[tokio::test]
    async fn every_uncertainty_fails_open_with_no_artifact() {
        let status = format!("{}\n[exit 0]", fixture("git/status-long.raw"));
        let cases: Vec<(String, Value, PinBudget)> = vec![
            // command mode, even for an identical capture
            (
                status.clone(),
                json!({"command": "git status"}),
                PinBudget::default(),
            ),
            // both modes
            (
                status.clone(),
                json!({"command": "git status", "argv": ["git", "status"]}),
                PinBudget::default(),
            ),
            // path executable
            (
                status.clone(),
                json!({"argv": ["/usr/bin/git", "status"]}),
                PinBudget::default(),
            ),
            // machine flag rejected by M1
            (
                status.clone(),
                json!({"argv": ["git", "status", "--porcelain"]}),
                PinBudget::default(),
            ),
            // exhausted pin budget
            (
                status.clone(),
                json!({"argv": ["git", "status"]}),
                PinBudget::new(0, usize::MAX),
            ),
            (
                status.clone(),
                json!({"argv": ["git", "status"]}),
                PinBudget::new(32, 10),
            ),
            // savings below the 256-byte footer budget
            (
                format!("{}\n[exit 0]", fixture("git/status.raw")),
                json!({"argv": ["git", "status"]}),
                PinBudget::default(),
            ),
            // ambiguous output
            (
                "hello\n\n[exit 0]".into(),
                json!({"argv": ["git", "status"]}),
                PinBudget::default(),
            ),
            // malformed envelope / no exit suffix
            (
                fixture("git/status-long.raw"),
                json!({"argv": ["git", "status"]}),
                PinBudget::default(),
            ),
            // truncated capture
            (
                format!(
                    "{}\n[stdout capped at 2MiB; the command ran to completion]\n[exit 0]",
                    fixture("git/status-long.raw")
                ),
                json!({"argv": ["git", "status"]}),
                PinBudget::default(),
            ),
        ];
        for (raw, args, budget) in cases {
            let (tool, store, runs) = wrapper(raw.clone(), budget);
            let execution = run(&tool, args.clone()).await;
            assert_eq!(runs.load(Ordering::SeqCst), 1);
            assert!(!execution.has_provider_projection(), "{args}");
            assert_eq!(
                execution.result().content,
                vec![Content::text(raw)],
                "{args}"
            );
            assert!(store.lock().unwrap().is_empty(), "no artifact for {args}");
        }
    }

    #[tokio::test]
    async fn multiple_or_non_text_blocks_pass_through() {
        struct Multi;
        #[async_trait]
        impl AgentTool for Multi {
            fn name(&self) -> &str {
                "bash"
            }
            fn description(&self) -> &str {
                "multi"
            }
            fn parameters(&self) -> Value {
                json!({})
            }
            async fn execute(&self, _id: &str, _args: Value) -> Result<AgentToolResult, String> {
                let text = format!("{}\n[exit 0]", fixture("git/status-long.raw"));
                Ok(AgentToolResult {
                    content: vec![Content::text(text.clone()), Content::text(text)],
                    ..AgentToolResult::default()
                })
            }
        }
        let store = new_shared();
        let tool = SpillingTool::with_command_minimization(
            Arc::new(Multi),
            store.clone(),
            PinBudget::default(),
        );
        let execution = run(&tool, json!({"argv": ["git", "status"]})).await;
        assert!(!execution.has_provider_projection());
        assert!(store.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn over_threshold_raw_only_spills_normally() {
        let mut raw = String::from("On branch main\n");
        while raw.len() <= SPILL_THRESHOLD_BYTES {
            raw.push_str("\tmodified:   some/long/path/file.rs\n");
        }
        raw.push_str("\n[exit 0]");
        let (tool, store, _) = wrapper(raw.clone(), PinBudget::default());
        let execution = run(&tool, json!({"argv": ["git", "status"]})).await;
        assert!(
            !execution.has_provider_projection(),
            "no minimization of a spill"
        );
        let s = store.lock().unwrap();
        assert_eq!(s.len(), 1, "exactly the ordinary spill artifact");
        assert_eq!(s.pinned_len(), 0);
        assert_eq!(s.get("a1").unwrap().text, raw);
    }

    #[tokio::test]
    async fn poisoned_store_fails_open() {
        let raw = format!("{}\n[exit 0]", fixture("git/status-long.raw"));
        let (tool, store, _) = wrapper(raw.clone(), PinBudget::default());
        let poisoner = store.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("poison");
        })
        .join();
        let execution = run(&tool, json!({"argv": ["git", "status"]})).await;
        assert!(!execution.has_provider_projection());
        assert_eq!(execution.result().content, vec![Content::text(raw)]);
    }

    #[test]
    fn projection_source_adds_no_logging_or_metadata_echo() {
        let source = include_str!("output_economy.rs");
        let code = source.split("#[cfg(test)]").next().unwrap();
        for forbidden in ["tracing", "println!", "eprintln!", "log::", "details"] {
            assert!(!code.contains(forbidden), "{forbidden} in output_economy");
        }
    }
}
