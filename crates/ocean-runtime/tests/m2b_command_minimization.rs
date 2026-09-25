//! Minimizer M2b — the gated-on path, end to end.
//!
//! Drives the real `CapabilityRegistry` → built-in argv-mode `BashTool` →
//! output-economy wrapper → agent loop with the default-off
//! `SessionContext::command_output_minimization` gate explicitly enabled.
//! Eligible programs are operator-provided executables selected by `PATH`
//! (design §3.3: the direct argv token is invocation identity, not a vendor
//! attestation), so this binary prepends a scratch directory of scripted
//! `cargo`/`git`/`gh`/`npm`/`npx`/`pytest` stand-ins to `PATH`.
//!
//! `PATH` is process-global, so everything runs inside ONE `#[test]`, which
//! sets it before any runtime thread or child process exists.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream;
use ocean_minimizer::{Invocation, Program};
use ocean_protocol::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Content, Context,
    Message, Model, Provider, StopReason, StreamOptions, Usage,
};
use ocean_runtime::capability::{BuiltinProvider, CapabilityRegistry, SessionContext};
use ocean_runtime::tools::bash::{argv_mode_parameters, BashTool};
use ocean_runtime::types::{AgentConfig, AgentEvent, AgentTool};
use ocean_runtime::{AgentError, ArtifactStore, CapabilityProvider, SharedArtifacts};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const PROGRAMS: [&str; 6] = ["cargo", "git", "gh", "npm", "npx", "pytest"];

struct Harness {
    bin: PathBuf,
    cwd: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("ocean-m2b-{}", std::process::id()));
        let bin = root.join("bin");
        let cwd = root.join("work");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        for program in PROGRAMS {
            let script = format!(
                "#!/bin/sh\ncat '{dir}/{program}.out'\nexit \"$(cat '{dir}/{program}.code')\"\n",
                dir = bin.display()
            );
            let path = bin.join(program);
            std::fs::write(&path, script).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        Self { bin, cwd }
    }

    /// Script what `program` prints (stdout) and its exit code.
    fn script(&self, program: &str, output: &str, code: i32) {
        std::fs::write(self.bin.join(format!("{program}.out")), output).unwrap();
        std::fs::write(self.bin.join(format!("{program}.code")), code.to_string()).unwrap();
    }

    fn ctx(&self, session: &str, gate: bool, artifacts: bool) -> SessionContext {
        SessionContext {
            cwd: self.cwd.clone(),
            session_id: Some(session.into()),
            artifacts,
            command_output_minimization: gate,
            ..SessionContext::default()
        }
    }
}

fn fixture(path: &str) -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../ocean-minimizer/tests/fixtures")
            .join(path),
    )
    .unwrap_or_else(|e| panic!("fixture {path}: {e}"))
}

async fn session_tools(
    ctx: &SessionContext,
) -> (
    Vec<Arc<dyn AgentTool>>,
    SharedArtifacts,
    Arc<BuiltinProvider>,
) {
    let builtin = Arc::new(BuiltinProvider::new());
    let registry = CapabilityRegistry::new(vec![builtin.clone()]);
    let tools = registry.tools_for_session(ctx).await;
    let store = builtin
        .artifacts_store(ctx.session_id.as_deref().unwrap())
        .unwrap();
    (tools, store, builtin)
}

fn bash_of(tools: &[Arc<dyn AgentTool>]) -> Arc<dyn AgentTool> {
    tools.iter().find(|t| t.name() == "bash").unwrap().clone()
}

fn text(content: &[Content]) -> &str {
    assert_eq!(content.len(), 1);
    content[0].as_text().unwrap()
}

fn store_len(store: &SharedArtifacts) -> usize {
    store.lock().unwrap().len()
}

fn pinned(store: &SharedArtifacts) -> usize {
    store.lock().unwrap().pinned_len()
}

fn cargo_build_failure() -> String {
    let mut out = String::new();
    for i in 0..30 {
        out.push_str(&format!(
            "   Compiling crate{i} v0.1.0 (/work/crates/crate{i})\n"
        ));
    }
    out.push_str("error[E0425]: cannot find value `x` in this scope\n --> src/lib.rs:1:1\n");
    out.push_str("error: could not compile `crate29` due to 1 previous error\n");
    out
}

fn gh_checks(all_pass: bool) -> String {
    let mut out = String::new();
    for i in 0..20 {
        out.push_str(&format!("✓\tjob-{i}\t1m{i}s\thttps://ci.test/job-{i}\n"));
    }
    if !all_pass {
        out.push_str("X\ttest\t3m4s\thttps://ci.test/test\n");
    }
    out
}

fn npx_first_run() -> String {
    let mut out = String::from(
        "Need to install the following packages:\ncowsay@1.6.0\nOk to proceed? (y)\n\n",
    );
    for i in 0..12 {
        out.push_str(&format!(
            "npm warn deprecated pkg{i}@1.0.0: this package is no longer maintained\n"
        ));
    }
    out.push_str(" _______\n< Hello >\n -------\n");
    out
}

#[test]
fn gated_command_output_minimization_end_to_end() {
    let harness = Harness::new();
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{path}", harness.bin.display()));

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            schema_and_gate_matrix(&harness).await;
            argv_validation_and_direct_execution(&harness).await;
            positive_programs(&harness).await;
            passthrough_via_real_execution(&harness).await;
            loop_projection_history_and_resume(&harness).await;
            run_cleanup_on_error_and_cancel(&harness).await;
        });
}

// ---------------------------------------------------------------------------

async fn schema_and_gate_matrix(h: &Harness) {
    let legacy = BashTool::new().parameters();
    let schema: Value = serde_json::from_str(include_str!(
        "../../ocean-protocol/tests/fixtures/bash_argv_tool_schema.json"
    ))
    .unwrap();
    assert_eq!(
        argv_mode_parameters(),
        schema,
        "runtime schema == protocol fixture"
    );
    for key in ["oneOf", "anyOf", "allOf", "required"] {
        assert!(
            schema.get(key).is_none(),
            "plain-object schema has no {key}"
        );
    }

    // (gate, artifacts, session) → argv offered?
    for (gate, artifacts, session, offered) in [
        (true, true, true, true),
        (false, true, true, false),
        (true, false, true, false),
        (true, true, false, false),
        (false, false, false, false),
    ] {
        let mut ctx = h.ctx("matrix", gate, artifacts);
        if !session {
            ctx.session_id = None;
        }
        let registry = CapabilityRegistry::builtin_only();
        let bash = bash_of(&registry.tools_for_session(&ctx).await);
        let expected = if offered {
            schema.clone()
        } else {
            legacy.clone()
        };
        assert_eq!(
            bash.parameters(),
            expected,
            "gate={gate} artifacts={artifacts} session={session}"
        );
    }
    assert!(
        !SessionContext::default().command_output_minimization,
        "direct callers default off"
    );

    // Gate off: argv is not a mode at all (legacy error, nothing spawned).
    let (tools, store, _) = session_tools(&h.ctx("off", false, true)).await;
    h.script("git", &fixture("git/status-long.raw"), 0);
    let error = bash_of(&tools)
        .execute_for_run("c", json!({"argv": ["git", "status"]}))
        .await
        .expect_err("legacy mode");
    assert_eq!(error, "missing 'command'");
    let out = bash_of(&tools)
        .execute_for_run("c", json!({"command": "git status"}))
        .await
        .unwrap();
    assert!(!out.has_provider_projection(), "gate off never projects");
    assert_eq!(store_len(&store), 0);
}

async fn argv_validation_and_direct_execution(h: &Harness) {
    let (tools, store, _) = session_tools(&h.ctx("argv", true, true)).await;
    let bash = bash_of(&tools);
    let marker = h.cwd.join("must-not-exist");
    let touch = marker.to_string_lossy().to_string();
    for (args, message) in [
        (
            json!({"command": format!("touch '{touch}'"), "argv": ["touch", touch]}),
            "provide exactly one of 'command' or 'argv', not both",
        ),
        (json!({}), "provide exactly one of 'command' or 'argv'"),
        (json!({"argv": []}), "'argv' must not be empty"),
        (json!({"argv": [""]}), "'argv' executable must not be empty"),
        (
            json!({"argv": ["touch", 1]}),
            "'argv' must be an array of strings",
        ),
        (
            json!({"argv": "touch x"}),
            "'argv' must be an array of strings",
        ),
        (json!({"command": 5}), "'command' must be a string"),
    ] {
        let error = bash.execute("c", args.clone()).await.expect_err("invalid");
        assert_eq!(error, message, "{args}");
    }
    assert!(!marker.exists(), "validation fails before spawn");

    // Direct execution: no expansion, globbing, or shell metacharacters.
    let out = bash
        .execute("c", json!({"argv": ["printf", "%s|", "$HOME", "*", "a;b"]}))
        .await
        .unwrap();
    assert_eq!(text(&out.content), "$HOME|*|a;b|\n[exit 0]");

    // Same envelope, cwd, and exit contract as command mode.
    let script = "printf o; printf e >&2; exit 3";
    let argv = bash
        .execute("c", json!({"argv": ["sh", "-c", script]}))
        .await
        .unwrap();
    let command = bash.execute("c", json!({"command": script})).await.unwrap();
    assert_eq!(text(&argv.content), "o\n[stderr]\ne\n[exit 3]");
    assert_eq!(argv.content, command.content);
    let pwd = bash.execute("c", json!({"argv": ["pwd"]})).await.unwrap();
    assert!(text(&pwd.content).contains("m2b"), "turn cwd is shared");

    // Timeout keeps the existing error and never projects.
    let error = bash
        .execute_for_run("c", json!({"argv": ["sleep", "5"], "timeout_ms": 200}))
        .await
        .expect_err("times out");
    assert_eq!(error, "command timed out after 200ms");
    assert_eq!(store_len(&store), 0);
}

async fn positive_programs(h: &Harness) {
    let cases: Vec<(&str, Vec<&str>, Program, String, i32)> = vec![
        (
            "cargo",
            vec!["test", "--workspace"],
            Program::Cargo,
            fixture("cargo/test-pass.raw"),
            0,
        ),
        (
            "cargo",
            vec!["build"],
            Program::Cargo,
            cargo_build_failure(),
            101,
        ),
        (
            "git",
            vec!["status"],
            Program::Git,
            fixture("git/status-long.raw"),
            0,
        ),
        ("git", vec!["log"], Program::Git, fixture("git/log.raw"), 0),
        (
            "gh",
            vec!["pr", "checks", "7"],
            Program::Gh,
            gh_checks(false),
            1,
        ),
        (
            "gh",
            vec!["pr", "checks", "7"],
            Program::Gh,
            gh_checks(true),
            0,
        ),
        (
            "npm",
            vec!["install"],
            Program::Npm,
            fixture("npm/install.raw"),
            0,
        ),
        (
            "npx",
            vec!["cowsay", "Hello"],
            Program::Npx,
            npx_first_run(),
            0,
        ),
        (
            "pytest",
            vec!["-v"],
            Program::Pytest,
            fixture("pytest/success.raw"),
            0,
        ),
        (
            "pytest",
            vec!["tests/test_math.py"],
            Program::Pytest,
            fixture("pytest/failure.raw"),
            1,
        ),
    ];
    for (program, args, m1, output, code) in cases {
        let (tools, store, _) =
            session_tools(&h.ctx(&format!("pos-{program}-{code}"), true, true)).await;
        h.script(program, &output, code);
        let mut argv = vec![program];
        argv.extend(args.iter().copied());
        let execution = bash_of(&tools)
            .execute_for_run("c", json!({ "argv": argv }))
            .await
            .unwrap();

        let raw = format!("{output}\n[exit {code}]");
        assert_eq!(
            text(&execution.result().content),
            raw,
            "{program} {args:?}: ordinary result is the exact raw envelope"
        );
        let id = execution
            .recovery_artifact_id()
            .unwrap_or_else(|| panic!("{program} {args:?} {code} must project"))
            .to_string();
        let m1_out = Invocation::new(m1, args.clone()).minimize(&output, code);
        let a = m1_out.accounting;
        let expected = format!(
            "{}\n[exit {code}]\n[output minimized: {}→{} lines, {}→{} bytes · full output: read artifact://{id}]",
            m1_out.text, a.input_lines, a.output_lines, a.input_bytes, a.output_bytes
        );
        let provider = text(execution.provider_content().unwrap());
        assert_eq!(provider, expected, "{program} {args:?}");
        assert!(provider.len() < raw.len(), "net savings for {program}");
        assert!(
            provider.contains(&format!("[exit {code}]")),
            "exit evidence is preserved"
        );
        let artifact = store.lock().unwrap().get(&id).unwrap().text.clone();
        assert_eq!(artifact, raw, "one exact pinned artifact");
        assert_eq!(store_len(&store), 1);
        assert_eq!(pinned(&store), 1);
        drop(execution);
        assert_eq!(pinned(&store), 0, "lease drop releases the pin");
    }
}

async fn passthrough_via_real_execution(h: &Harness) {
    let status = fixture("git/status-long.raw");
    let git_path = h.bin.join("git").to_string_lossy().to_string();
    let cases: Vec<(Value, &str, String, i32)> = vec![
        (json!({"command": "git status"}), "git", status.clone(), 0),
        (
            json!({"command": "git status | cat"}),
            "git",
            status.clone(),
            0,
        ),
        (
            json!({"argv": [git_path, "status"]}),
            "git",
            status.clone(),
            0,
        ),
        (
            json!({"argv": ["git", "status", "--porcelain"]}),
            "git",
            status.clone(),
            0,
        ),
        (
            json!({"argv": ["git", "status"]}),
            "git",
            fixture("git/status.raw"),
            0,
        ),
        (
            json!({"argv": ["gh", "pr", "checks", "1"]}),
            "gh",
            fixture("gh/pr-checks.raw"),
            1,
        ),
        (
            json!({"argv": ["git", "status"]}),
            "git",
            "not a status\n".into(),
            0,
        ),
        (
            json!({"argv": ["cargo", "metadata"]}),
            "cargo",
            status.clone(),
            0,
        ),
    ];
    for (args, program, output, code) in cases {
        let (tools, store, _) = session_tools(&h.ctx("pass", true, true)).await;
        h.script(program, &output, code);
        let execution = bash_of(&tools)
            .execute_for_run("c", args.clone())
            .await
            .unwrap();
        assert!(!execution.has_provider_projection(), "{args}");
        // Command strings run through `bash -lc`, whose login startup files may
        // rewrite PATH (so the shadowing stand-in need not even run) — exactly
        // why shell source is never eligible. Compare against the legacy tool.
        let expected = if args.get("command").is_some() {
            let legacy = BashTool::for_cwd(h.cwd.clone())
                .execute("c", args.clone())
                .await
                .unwrap();
            text(&legacy.content).to_string()
        } else {
            format!("{output}\n[exit {code}]")
        };
        assert_eq!(
            text(&execution.result().content),
            expected,
            "{args}: raw bytes unchanged"
        );
        assert_eq!(store_len(&store), 0, "{args}: no artifact");
    }

    // Over the spill threshold: only the ordinary spill artifact, no projection.
    let mut big = String::from("On branch main\n\nChanges not staged for commit:\n");
    while big.len() <= 30_000 {
        big.push_str("\tmodified:   some/long/path/file.rs\n");
    }
    let (tools, store, _) = session_tools(&h.ctx("spill", true, true)).await;
    h.script("git", &big, 0);
    let execution = bash_of(&tools)
        .execute_for_run("c", json!({"argv": ["git", "status"]}))
        .await
        .unwrap();
    assert!(!execution.has_provider_projection());
    assert!(text(&execution.result().content).contains("[output truncated:"));
    assert_eq!(store_len(&store), 1);
    assert_eq!(pinned(&store), 0);
}

// ---------------------------------------------------------------------------
// Agent loop: raw authority everywhere, projection only in active requests.
// ---------------------------------------------------------------------------

struct Scripted {
    turns: Mutex<VecDeque<Vec<AssistantMessageEvent>>>,
    contexts: Mutex<Vec<Context>>,
    cancel_on: Option<(usize, CancellationToken)>,
}

impl Scripted {
    fn new(turns: Vec<Vec<AssistantMessageEvent>>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
            contexts: Mutex::new(Vec::new()),
            cancel_on: None,
        }
    }
}

#[async_trait]
impl Provider for Scripted {
    async fn stream(
        &self,
        _model: &Model,
        context: &Context,
        _options: &StreamOptions,
    ) -> ocean_protocol::Result<AssistantMessageEventStream> {
        let round = {
            let mut contexts = self.contexts.lock().unwrap();
            contexts.push(context.clone());
            contexts.len() - 1
        };
        if let Some((at, token)) = &self.cancel_on {
            if *at == round {
                // Halt while the provider is silent: the loop's biased cancel
                // race must unwind with `Cancelled`.
                token.cancel();
                return Ok(Box::pin(stream::pending()));
            }
        }
        let turn = self.turns.lock().unwrap().pop_front().expect("turn");
        Ok(Box::pin(stream::iter(turn.into_iter().map(Ok))))
    }
}

fn done(content: Vec<Content>, stop: StopReason) -> AssistantMessageEvent {
    AssistantMessageEvent::Done {
        reason: stop,
        message: AssistantMessage {
            content,
            api: "mock".into(),
            provider: "mock".into(),
            model: "mock".into(),
            usage: Usage::default(),
            stop_reason: stop,
            error_message: None,
            timestamp: 0,
        },
    }
}

fn bash_call(id: &str, args: Value) -> AssistantMessageEvent {
    done(
        vec![Content::ToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: args,
        }],
        StopReason::ToolUse,
    )
}

fn tool_results(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult(r) => Some(text(&r.content).to_string()),
            _ => None,
        })
        .collect()
}

async fn loop_projection_history_and_resume(h: &Harness) {
    h.script("git", &fixture("git/status-long.raw"), 0);
    let raw = format!("{}\n[exit 0]", fixture("git/status-long.raw"));
    let (tools, store, _) = session_tools(&h.ctx("loop", true, true)).await;
    let provider = Arc::new(Scripted::new(vec![
        vec![bash_call("call_1", json!({"argv": ["git", "status"]}))],
        // A later round reuses the provider id with an ineligible call.
        vec![bash_call(
            "call_1",
            json!({"argv": ["git", "status", "--porcelain"]}),
        )],
        vec![done(vec![Content::text("done")], StopReason::Stop)],
    ]));
    let config = AgentConfig::new(Model::anthropic_claude_sonnet_4_6(), "sys")
        .with_provider(provider.clone())
        .with_tools(tools)
        .with_session_id("loop");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let run = ocean_runtime::run_agent(&config, Message::user_text("go"), Some(tx))
        .await
        .expect("run");

    let mut live = Vec::new();
    let mut checkpointed = Vec::new();
    let mut ended = None;
    while let Ok(event) = rx.try_recv() {
        match event {
            AgentEvent::ToolExecutionEnd {
                content, details, ..
            } => live.push((text(&content).to_string(), details)),
            AgentEvent::TurnCheckpoint { messages, .. } => checkpointed.extend(messages),
            AgentEvent::AgentEnd { messages, .. } => ended = Some(messages),
            _ => {}
        }
    }
    assert_eq!(
        live,
        vec![(raw.clone(), Value::Null), (raw.clone(), Value::Null)],
        "live ToolExecutionEnd content/details stay raw"
    );
    assert_eq!(tool_results(&checkpointed), vec![raw.clone(), raw.clone()]);
    assert_eq!(tool_results(&run.messages), vec![raw.clone(), raw.clone()]);
    assert_eq!(
        tool_results(&ended.unwrap()),
        vec![raw.clone(), raw.clone()]
    );
    let saved = serde_json::to_string(&run.messages).unwrap();
    assert!(
        !saved.contains("artifact://"),
        "saved history has no M2 URI"
    );

    let contexts = provider.contexts.lock().unwrap().clone();
    let round2 = tool_results(&contexts[1].messages);
    assert_eq!(round2.len(), 1);
    assert!(
        round2[0].contains("[output minimized:"),
        "active request projects"
    );
    assert!(round2[0].contains("read artifact://a1]"));
    let round3 = tool_results(&contexts[2].messages);
    assert_eq!(round3.len(), 2);
    assert_eq!(round3[0], round2[0], "projection bound to its own ordinal");
    assert_eq!(round3[1], raw, "repeated call_1 id is not retargeted");

    assert_eq!(pinned(&store), 0, "run end releases every lease");
    assert!(
        store.lock().unwrap().get("a1").is_some(),
        "artifact returns to ordinary eviction, not deleted"
    );

    // Daemon restart + resume: fresh provider/store, raw history only.
    let (tools, _, _) = session_tools(&h.ctx("loop", true, true)).await;
    let resumed = Arc::new(Scripted::new(vec![vec![done(
        vec![Content::text("resumed")],
        StopReason::Stop,
    )]]));
    let mut history = run.messages.clone();
    history.push(Message::user_text("continue"));
    let config = AgentConfig::new(Model::anthropic_claude_sonnet_4_6(), "sys")
        .with_provider(resumed.clone())
        .with_tools(tools)
        .with_session_id("loop");
    ocean_runtime::run_agent_with_history(&config, history, None)
        .await
        .expect("resume");
    let request = resumed.contexts.lock().unwrap()[0].clone();
    assert_eq!(tool_results(&request.messages), vec![raw.clone(), raw]);
    assert!(!serde_json::to_string(&request.messages)
        .unwrap()
        .contains("artifact://"));
}

async fn run_cleanup_on_error_and_cancel(h: &Harness) {
    h.script("git", &fixture("git/status-long.raw"), 0);

    // Provider error after a projected round.
    let (tools, store, _) = session_tools(&h.ctx("error", true, true)).await;
    let provider = Arc::new(Scripted::new(vec![
        vec![bash_call("call_1", json!({"argv": ["git", "status"]}))],
        vec![AssistantMessageEvent::Error {
            reason: StopReason::Error,
            error: AssistantMessage {
                content: vec![],
                api: "mock".into(),
                provider: "mock".into(),
                model: "mock".into(),
                usage: Usage::default(),
                stop_reason: StopReason::Error,
                error_message: Some("boom".into()),
                timestamp: 0,
            },
        }],
    ]));
    let config = AgentConfig::new(Model::anthropic_claude_sonnet_4_6(), "sys")
        .with_provider(provider)
        .with_tools(tools)
        .with_session_id("error");
    let error = ocean_runtime::run_agent(&config, Message::user_text("go"), None).await;
    assert!(error.is_err());
    assert_eq!(pinned(&store), 0, "error return releases leases");
    assert_eq!(store_len(&store), 1);

    // Cancellation after a projected round.
    let (tools, store, _) = session_tools(&h.ctx("cancel", true, true)).await;
    let token = CancellationToken::new();
    let mut provider = Scripted::new(vec![vec![bash_call(
        "call_1",
        json!({"argv": ["git", "status"]}),
    )]]);
    provider.cancel_on = Some((1, token.clone()));
    let mut config = AgentConfig::new(Model::anthropic_claude_sonnet_4_6(), "sys")
        .with_provider(Arc::new(provider))
        .with_tools(tools)
        .with_session_id("cancel");
    config.stream_options.cancel = Some(token);
    let result = ocean_runtime::run_agent(&config, Message::user_text("go"), None).await;
    assert!(
        matches!(result, Err(AgentError::Cancelled)),
        "{:?}",
        result.err()
    );
    assert_eq!(pinned(&store), 0, "cancellation releases leases");

    // Pin budget pressure inside one run cannot evict a pinned artifact.
    let tiny: SharedArtifacts = Arc::new(Mutex::new(ArtifactStore::new(64, 2)));
    let budget = ocean_runtime::PinBudget::default();
    let lease = budget.pin(&tiny, "bash", "pinned raw".into()).unwrap();
    for i in 0..5 {
        tiny.lock()
            .unwrap()
            .put("bash", format!("spill-{i}-{}", "x".repeat(40)));
    }
    assert!(tiny.lock().unwrap().get(lease.id()).is_some());
}
