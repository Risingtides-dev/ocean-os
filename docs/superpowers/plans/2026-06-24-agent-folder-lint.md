# Agent Folder Lint Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `validate()` function in `agentdir.rs` that catches broken agent folders with clear, actionable diagnostics before invoke, and wire an `ocean agents lint [path]` CLI subcommand.

**Architecture:** A `Diagnostic` struct (severity + path + message) is produced by a `validate()` function in `agentdir.rs` that walks an already-resolved `AgentDef` and checks all known failure modes. The CLI gets an `Agents { subcmd: AgentsCmd::Lint { path } }` arm that calls `discover` + `resolve` + `validate` and prints results, exiting non-zero on any `Error`-severity finding.

**Tech Stack:** Rust, clap (existing in ocean-cli), `ocean_agent::agentdir` (existing), `ocean_providers::known_models` (re-exported from `ocean_agent`).

---

## File Map

| File | Action | What changes |
|------|--------|--------------|
| `crates/ocean-agent/src/agentdir.rs` | Modify | Add `Severity`, `Diagnostic`, `validate()`, `builtin_tool_names()`. Add `#[cfg(test)]` coverage. |
| `crates/ocean-cli/Cargo.toml` | Modify | Add `ocean-agent` workspace dep. |
| `crates/ocean-cli/src/main.rs` | Modify | Add `Agents { subcmd: AgentsCmd }` + `AgentsCmd::Lint` arm in the `Cmd` enum; implement the lint handler. |

No other crates are touched.

---

### Task 1: Add `Severity`, `Diagnostic`, `validate()`, and `builtin_tool_names()` to agentdir.rs

**Files:**
- Modify: `crates/ocean-agent/src/agentdir.rs`

Background: `agentdir.rs` already has `AgentDef`, `AgentConfig`, `resolve()`, `discover()`. We're adding:

1. `builtin_tool_names()` — returns the hard-coded set of tool names the daemon ships today (same list as `ocean_runtime::tools::default_tools()` `.name()` output, but without pulling in async machinery). This is the source-of-truth list for the unknown-tool diagnostic. The set is: `read`, `write`, `edit`, `bash`, `ls`, `grep`, `glob`, `web_fetch`, `todo`, `component_render`, `component_unmount`, `surface_patch`, `slack_canvas`, `component_wait`.

2. `Severity` — `Error | Warn` enum with `Display`.

3. `Diagnostic` — struct with `severity: Severity`, `path: Option<PathBuf>`, `message: String`.

4. `validate(def: &AgentDef) -> Vec<Diagnostic>` — produces diagnostics for:
   - `Error`: `instructions.md` is missing or whitespace-only (`def.system_prompt()` is `None`).
   - `Error`: `agent.toml` declares a `model` that is NOT in `crate::known_models()` by `.id`.
   - `Warn`: a tool name in `def.effective_tools()` that is NOT in `builtin_tool_names()`.
   - `Warn`: a subagent name is listed in `def.subagents` but cannot be `resolve()`d from `<def.root>/subagents`.
   - `Warn`: a skill file listed in `def.skills` is present (path exists) but its content is empty/whitespace-only when read (indicating a dangling or blank skill).

- [ ] **Step 1: Add `builtin_tool_names()` to the bottom of agentdir.rs, above the `#[cfg(test)]` block**

```rust
/// The set of tool names the daemon ships as builtins today
/// (`ocean_runtime::tools::default_tools()` names). Kept as a
/// plain `HashSet` so `validate` can O(1)-check the agent's
/// declared allowlist without pulling in the runtime's async
/// machinery.
///
/// Update this list when `ocean_runtime::tools::mod.rs`
/// `default_tools()` gains or loses a tool.
pub fn builtin_tool_names() -> std::collections::HashSet<&'static str> {
    [
        "read",
        "write",
        "edit",
        "bash",
        "ls",
        "grep",
        "glob",
        "web_fetch",
        "todo",
        "component_render",
        "component_unmount",
        "surface_patch",
        "slack_canvas",
        "component_wait",
    ]
    .into_iter()
    .collect()
}
```

Run: `cargo build -p ocean-agent` — should compile cleanly.

- [ ] **Step 2: Add `Severity` and `Diagnostic` types immediately above `builtin_tool_names()`**

```rust
/// Severity of a lint diagnostic produced by [`validate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warn,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Error => write!(f, "error"),
            Severity::Warn => write!(f, "warn"),
        }
    }
}

/// A single lint finding from [`validate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    /// The file or directory this finding points at, if applicable.
    pub path: Option<PathBuf>,
    /// Human-readable message explaining the problem and, where possible,
    /// how to fix it.
    pub message: String,
}

impl Diagnostic {
    fn error(message: impl Into<String>) -> Self {
        Self { severity: Severity::Error, path: None, message: message.into() }
    }
    fn error_at(path: PathBuf, message: impl Into<String>) -> Self {
        Self { severity: Severity::Error, path: Some(path), message: message.into() }
    }
    fn warn(message: impl Into<String>) -> Self {
        Self { severity: Severity::Warn, path: None, message: message.into() }
    }
    fn warn_at(path: PathBuf, message: impl Into<String>) -> Self {
        Self { severity: Severity::Warn, path: Some(path), message: message.into() }
    }
}
```

Run: `cargo build -p ocean-agent` — should compile cleanly.

- [ ] **Step 3: Add `validate()` immediately after the `Diagnostic` block**

```rust
/// Lint a resolved [`AgentDef`] and return all findings.
///
/// `validate` is side-effect-free: it reads skill file bodies
/// lazily to check for empty content, but never writes anything and
/// never invokes the agent.  Returns an empty `Vec` when the agent
/// folder is fully valid.
///
/// **Error** severity means the agent WILL fail at invoke time.
/// **Warn** severity means the agent may behave unexpectedly.
///
/// The caller (e.g. `ocean agents lint`) should exit non-zero when
/// any `Error`-severity diagnostic is present.
pub fn validate(def: &AgentDef) -> Vec<Diagnostic> {
    let mut diags: Vec<Diagnostic> = Vec::new();

    // --- instructions.md ---------------------------------------------------
    // A missing or blank instructions.md means the agent has no system prompt
    // at all — the daemon will fall back to the compiled base prompt, which is
    // almost certainly not what the operator intended.
    if def.system_prompt().is_none() {
        diags.push(Diagnostic::error_at(
            def.root.join("instructions.md"),
            "instructions.md is missing or empty; the agent has no system prompt \
             (create instructions.md with at least one non-whitespace line)",
        ));
    }

    // --- model alias -------------------------------------------------------
    // A model id that's not in the known-models catalogue will silently fall
    // back to the daemon's global model at invoke time (fail-soft path in
    // `AgentRuntime::prompt`). Surface it as an error so the operator can
    // see the typo/outdated alias before wasting a turn.
    if let Some(model) = &def.config.model {
        let known = crate::known_models();
        if !known.iter().any(|m| m.id == *model) {
            let known_ids: Vec<&str> = known.iter().map(|m| m.id.as_str()).collect();
            diags.push(Diagnostic::error_at(
                def.root.join("agent.toml"),
                format!(
                    "model {:?} is not a known Ocean alias; known models: {}",
                    model,
                    known_ids.join(", ")
                ),
            ));
        }
    }

    // --- tool allowlist ----------------------------------------------------
    // Tools listed in the allowlist but absent from the builtin registry will
    // silently narrow the agent to zero tools (or just be ignored, depending
    // on the narrowing logic). Warn rather than error — future sideloaded
    // tools via subprocess:/wasm: may legitimately extend the set.
    let builtins = builtin_tool_names();
    for tool in def.effective_tools() {
        if !builtins.contains(tool.as_str()) {
            diags.push(Diagnostic::warn_at(
                def.root.join("agent.toml"),
                format!(
                    "tool {:?} is not a known builtin tool; known tools: {}. \
                     If this is a sideloaded tool (subprocess:/wasm:) it may be \
                     intentional.",
                    tool,
                    {
                        let mut names: Vec<_> = builtins.iter().copied().collect();
                        names.sort_unstable();
                        names.join(", ")
                    }
                ),
            ));
        }
    }

    // --- subagents ---------------------------------------------------------
    // A name listed in subagents/ that can't be resolved is a dangling
    // reference — the agent will fail when trying to delegate to it.
    let subagents_root = def.root.join("subagents");
    for sub_name in &def.subagents {
        if let Err(e) = resolve(&subagents_root, sub_name) {
            diags.push(Diagnostic::warn_at(
                subagents_root.join(sub_name),
                format!("subagent {:?} does not resolve: {e}"),
            ));
        }
    }

    // --- skills ------------------------------------------------------------
    // A skill file that exists but is entirely whitespace is a blank procedure
    // — the agent will see it in its skill list but get no content from it.
    for skill in &def.skills {
        match fs::read_to_string(&skill.path) {
            Ok(body) if body.trim().is_empty() => {
                diags.push(Diagnostic::warn_at(
                    skill.path.clone(),
                    format!(
                        "skill {:?} exists but is empty; add content or remove the file",
                        skill.name
                    ),
                ));
            }
            // Non-empty or unreadable (IO error reading a file that exists at
            // resolve time is transient — don't report it as a lint error).
            _ => {}
        }
    }

    diags
}
```

Run: `cargo build -p ocean-agent` — should compile cleanly.

- [ ] **Step 4: Commit agentdir.rs additions (types + validate + builtin_tool_names)**

```bash
git add crates/ocean-agent/src/agentdir.rs
git commit -m "feat(agentdir): add Diagnostic/Severity types and validate() lint fn

Checks: missing/empty instructions.md (error), bad model alias (error),
unknown tool names (warn), dangling subagents (warn), blank skills (warn).

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01EZXGEw6ppkJXinyGRPhSHq"
```

---

### Task 2: Add `#[test]` coverage in agentdir.rs

**Files:**
- Modify: `crates/ocean-agent/src/agentdir.rs` (the `#[cfg(test)]` block)

The existing `scaffold(tag: &str)` helper creates a valid `researcher` + `minimal` agent tree. We need additional fixture helpers per test that set up bad agents. Each uses a unique `tag` (like `"lint-bad-model"`) so parallel test runners never collide.

- [ ] **Step 5: Add validate tests inside the existing `#[cfg(test)]` block**

Add these tests inside the existing `mod tests { ... }` block in `agentdir.rs`:

```rust
    /// A valid agent (researcher from scaffold) must produce zero diagnostics.
    #[test]
    fn validate_clean_agent_returns_no_diagnostics() {
        let root = scaffold("validate-clean");
        let def = resolve(&root, "researcher").unwrap();
        let diags = validate(&def);
        assert!(
            diags.is_empty(),
            "clean agent must lint green, got: {diags:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// An agent with no instructions.md (config-only) must produce an Error
    /// diagnostic pointing at instructions.md.
    #[test]
    fn validate_missing_instructions_is_error() {
        let root = std::env::temp_dir().join(format!(
            "ocean-agentdir-test-{}-validate-no-instr",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let agent_dir = root.join("bot");
        fs::create_dir_all(&agent_dir).unwrap();
        // Only agent.toml — no instructions.md.
        fs::write(
            agent_dir.join("agent.toml"),
            "model = \"claude-opus-4-7\"\ndescription = \"test bot\"\n",
        )
        .unwrap();

        let def = resolve(&root, "bot").unwrap();
        let diags = validate(&def);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert!(
            !errors.is_empty(),
            "missing instructions.md must produce an error diagnostic"
        );
        assert!(
            errors
                .iter()
                .any(|d| d.message.contains("instructions.md")),
            "error must mention instructions.md; got: {errors:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// An agent with a whitespace-only instructions.md must also produce an
    /// Error (system_prompt() returns None for whitespace).
    #[test]
    fn validate_blank_instructions_is_error() {
        let root = std::env::temp_dir().join(format!(
            "ocean-agentdir-test-{}-validate-blank-instr",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let agent_dir = root.join("bot");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(agent_dir.join("instructions.md"), "   \n\t\n   ").unwrap();

        let def = resolve(&root, "bot").unwrap();
        let diags = validate(&def);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert!(
            !errors.is_empty(),
            "blank instructions.md must produce an error diagnostic"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A bad model alias in agent.toml must produce an Error diagnostic.
    #[test]
    fn validate_bad_model_alias_is_error() {
        let root = std::env::temp_dir().join(format!(
            "ocean-agentdir-test-{}-validate-bad-model",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let agent_dir = root.join("bot");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(
            agent_dir.join("agent.toml"),
            "model = \"not-a-real-model-xyz\"\ndescription = \"test\"\n",
        )
        .unwrap();
        fs::write(agent_dir.join("instructions.md"), "Do stuff.\n").unwrap();

        let def = resolve(&root, "bot").unwrap();
        let diags = validate(&def);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert!(
            !errors.is_empty(),
            "bad model alias must produce an error diagnostic"
        );
        assert!(
            errors.iter().any(|d| d.message.contains("not-a-real-model-xyz")),
            "error must name the bad model; got: {errors:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A tool name not in the builtin set must produce a Warn diagnostic.
    #[test]
    fn validate_unknown_tool_is_warn() {
        let root = std::env::temp_dir().join(format!(
            "ocean-agentdir-test-{}-validate-unknown-tool",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let agent_dir = root.join("bot");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(
            agent_dir.join("agent.toml"),
            "model = \"claude-opus-4-7\"\ndescription = \"test\"\ntools = [\"not_a_real_tool\"]\n",
        )
        .unwrap();
        fs::write(agent_dir.join("instructions.md"), "Do stuff.\n").unwrap();

        let def = resolve(&root, "bot").unwrap();
        let diags = validate(&def);
        let warns: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Warn)
            .collect();
        assert!(
            !warns.is_empty(),
            "unknown tool must produce a warn diagnostic"
        );
        assert!(
            warns.iter().any(|d| d.message.contains("not_a_real_tool")),
            "warn must name the unknown tool; got: {warns:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }
```

- [ ] **Step 6: Run tests to verify all pass**

```bash
cargo test -p ocean-agent -- agentdir 2>&1
```

Expected: all tests in `agentdir::tests` pass. Any test that fails means the implementation has a bug — fix it before continuing.

- [ ] **Step 7: Commit the tests**

```bash
git add crates/ocean-agent/src/agentdir.rs
git commit -m "test(agentdir): add validate() coverage for clean/bad-model/missing-instr/unknown-tool

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01EZXGEw6ppkJXinyGRPhSHq"
```

---

### Task 3: Wire `ocean agents lint` into ocean-cli

**Files:**
- Modify: `crates/ocean-cli/Cargo.toml`
- Modify: `crates/ocean-cli/src/main.rs`

The CLI does not currently depend on `ocean-agent`. We add it as a workspace dep. The `Agents` subcommand nests an `AgentsCmd` sub-subcommand. This mirrors how `Onboard` is a standalone `Cmd` variant — same pattern, but for local filesystem work (no HTTP).

- [ ] **Step 8: Add `ocean-agent` to ocean-cli's `[dependencies]` in Cargo.toml**

In `/Users/risingtidesdev/dev/ocean-os/.claude/worktrees/agent-aa897e78f09c2e635/crates/ocean-cli/Cargo.toml`, add after `ocean-core.workspace = true`:

```toml
ocean-agent.workspace = true
```

Run: `cargo build -p ocean-cli` — should compile (ocean-agent is already in the workspace).

- [ ] **Step 9: Add the `Agents` + `AgentsCmd::Lint` subcommand to main.rs**

In `crates/ocean-cli/src/main.rs`, add these new types after the existing `PermissionMode` block (before the `Cli` struct):

```rust
#[derive(Debug, Subcommand)]
enum AgentsCmd {
    /// Lint one or all agent folders under the agents root. Exits non-zero
    /// when any error-severity diagnostic is found.
    ///
    /// Checks:
    ///   - missing/empty instructions.md  (error)
    ///   - model alias not in known_models (error)
    ///   - tool name not a known builtin   (warn)
    ///   - dangling subagent reference     (warn)
    ///   - blank skill file                (warn)
    ///
    /// The agents root is $OCEAN_AGENTS_DIR or ./agents.
    Lint {
        /// Path to a single agent folder to lint instead of scanning the
        /// entire agents root.
        #[arg(value_name = "PATH")]
        path: Option<std::path::PathBuf>,
    },
}
```

Then add `Agents` to the existing `Cmd` enum:

```rust
    /// Agent folder (folder-as-agent) management. Run `ocean-rs agents --help`.
    Agents {
        #[command(subcommand)]
        subcmd: AgentsCmd,
    },
```

- [ ] **Step 10: Implement the Agents handler in `main()`**

In the `match cli.cmd { ... }` block in `main()`, add before the closing `}`:

```rust
        Cmd::Agents { subcmd } => match subcmd {
            AgentsCmd::Lint { path } => {
                use ocean_agent::agentdir::{discover, resolve, validate, Severity};

                /// Resolve the agents root: $OCEAN_AGENTS_DIR else ./agents.
                fn agents_root() -> std::path::PathBuf {
                    std::env::var("OCEAN_AGENTS_DIR")
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(|_| std::path::PathBuf::from("agents"))
                }

                // Collect (root, name) pairs to lint.
                let targets: Vec<(std::path::PathBuf, String)> = match path {
                    Some(p) => {
                        // Single path: the parent is the "root" and the folder
                        // name is the agent name — mirrors how resolve() works.
                        let p = p.canonicalize().with_context(|| {
                            format!("path does not exist: {}", p.display())
                        })?;
                        let name = p
                            .file_name()
                            .and_then(|n| n.to_str())
                            .map(|s| s.to_string())
                            .with_context(|| {
                                format!("cannot determine agent name from path: {}", p.display())
                            })?;
                        let parent = p
                            .parent()
                            .unwrap_or(std::path::Path::new("."))
                            .to_path_buf();
                        vec![(parent, name)]
                    }
                    None => {
                        let root = agents_root();
                        discover(&root)
                            .into_iter()
                            .map(|name| (root.clone(), name))
                            .collect()
                    }
                };

                if targets.is_empty() {
                    eprintln!("[ocean agents lint] no agent folders found");
                    return Ok(());
                }

                let mut any_error = false;
                for (root, name) in &targets {
                    let def = match resolve(root, name) {
                        Ok(d) => d,
                        Err(e) => {
                            eprintln!("error: {name}: {e}");
                            any_error = true;
                            continue;
                        }
                    };
                    let diags = validate(&def);
                    if diags.is_empty() {
                        println!("{name}: ok");
                    } else {
                        for d in &diags {
                            let loc = d
                                .path
                                .as_ref()
                                .map(|p| format!(" ({})", p.display()))
                                .unwrap_or_default();
                            println!("{}: {}{}: {}", name, d.severity, loc, d.message);
                        }
                        if diags.iter().any(|d| d.severity == Severity::Error) {
                            any_error = true;
                        }
                    }
                }

                if any_error {
                    std::process::exit(1);
                }
            }
        },
```

- [ ] **Step 11: Build to verify it compiles**

```bash
cargo build -p ocean-agent -p ocean-cli 2>&1
```

Expected: clean build, no errors. If there are type mismatches (e.g. `Severity` import path), fix them.

- [ ] **Step 12: Smoke-test the CLI with the example agent**

```bash
OCEAN_AGENTS_DIR=docs/examples/agents ./target/debug/ocean-rs agents lint
```

Expected output: `researcher: ok` (the shipped example agent is known-good per the `shipped_example_agent_resolves` test).

- [ ] **Step 13: Smoke-test a bad agent path**

```bash
mkdir -p /tmp/bad-lint-test/broken-agent
echo 'model = "not-a-model"' > /tmp/bad-lint-test/broken-agent/agent.toml
./target/debug/ocean-rs agents lint /tmp/bad-lint-test/broken-agent
echo "exit: $?"
```

Expected: non-zero exit, output lines containing `error` and `not-a-model`.

- [ ] **Step 14: Run the full test suite for both crates**

```bash
cargo test -p ocean-agent 2>&1 && cargo test -p ocean-cli 2>&1
```

Expected: all tests pass.

- [ ] **Step 15: Run clippy to verify no new warnings**

```bash
cargo clippy -p ocean-agent -p ocean-cli 2>&1
```

Expected: no new warnings introduced by this change.

- [ ] **Step 16: Commit the CLI additions**

```bash
git add crates/ocean-cli/Cargo.toml crates/ocean-cli/src/main.rs
git commit -m "feat(cli): add 'ocean agents lint' subcommand for pre-invoke agent validation

Discovers agents under OCEAN_AGENTS_DIR (./agents fallback), resolves each,
runs validate() and prints diagnostics. Exits non-zero on any error severity.

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01EZXGEw6ppkJXinyGRPhSHq"
```

---

### Task 4: Branch, final checks, push, and open PR

- [ ] **Step 17: Verify you're on the feature branch**

```bash
git branch --show-current
```

Expected: `feat/agent-folder-lint`. If not, you're on the wrong branch — check `git status` and correct before pushing.

- [ ] **Step 18: Final build + test + clippy pass**

```bash
cargo build -p ocean-agent -p ocean-cli 2>&1 && cargo test -p ocean-agent 2>&1 && cargo clippy -p ocean-agent -p ocean-cli 2>&1
```

Expected: green across the board.

- [ ] **Step 19: Push the branch**

```bash
git push -u origin feat/agent-folder-lint
```

- [ ] **Step 20: Open the PR**

```bash
gh pr create \
  --title "feat(agentdir): agent folder lint — catch broken folders before invoke" \
  --body "$(cat <<'EOF'
## Summary

- Adds `Severity`, `Diagnostic`, and `validate(&AgentDef) -> Vec<Diagnostic>` to `crates/ocean-agent/src/agentdir.rs`, catching: missing/empty `instructions.md` (error), unknown model alias (error), unknown builtin tool (warn), dangling subagent (warn), blank skill file (warn).
- Wires `ocean agents lint [path]` CLI subcommand in `ocean-cli`; exits non-zero on any error-severity finding.
- `#[test]` coverage: clean agent lints green; bad model alias / missing instructions / unknown tool each produce the expected diagnostic.

## Test plan

- [ ] `cargo test -p ocean-agent` passes (all agentdir tests including new lint tests)
- [ ] `cargo build -p ocean-agent -p ocean-cli` compiles cleanly
- [ ] `cargo clippy -p ocean-agent -p ocean-cli` — no new warnings
- [ ] `OCEAN_AGENTS_DIR=docs/examples/agents ./target/debug/ocean-rs agents lint` prints `researcher: ok`
- [ ] `ocean-rs agents lint /path/to/broken-agent` exits 1 and names the bad model

🤖 Generated with [Claude Code](https://claude.com/claude-code)

https://claude.ai/code/session_01EZXGEw6ppkJXinyGRPhSHq
EOF
)"
```

---

## Self-Review

**Spec coverage check:**

1. `validate()` with `Diagnostic` struct — Task 1. ✓
2. Model alias cross-check vs `known_models()` — Task 1, Step 3. ✓
3. Missing/empty `instructions.md` — Task 1, Step 3. ✓
4. Unknown builtin tools — Task 1, Step 3. ✓
5. Dangling subagents — Task 1, Step 3. ✓
6. Blank/unparseable skills — Task 1, Step 3 (blank skill check via `fs::read_to_string`). ✓
7. `ocean agents lint [path]` CLI subcommand — Task 3. ✓
8. Non-zero exit on error severity — Task 3, Step 10. ✓
9. Test coverage (good agent, bad model, missing instructions, unknown tool) — Task 2. ✓
10. Per-test unique temp-dir tags (parallel collision fix) — Task 2 (each test uses its own unique tag). ✓
11. `cargo build -p ocean-agent -p ocean-cli` green — Task 4, Step 18. ✓
12. `cargo test -p ocean-agent` green — Task 4, Step 18. ✓
13. `cargo clippy` no new warnings — Task 4, Step 18. ✓
14. Branch `feat/agent-folder-lint` — Task 4. ✓
15. PR via `gh pr create` — Task 4, Step 20. ✓

**Placeholder scan:** No TBDs. Every step has exact code or exact commands. Type names used consistently throughout (`Severity::Error`, `Diagnostic`, `validate`, `builtin_tool_names`).

**Type consistency:**
- `Diagnostic` struct: `severity: Severity`, `path: Option<PathBuf>`, `message: String` — used identically in validate() and CLI handler.
- `validate(def: &AgentDef) -> Vec<Diagnostic>` — import path in CLI: `ocean_agent::agentdir::{..., validate, Severity}`.
- `builtin_tool_names() -> HashSet<&'static str>` — used only inside `validate()`.
