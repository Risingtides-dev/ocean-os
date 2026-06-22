# Ocean Project Workspace Binding

Architecture decision: how Ocean discovers and binds to a project workspace so it works like pi/codex/claude-code — launch from any directory, the agent knows where it is.

## Current State

### What works

- **TUI captures cwd at launch:** `ocean-tui/src/main.rs:2367` calls `env::current_dir()`, resolves relative `--root`, stores absolute path in `app.root`. This is sent as `cwd` on every turn.
- **Daemon resolves cwd correctly:** `ocean-agent/src/lib.rs:819` (`resolve_cwd_for_turn`) prefers client-supplied cwd, falls back to project workspace root, refuses to guess if neither is present.
- **System prompt walks up from cwd:** `build_system_prompt` at line 4446 discovers `AGENTS.md`, `CLAUDE.md`, `.ocean/AGENTS.md` by walking up from the turn's cwd.
- **Workspace root resolves via git:** `workspace_root()` at line 2047 invokes `git rev-parse --show-toplevel`, falls back to cwd.
- **Daemon refuses to boot from inside a git repo:** The cwd-binding trap (#229) prevents the daemon from conflating its own cwd with project workspace. Correct behavior.

### The binding rule

The old first-write-wins bug is gone. `bind_workspace` now refreshes `cwd` on every turn and rewrites `workspace_root` plus git metadata when the caller crosses into a new workspace. The daemon turn resolver keeps the caller's launch cwd as the execution directory, so a resumed session follows the directory it was opened from instead of freezing on its first bind.

```
cd /project-a && ocean          # session-1 → cwd=/project-a ✓
# ... later ...
cd /project-b && ocean --resume session-1   # cwd=/project-b, session rebinds ✓
```

That was the root cause of "ocean doesn't work across projects" and "launch cwd gets ignored."

## Decision: Explicit workspace rebind on resume

### Rule

When a turn arrives, the caller's cwd is the cwd the turn executes in. If the incoming cwd resolves to a different workspace root than the session currently holds, the session rebinds to that new workspace root on save. Otherwise the workspace root stays the same and only the cwd refreshes.

### Implementation

In `bind_workspace`, refresh `cwd` on every bind. If the incoming cwd resolves to a different workspace root than the session currently holds, replace `workspace_root`, `git_branch`, and `git_commit` too. The turn handler just passes the resolved cwd through.

Pseudocode for the turn handler in `ocean-agent/src/lib.rs`:

```rust
// After resolve_cwd_for_turn, before or during bind_workspace
let new_root = workspace_root(Path::new(&cwd));
session.cwd = Some(cwd.clone());
if session.workspace_root.as_deref().map(PathBuf::from) != Some(new_root.clone()) {
    // Different project — rebind the workspace bucket and git metadata.
    session.workspace_root = Some(new_root.to_string_lossy().into_owned());
    let (branch, commit) = probe_git(&new_root);
    session.git_branch = branch;
    session.git_commit = commit;
    // Preserve messages, keep session_id
}
```

### Behavior matrix

| Scenario | Old behavior | New behavior |
|---|---|---|
| New session, first turn | Bind to cwd ✓ | Same ✓ |
| Resume session, same project | Bind skipped (cwd stays stale) ✗ | Refresh cwd, keep workspace root ✓ |
| Resume session, different project | Stale cwd — agent talks about wrong project ✗ | Refresh cwd and rebind workspace ✓ |
| No cwd, no project_id | Daemon refuses (correct) ✓ | Same ✓ |
| `--project` flag | Uses project workspace root ✓ | Same ✓ |

### What doesn't change

- **Daemon cwd neutrality:** The daemon still doesn't bind to its own process cwd. The cwd-binding trap (#229) is unchanged.
- **Session persistence:** Sessions still save/load to disk. The recorded cwd, workspace root, and git metadata are mutable on resume.
- **Devlog discovery:** `build_system_prompt` already walks from cwd upward. Rebind means it walks from the correct cwd.
- **TUI behavior:** TUI captures `env::current_dir()` at launch and keeps that launch cwd as the surface root, even when a session resumes.

## Decision: Fresh-launch auto-resume

### Rule

When `ocean` is launched without `--session`, the TUI queries the daemon for sessions matching the launch cwd:

- **Zero sessions** → create a new session
- **Exactly one session** → auto-resume it
- **Multiple sessions** → create a new session (no picker in this pass)

No hidden global resume. No cross-project guessing. No stale session surprise.

### Implementation

In `ocean-tui/src/main.rs`, `run_daemon()` around line 2386, when `resumed` is `None`:

```rust
let sessions = client.sessions(&url, Some(&cwd_str), false)?;
if sessions.len() == 1 {
    resumed = Some(sessions[0].id.clone());
}
```

The `GET /v1/agent/sessions?cwd=...` endpoint already filters by workspace root. The launcher still resolves the matching session from that workspace, but it no longer swaps its root to the stored session root.

### Acceptance

- `cd repo && ocean` (first time) → new session, scoped to repo
- `cd repo && ocean` (second time) → auto-resumes the one session for that repo and keeps the launch cwd
- `cd repo && ocean` (with 3 sessions for that repo) → new session
- `cd repo && ocean --session <id>` → explicit resume, launch cwd still wins

## Devlog discovery root

The devlog root is the workspace root, not the cwd. `build_system_prompt` walks up from cwd, which in a git repo resolves to `git rev-parse --show-toplevel`. The root AGENTS.md lives there. This is already correct and requires no change.

If cwd is not in a git repo, the cwd itself is the discovery root. The agent walks up from cwd to find AGENTS.md. Also correct.

## Verification

- `cargo check --workspace` must pass
- `cargo test -p ocean-agent` must pass (existing `bind_workspace` tests at line 2620+)
- Manual: `cd /project-a && ocean` → new session → `cd /project-b && ocean --resume <id>` → agent detects project-b context
- Manual: `cd /project-a && ocean` → new session → `cd /project-a/subdir && ocean --resume <id>` → launch cwd stays `/project-a/subdir`
