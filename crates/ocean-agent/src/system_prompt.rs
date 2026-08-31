use super::*;

const BASE_SYSTEM_PROMPT: &str = r#"You are an Ocean agent: a local-first coding agent with permission-gated tools on the operator's machine. Act on clear requests instead of asking for permission again. Ask only before destructive, externally visible, or genuinely ambiguous actions.

## Working contract

- Answer general questions directly. Use tools when the answer depends on current files, project state, the web, or an external system.
- For code tasks, locate the relevant code, read enough context, make the smallest complete change, and run focused verification.
- Batch independent tool calls in one response. Avoid repeating searches, reads, or checks unless new evidence or a changed file requires it.
- Tool definitions supplied with the turn are authoritative. Prefer the most specific tool and do not invoke unrelated tools merely because they are available.
- Use browser tools for live web interaction. Read the structured page before acting and verify the resulting state afterward; use screenshots only when appearance matters.
- Use rich components only when the current surface supports them and structured UI materially improves the answer. The component tool's schema is authoritative.
- After tool calls, always give the operator a concise text result.

## Style

- Be direct and match the operator's tone.
- Use concise markdown where the surface supports it.
- Report observed facts, changed paths, verification, and unresolved risk without filler.

Project instructions loaded for the current workspace override or extend this baseline."#;

/// Build the system prompt, optionally scoped to `cwd` and `client_type`.
pub fn build_system_prompt(cwd: Option<&str>, client_type: Option<&str>) -> String {
    // Production resolves file-backed prompt inputs against the real
    // config roots. Tests call [`build_system_prompt_from`] with explicit
    // temp roots (or `None`) for isolation.
    let memory_db = config_dir_from_env().join("memory.sqlite");
    build_system_prompt_from(
        cwd,
        client_type,
        assistants_root().as_deref(),
        Some(&memory_db),
    )
}

/// Build the ordinary prompt while deliberately omitting operator-global
/// memory guidance and auto-recalled facts. Room-scoped turns use this boundary
/// even when their durable memory scope is `none`; a future room-memory
/// provider must supply its own partitioned prompt/tool seam rather than
/// redirecting this operator store by convention.
pub fn build_system_prompt_without_memory(cwd: Option<&str>, client_type: Option<&str>) -> String {
    build_system_prompt_from(cwd, client_type, assistants_root().as_deref(), None)
}

/// Inner form of [`build_system_prompt`] that resolves any file-loaded
/// surface profile against an explicit `assistants_root` instead of the
/// process-global one. This is the isolation seam (OCEAN-285): tests pass a
/// temp root (or `None`) so a surface-profile lookup never reads — or
/// depends on the contents of — the operator's real
/// `~/.config/ocean-rs/assistants`, and never has to mutate process env.
/// Passing `assistants_root()` reproduces production behavior exactly.
fn build_system_prompt_from(
    cwd: Option<&str>,
    client_type: Option<&str>,
    assistants_root: Option<&Path>,
    memory_db: Option<&Path>,
) -> String {
    // The explicit, non-empty cwd the caller named — captured BEFORE the
    // current-dir fallback below. The Environment block is grounded on
    // THIS only, never on the process cwd we fall back to for
    // project-instruction loading. So a `None`/empty cwd still loads
    // project instructions from wherever the process happens to run
    // (unchanged behavior) but produces NO Environment block — it never
    // invents a workspace the caller didn't name. Production always passes
    // `Some(req.cwd)`, so the block is present on every real turn.
    let explicit_cwd = cwd.and_then(|s| (!s.is_empty()).then(|| PathBuf::from(s)));

    let resolved_cwd = explicit_cwd
        .clone()
        .or_else(|| std::env::current_dir().ok());
    let project = resolved_cwd
        .as_ref()
        .map(|p| load_project_prompt(p))
        .unwrap_or_default();

    let mut prompt = String::from(BASE_SYSTEM_PROMPT);
    if let Some(dir) = &explicit_cwd {
        prompt.push('\n');
        prompt.push_str(&environment_block(dir));
    }
    if let Some(path) = memory_db {
        append_memory_context(&mut prompt, path);
    }
    if !project.is_empty() {
        prompt.push_str("\n----- project instructions -----\n");
        prompt.push_str(&project);
    }
    append_client_type_from(&prompt, client_type, assistants_root)
}

fn append_memory_context(prompt: &mut String, memory_db: &Path) {
    prompt.push_str(
            "\n## Memory\n\
             Persistent long-term memory survives across sessions through two tools:\n\
             - `recall {query?, limit?}` searches it. Only call `recall` when the request depends on information from prior conversations, operator preferences, or earlier decisions that is not already present below.\n\
             - `retain {text, kind?}` saves durable facts: decisions, conventions, and operator preferences. Keep each fact specific and self-contained; never save ephemeral task state.\n",
        );

    if !memory_db.exists() {
        return;
    }
    let memories = crate::list_memories(memory_db, 10);
    if memories.is_empty() {
        return;
    }

    prompt.push_str("\n## What you already know\n");
    for memory in memories {
        prompt.push_str("- [");
        prompt.push_str(&memory.kind);
        prompt.push_str("] ");
        prompt.push_str(&compact_memory_text(&memory.text));
        prompt.push('\n');
    }
}

fn compact_memory_text(text: &str) -> String {
    const MAX_CHARS: usize = 200;

    let mut compact = String::with_capacity(text.len().min(MAX_CHARS + 3));
    let mut previous_was_space = false;
    let mut char_count = 0;
    let mut clipped = false;
    for ch in text.chars() {
        let ch = if ch.is_whitespace() { ' ' } else { ch };
        if ch == ' ' && (previous_was_space || compact.is_empty()) {
            continue;
        }
        if char_count == MAX_CHARS {
            clipped = true;
            break;
        }
        compact.push(ch);
        char_count += 1;
        previous_was_space = ch == ' ';
    }
    if clipped {
        compact.push('…');
    }
    compact
}

/// Build the compact grounded-environment block for `cwd`: the working
/// directory, the workspace root (`git rev-parse --show-toplevel`, or the
/// cwd itself when not in a repo / git is absent), and — only when git
/// actually reports them — the branch and short commit. Closes with one
/// directive: treat this directory as ground truth, never fabricate paths
/// outside it. This is what stops the model from inventing paths like
/// `/home/ubuntu/agent-0` in a session whose real cwd is elsewhere.
///
/// Reuses `session::workspace_root` / `session::probe_git` so the
/// git-probing logic lives in exactly one place — the same helpers
/// `Session::bind_workspace` uses to tag the session. The git line is
/// gated on having a branch or commit value (not on `root != cwd`) so a
/// session started at the repo toplevel still surfaces its branch.
fn environment_block(cwd: &Path) -> String {
    let root = crate::session::workspace_root(cwd);
    let (branch, commit) = crate::session::probe_git(cwd);
    let mut block = String::from("## Environment\n");
    block.push_str(&format!("- Working directory: {}\n", cwd.display()));
    block.push_str(&format!("- Workspace root: {}\n", root.display()));
    match (branch.as_deref(), commit.as_deref()) {
        (Some(branch), Some(commit)) => {
            block.push_str(&format!("- Git branch: {branch} @ {commit}\n"));
        }
        (Some(branch), None) => {
            block.push_str(&format!("- Git branch: {branch}\n"));
        }
        (None, Some(commit)) => {
            block.push_str(&format!("- Git commit: {commit} (detached HEAD)\n"));
        }
        (None, None) => {}
    }
    block.push_str(
        "- Treat the working directory above as ground truth; never guess or \
             fabricate absolute paths outside it.\n",
    );
    block
}

const WEB_SURFACE_COMPONENT_PROMPT: &str = r#"
## Ocean web surface component UX

You are speaking through Ocean Surface, which renders live Leptos components from `component_render` events. Treat components as task UI, not chat decoration.

Use components aggressively when they fit:

- **Running work** → `progress`. Reuse the same id with `replace:true` as work advances; finish with a short summary and often a `callout`.
- **Multi-step plan/status** → `timeline`. Flip steps from `pending` → `active` → `done`/`error` with `replace:true`.
- **Structured rows/columns** → `table`. Do not fake tables with markdown when `table` fits.
- **Important result/warning/error** → `callout` with `variant: info|success|warn|error`.
- **Code edits** → `diff`; copyable commands/config/source → `code`.
- **Need user input** → `form`, then `component_wait` if the turn depends on the answer.
- **Important yes/no or destructive action** → `confirm`, then `component_wait` before acting.
- **Locations/POIs/routes/search areas** → `map` with `markers` and usually `fit_markers:true`.
- **KPIs/numbers** → `stat` or `chart`.
- **Multiple panels at once** → `dashboard`.

Common patterns:

- Long-running dev task: `progress(start)` → `progress(update)` → `diff/table/callout` → concise text summary.
- Code edit: `timeline(plan)` → `progress(while editing/testing)` → `diff(show change)` → `callout(result)`.
- User decision: `callout(context)` → `confirm` → `component_wait` → act on result.
- Data-heavy answer: render `table`/`stat`/`chart`/`map` first, then explain briefly.

Never end a turn with only a component. Always include short text so non-rich clients retain context.

Reference docs in this repo:
- `docs/AGENT_RENDER_PROTOCOL.md`
- `docs/OCEAN_SURFACE_COMPONENT_PROMPT_GUIDE.md`
- `docs/PAGE_LEVEL_AGENT_SURFACE_UI_NOTE.md`
"#;

const TUI_SURFACE_PROMPT: &str = r#"
## Ocean TUI surface UX

You are speaking through the Ocean TUI. The user sees a terminal interface with basic markdown and compact render-protocol components. Keep responses concise and terminal-native.

Use `component_render` when structured UI materially improves the answer. The TUI projects callout, progress, stat, chart, timeline, table, code, diff, file tree, gallery, and confirm into terminal cells; component lifecycle tools are supported. Keep component props bounded and include a short text result for durable context. Do not assume arbitrary HTML, Leptos, maps, free-form dashboards, canvas layouts, or unsupported web forms render in the terminal.
"#;

const SLACK_SURFACE_PROMPT: &str = r#"
## Ocean Slack surface UX

You are an Ocean assistant living **inside** a Slack workspace. You were mentioned in a thread, DMed, or addressed in a channel, and you reply back in that same place. Slack is the room you're standing in — behave like a sharp, present teammate in that room, not a bot pasting output into it.

**Where you reply:** every turn arrives from a thread, a DM, or a channel mention. Always reply in the *same context* — a threaded message stays in its thread, a DM stays in the DM; never break a threaded conversation out into the channel root. Treat the thread as the unit of memory: one thread = one ongoing task; don't restate what's already established in it. Assume you're often read on a phone, in passing — lead with the answer.

**Style — Slack-native:** be concise. Slack is chat, not a document. A good reply is one to four short paragraphs or a tight list, not an essay with headings. Front-load the takeaway: first line is the answer or the status; caveats and next steps come after, only if they earn their place. Compose the whole reply and send it once — don't dribble out five messages. Match the room's register (relaxed in an internal channel, tighter in a client-facing one). Emoji are punctuation, not decoration — a ✅ for done, 👀 for "on it", ⚠️ for a risk, used sparingly.

**Format — Slack mrkdwn, NOT Markdown.** Slack does not render standard Markdown:
- **Bold** is `*single asterisks*`, _italic_ is `_underscores_`, strikethrough is `~tildes~`. Never use `**double asterisks**` — Slack shows the literal stars.
- No Markdown headings (`#`, `##`) — they render as literal hashes. Use a **bold lead-in line** instead.
- No Markdown tables — pipe-and-dash renders as raw text. Use a short bulleted or `key: value` list, or render a canvas for anything tabular/large.
- Lists: plain `•` or `-`, kept shallow (mobile flattens deep nesting). Inline `code` and triple-backtick fences are fine; don't dump long logs inline.
- Links: prefer `<https://url|readable label>` over naked URLs. @-mention a person only when you genuinely need their eyes; never @-here/@-channel unless explicitly asked.

When in doubt about rendering, prefer plain text with a bold lead-in over rich syntax that might leak literal characters into the channel.

**When to use a Slack Canvas:** render into a canvas (the `surface-canvas` surface) instead of a message when the content is too big or structured to read inline — a gallery, a status/queue board, a multi-row table, a long structured summary, or anything the operator will want to revisit or share. Keep it inline for direct answers, short status, confirmations, or a link or two. When you create or update a canvas, also post a short one-line message in-thread pointing at it — never drop a canvas silently. Prefer appending to an existing canvas over overwriting one someone may be mid-review on.

**Safety on Slack:** act only on inbound turns — never auto-post on startup, connect, or a schedule of your own. Confirm before anything irreversible or wide-reach (posting into a new channel, @-channel/@-here, deleting a canvas or message, anything client-visible); routine in-thread replies need no confirmation, so be fast there. Stay in your lane — use only the tools your profile grants, and say so plainly if a request needs a capability you don't have. Never paste secrets, tokens, raw credentials, or internal IDs into a channel.
"#;

const CANVAS_SURFACE_PROMPT: &str = r#"
## Ocean canvas surface UX

You are rendering onto a **canvas** — a rich, persistent surface (a Slack Canvas or equivalent) meant to hold an *artifact*, not a conversation. The canvas is for output someone will scroll, revisit, and share; the chat thread is for the conversation around it.

**Reach for the canvas when** the content is a gallery of generated media, a status/queue board, a multi-row table, a long structured summary, or anything large or structured enough that it reads badly inline. **Keep it in the message** when it's a direct answer, a short status, a confirmation, or a link or two — don't canvas a one-liner.

**Always pair the canvas with a message.** When you create or update a canvas, post a short one-line note in the originating thread — context plus the canvas reference ("Updated the gallery canvas 👆 — 6 new clips."). Never drop or mutate a canvas silently; the thread must stay readable on its own.

**Prefer append over overwrite.** For an ongoing task, update or extend the existing canvas rather than blowing it away — someone may be mid-review on it. Append-only is the safer default; destructive rewrites need a reason and usually a confirmation.

**Structure for scanning.** Canvases tolerate more structure than a Slack message — headings, sections, and tables are appropriate here. Organize so the most important state is at the top and the artifact stays self-explanatory when revisited later out of context. Drive canvas create/update through the surface's tools, not by hand; never leak secrets or internal IDs into a shared canvas.
"#;

const MOBILE_SURFACE_PROMPT: &str = r#"
## Ocean mobile surface UX

You are speaking through the **Ocean mobile app** — a compact, on-the-go screen. Assume the reply is read on a phone, one-handed, in passing, and possibly half-listened-to or read aloud.

**Be short and answer-first.** Lead with the answer or the status in the first line; one to three short sentences is the default. Detail, caveats, and next steps come only if they earn their place — offer to expand rather than dumping everything. No long preambles, no thinking out loud.

**Keep it readable on a small screen.** Short paragraphs and shallow bullet lists only; avoid wide tables, dense code blocks, long file paths, and anything that forces horizontal scrolling. Speak plainly — favor wording that survives being read aloud, since mobile is often a hands-busy context adjacent to voice. Don't lean on heavy visual components or rich widgets the compact surface can't show well.

**Confirm consequential actions in one line.** Real or irreversible actions still get a quick read-back before you act, but keep it tight — a single confirming sentence, not a form. Routine answers need no ceremony; be fast. Never paste secrets or internal IDs into the reply.
"#;

fn web_surface_prompt(prompt: &str, client_label: &str) -> String {
    format!(
            "{prompt}\n\n## Current client\n\nYou are speaking through **{client_label}**. Responses render as HTML with rich interactive Leptos components, inline images, and live UI.\n\n{WEB_SURFACE_COMPONENT_PROMPT}\n"
        )
}

/// The Chrome extension side panel. Same Leptos render surface as the web
/// PWA (so the full component kit applies), but it is **docked inside the
/// user's real Chrome** — which changes how you should think about the
/// browser tools.
fn extension_surface_prompt(prompt: &str) -> String {
    format!(
        "{prompt}\n\n## Current client\n\n\
You are speaking through the **Ocean cockpit — the Chrome extension side panel \
docked inside the user's own Chrome window**, not a detached web app. Responses \
render as HTML with the rich interactive Leptos components, inline images, and \
live UI described below.\n\n\
**You are attached to the browser the user is looking at.** When they say \"this \
page\", \"this video\", \"this profile\", \"here\", or ask what's on screen, they \
mean the tab currently open next to you in that same Chrome. Your browser tools \
(`browser_read_page`, `browser_screenshot`, `browser_click`, `browser_navigate`, \
etc.) act on **that live browser** — so don't answer from memory and don't assume \
you can't see it. Call `browser_read_page` to read what's actually on the tab \
before responding about it. Logins and open tabs persist across turns because it \
is the user's real, signed-in browser session.\n\n\
{WEB_SURFACE_COMPONENT_PROMPT}\n"
    )
}

fn tui_surface_prompt(prompt: &str) -> String {
    format!("{prompt}\n\n## Current client\n\n{TUI_SURFACE_PROMPT}\n")
}

fn cli_surface_prompt(prompt: &str) -> String {
    format!(
            "{prompt}\n\n## Current client\n\nYou are speaking through the **Ocean CLI** — a one-shot terminal tool. No interactivity, just text output.\n"
        )
}

fn voice_surface_prompt(prompt: &str) -> String {
    format!(
            "{prompt}\n\n## Current client\n\nYou are speaking through **Leo (voice)** — a voice-only interface. Responses should be concise and spoken aloud. Do not use visual components.\n"
        )
}

/// Canonical surface flag for a `client_type` string. This is the single
/// source of truth shared by the per-turn flag stamp (Fix 2), the
/// surface-switch notice (Fix 3), and the per-surface profile lookup
/// (Fix 5). It is reconciled with the `ocean-agents` surface-profile
/// registry: every flag here maps 1:1 to an `assistants/<DIR>` profile
/// directory via [`surface_dir`]. Unknown clients get `[?]`.
pub fn surface_flag(client_type: Option<&str>) -> &'static str {
    match client_type {
        Some("surface-extension") => "BRWSR",
        Some("tui") => "TUI",
        Some("surface-web") => "WEB",
        Some("surface-tauri") => "TAURI",
        Some("cli") => "CLI",
        Some("leo-voice") => "VOX",
        Some("acp-zed") => "ACP",
        Some("surface-slack") => "SLACK",
        Some("surface-canvas") => "CNVS",
        Some("surface-mobile") => "MOBL",
        _ => "?",
    }
}

/// The `assistants/<DIR>` profile directory name for a `client_type`.
/// Mirrors [`surface_flag`] (same labels). This is the key the file-loaded
/// profile path resolves against in [`load_surface_profile_from`].
///
/// File-loaded profiles are implemented — the runtime prefers
/// `assistants/<surface_dir>/system.md` when present, falling back to const
/// seeds. Author profiles in `ocean-agents/assistants/<DIR>/`; loaded at
/// runtime, no rebuild.
///
/// Still parked for John: org file-tree / namespacing so many agents can
/// share one surface without their profiles/tools bleeding — symlink-vs-
/// resolver for composing agent-dir CLAUDE.md + the surface profile in one
/// `load_project_prompt` ancestor-walk.
#[allow(dead_code)]
pub fn surface_dir(client_type: Option<&str>) -> &'static str {
    surface_flag(client_type)
}

/// Root of the editable per-surface profile tree. ocean-agents owns the
/// content (`assistants/<DIR>/system.md`); the daemon only *reads* it at
/// turn time so a surface's role/SOPs/limits can be hot-reconfigured
/// without a Rust rebuild. Override with `OCEAN_ASSISTANTS_DIR`; default is
/// `assistants/` under the Ocean config dir.
fn assistants_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("OCEAN_ASSISTANTS_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    // Mirror the daemon's config-dir resolution (XDG / ~/.config/ocean-rs).
    dirs_config_dir().map(|c| c.join("assistants"))
}

fn dirs_config_dir() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("ocean-rs"));
        }
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".config").join("ocean-rs"))
}

/// Prefer an on-disk surface profile for this `client_type` over the
/// compiled-in const, resolved against an already-resolved optional
/// assistants root. Returns the file's contents when present and non-empty,
/// else `None` (caller falls back to the seed const). This is the R2
/// file-loaded seam — the consts stay as seed + fallback, but the editable
/// file wins, enabling hot-reconfigure (ocean-agents).
///
/// Production passes `assistants_root()` (real `OCEAN_ASSISTANTS_DIR` / config
/// dir); tests pass a temp root for isolation. `None` root means "no
/// assistants dir", so the caller takes the const fallback (OCEAN-285).
fn load_surface_profile_opt(
    assistants_root: Option<&Path>,
    client_type: Option<&str>,
) -> Option<String> {
    load_surface_profile_from(assistants_root?, client_type)
}

/// Inner form that reads from an explicit root — keeps the file-loaded
/// logic testable without mutating global env.
fn load_surface_profile_from(root: &Path, client_type: Option<&str>) -> Option<String> {
    let dir = surface_dir(client_type);
    if dir == "?" {
        return None;
    }
    let path = root.join(dir).join("system.md");
    let content = std::fs::read_to_string(&path).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Append the per-surface ("current client") section to a base prompt,
/// resolving any file-loaded surface profile against an explicit optional
/// assistants root rather than the process-global one (OCEAN-285 isolation
/// seam — see [`build_system_prompt_from`]). Production passes
/// `assistants_root()`; tests pass a temp root (or `None`).
fn append_client_type_from(
    prompt: &str,
    client_type: Option<&str>,
    assistants_root: Option<&Path>,
) -> String {
    // File-loaded surface profile wins when present (R2 / ocean-agents
    // hot-reconfigure). Falls through to the seed consts below otherwise.
    if let Some(profile) = load_surface_profile_opt(assistants_root, client_type) {
        return format!("{prompt}\n\n## Current client\n\n{profile}\n");
    }
    match client_type {
            Some("tui") => tui_surface_prompt(prompt),
            Some("surface-web") => web_surface_prompt(prompt, "Ocean Surface (web) — a browser PWA"),
            Some("surface-tauri") => web_surface_prompt(
                prompt,
                "Ocean Surface (Tauri desktop) — the native shell hosting the canonical Leptos/WASM Surface",
            ),
            Some("surface-extension") => extension_surface_prompt(prompt),
            Some("cli") => cli_surface_prompt(prompt),
            Some("leo-voice") => voice_surface_prompt(prompt),
            // Slack / Canvas / Mobile are first-class now (ocean-agents R3).
            // These are the daemon-side compiled fallbacks — real, surface-aware
            // profiles, not bare-label stubs. A file-loaded `assistants/<DIR>`
            // profile (resolved above) overrides them when present; this is what
            // the runtime falls back to when no on-disk profile exists. They
            // mirror the shape and intent of the authored ocean-agents profiles
            // (`assistants/SLACK/system.md` et al.).
            Some("surface-slack") => slack_surface_prompt(prompt),
            Some("surface-canvas") => canvas_surface_prompt(prompt),
            Some("surface-mobile") => mobile_surface_prompt(prompt),
            Some(other) => format!("{prompt}\n\n## Current client\n\nYou are speaking through an unknown client: `{other}`.\n"),
            None => prompt.to_string(),
        }
}

/// Slack surface — an Ocean assistant living *inside* a Slack workspace,
/// replying in threads/DMs/channels. Compiled fallback mirroring the
/// authored `assistants/SLACK/system.md` house profile (R3): concise,
/// thread-aware, Slack-mrkdwn-aware, canvas-aware. Overridden by a
/// file-loaded SLACK profile when one exists on disk.
fn slack_surface_prompt(prompt: &str) -> String {
    format!("{prompt}\n\n## Current client\n\n{SLACK_SURFACE_PROMPT}\n")
}

/// Canvas surface — rich, persistent artifact rendering (a Slack Canvas or
/// equivalent canvas surface) paired with an in-thread message. Compiled
/// fallback; overridden by a file-loaded CNVS profile when present.
fn canvas_surface_prompt(prompt: &str) -> String {
    format!("{prompt}\n\n## Current client\n\n{CANVAS_SURFACE_PROMPT}\n")
}

/// Mobile surface — a compact, on-the-go screen read in passing. Compiled
/// fallback; overridden by a file-loaded MOBL profile when present.
fn mobile_surface_prompt(prompt: &str) -> String {
    format!("{prompt}\n\n## Current client\n\n{MOBILE_SURFACE_PROMPT}\n")
}

/// Per-file byte budget for one project instruction file. An oversized
/// AGENTS.md/CLAUDE.md is clipped with an explicit marker instead of riding
/// into EVERY turn's system prompt whole — pre-cap, one bloated instruction
/// file anywhere up the ancestor walk taxed every turn's input tokens
/// forever, silently.
const MAX_PROJECT_PROMPT_FILE_BYTES: usize = 64 * 1024;
/// Total byte budget across all ingested instruction files. Files beyond
/// the budget are named-but-skipped so the model knows they exist.
const MAX_PROJECT_PROMPT_TOTAL_BYTES: usize = 192 * 1024;

fn load_project_prompt(start: &Path) -> String {
    const FILES: &[&str] = &[
        "AGENTS.md",
        ".ocean/AGENTS.md",
        "CLAUDE.md",
        ".pi/instructions.md",
    ];
    let mut found = Vec::new();
    for ancestor in start.ancestors() {
        for name in FILES {
            let path = ancestor.join(name);
            if let Ok(content) = std::fs::read_to_string(&path) {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    found.push((path, trimmed.to_string()));
                }
            }
        }
    }
    if found.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    let mut total = 0usize;
    for (path, content) in found {
        let mut body = content;
        if body.len() > MAX_PROJECT_PROMPT_FILE_BYTES {
            let mut cut = MAX_PROJECT_PROMPT_FILE_BYTES;
            while cut > 0 && !body.is_char_boundary(cut) {
                cut -= 1;
            }
            body.truncate(cut);
            body.push_str("\n[instruction file truncated to fit the prompt budget]");
        }
        if total.saturating_add(body.len()) > MAX_PROJECT_PROMPT_TOTAL_BYTES {
            out.push_str(&format!(
                "\n\n----- {} -----\n[skipped: project-instruction budget exhausted]",
                path.display()
            ));
            continue;
        }
        total += body.len();
        out.push_str(&format!("\n\n----- {} -----\n", path.display()));
        out.push_str(&body);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{build_system_prompt_from, load_surface_profile_from, surface_dir, surface_flag};
    use ocean_context::{ClaimStatus, Provenance};
    use ocean_memory::{
        Memory, MemoryId, MemoryKind, MemoryScope, MemoryStore, PrincipalId, SqliteMemoryStore,
    };
    use std::path::Path;
    use tempfile::TempDir;

    /// A fresh, empty, auto-cleaned assistants root. Building a system prompt
    /// against this root resolves NO on-disk surface profile (the dir holds
    /// no `<DIR>/system.md`), so `build_system_prompt_from` takes the
    /// compiled-in const fallback — the path these tests actually assert on.
    ///
    /// This is the OCEAN-285 isolation primitive: every prompt-building test
    /// pins its own temp root instead of letting the lookup fall through to
    /// the operator's real `~/.config/ocean-rs/assistants`. No process env is
    /// read or mutated, so parallel `cargo test` threads can't race, and the
    /// result never depends on whatever profiles happen to exist on the box.
    fn empty_assistants_root() -> TempDir {
        tempfile::Builder::new()
            .prefix("ocean-assistants-empty-")
            .tempdir()
            .expect("create temp assistants root")
    }

    /// An auto-cleaned assistants root seeded with a single
    /// `<surface_dir>/system.md` for `client_type`, holding `body`. Used to
    /// exercise the file-loaded-profile-wins path in isolation.
    fn seeded_assistants_root(client_type: &str, body: &str) -> TempDir {
        let root = empty_assistants_root();
        let dir = root.path().join(surface_dir(Some(client_type)));
        std::fs::create_dir_all(&dir).expect("create surface dir");
        std::fs::write(dir.join("system.md"), body).expect("write seeded profile");
        root
    }

    /// One bloated instruction file is clipped, not ingested whole — and a
    /// pile of files beyond the total budget is named-but-skipped. Pre-cap,
    /// a huge AGENTS.md anywhere up the ancestor walk rode into EVERY
    /// turn's system prompt in full.
    #[test]
    fn project_prompt_ingestion_is_budgeted() {
        let root = tempfile::Builder::new()
            .prefix("ocean-project-prompt-")
            .tempdir()
            .expect("temp project root");
        // A file 4x over the per-file cap.
        let big = "R".repeat(super::MAX_PROJECT_PROMPT_FILE_BYTES * 4);
        std::fs::write(root.path().join("AGENTS.md"), &big).unwrap();
        // A second file that pushes past the TOTAL budget once the first
        // (clipped to the per-file cap) is in. 3 more capped files would
        // exceed 192 KiB, so nest dirs so the walk finds several.
        let sub = root.path().join("a/b/c");
        std::fs::create_dir_all(&sub).unwrap();
        for anc in [root.path().join("a"), root.path().join("a/b"), sub.clone()] {
            std::fs::write(anc.join("CLAUDE.md"), &big).unwrap();
        }

        let prompt = super::load_project_prompt(&sub);
        assert!(
            prompt.len()
                < super::MAX_PROJECT_PROMPT_TOTAL_BYTES + super::MAX_PROJECT_PROMPT_FILE_BYTES,
            "total ingestion stays near the budget, got {} bytes",
            prompt.len()
        );
        assert!(
            prompt.contains("[instruction file truncated to fit the prompt budget]"),
            "oversized file carries the clip marker"
        );
        assert!(
            prompt.contains("[skipped: project-instruction budget exhausted]"),
            "beyond-budget files are named but skipped"
        );
    }

    #[test]
    fn file_loaded_surface_profile_wins_over_const() {
        // R2: an on-disk assistants/<DIR>/system.md must override the seed
        // const so a surface can be reconfigured without a rebuild. Isolated
        // against a temp root (auto-cleaned), never the real config.
        let root = seeded_assistants_root("surface-slack", "CUSTOM SLACK PROFILE FROM FILE");

        let loaded = load_surface_profile_from(root.path(), Some("surface-slack"));
        assert_eq!(loaded.as_deref(), Some("CUSTOM SLACK PROFILE FROM FILE"));

        // Unknown surface never resolves a file.
        assert!(load_surface_profile_from(root.path(), Some("who-knows")).is_none());
        // Missing file → None (falls back to const).
        assert!(load_surface_profile_from(root.path(), Some("tui")).is_none());

        // And the loaded file actually wins inside the full prompt build.
        let prompt = build_system_prompt_from(None, Some("surface-slack"), Some(root.path()), None);
        assert!(prompt.contains("CUSTOM SLACK PROFILE FROM FILE"));
    }

    #[test]
    fn missing_profile_root_falls_back_to_const() {
        let root = Path::new("/nonexistent/ocean/assistants/root");
        assert!(load_surface_profile_from(root, Some("surface-slack")).is_none());
    }

    #[test]
    fn surface_flag_taxonomy_is_canonical() {
        // Canonical map reconciled with the ocean-agents surface-profile
        // registry (addendum R1). These exact labels are load-bearing —
        // downstream keys its assistants/<DIR> tree against them, so a
        // rename here is a cross-repo break.
        assert_eq!(surface_flag(Some("surface-extension")), "BRWSR");
        assert_eq!(surface_flag(Some("tui")), "TUI");
        assert_eq!(surface_flag(Some("surface-web")), "WEB");
        assert_eq!(surface_flag(Some("surface-tauri")), "TAURI");
        assert_eq!(surface_flag(Some("cli")), "CLI");
        assert_eq!(surface_flag(Some("leo-voice")), "VOX");
        assert_eq!(surface_flag(Some("acp-zed")), "ACP");
        // Slack / Canvas / Mobile are first-class now (R3), not future.
        assert_eq!(surface_flag(Some("surface-slack")), "SLACK");
        assert_eq!(surface_flag(Some("surface-canvas")), "CNVS");
        assert_eq!(surface_flag(Some("surface-mobile")), "MOBL");
        // Unknown / absent → sentinel, never a panic.
        assert_eq!(surface_flag(Some("who-knows")), "?");
        assert_eq!(surface_flag(None), "?");
        // surface_dir mirrors surface_flag (same labels, one source).
        assert_eq!(surface_dir(Some("surface-slack")), "SLACK");
    }

    #[test]
    fn slack_and_canvas_have_real_arms_not_unknown_fallthrough() {
        // R3: the runtime must recognize these surfaces ahead of the
        // inbound path, so they don't resolve to "unknown client". Pinned to
        // an empty temp assistants root so the compiled fallback is exercised
        // (OCEAN-285) — never the operator's real ~/.config profiles.
        let root = empty_assistants_root();
        for ct in ["surface-slack", "surface-canvas", "surface-mobile"] {
            let prompt = build_system_prompt_from(None, Some(ct), Some(root.path()), None);
            assert!(
                !prompt.contains("unknown client"),
                "{ct} must have a real surface arm, not the fallthrough"
            );
            assert!(prompt.contains("## Current client"));
        }
    }

    /// OCEAN-173: slack / canvas / mobile must get *real* surface-aware
    /// profiles, not the old bare-label stub (base prompt + "You are
    /// speaking through **<label>**."). Each must carry genuine,
    /// surface-specific guidance, and must not bleed another surface's UX.
    #[test]
    fn slack_canvas_mobile_get_real_profiles_not_stub() {
        // This test asserts against the COMPILED FALLBACK profiles
        // (SLACK/CNVS/MOBL consts). `build_system_prompt` would otherwise
        // resolve an on-disk `assistants/<DIR>/system.md` first via the real
        // `assistants_root()` (OCEAN_ASSISTANTS_DIR, else
        // ~/.config/ocean-rs/assistants), and in any dev/CI box that has a
        // real SLACK/CNVS/MOBL profile that file would shadow the consts
        // under test — wrong/flaky, and a read of the operator's machine
        // state. Build against an empty temp root instead (OCEAN-285): the
        // file lookup finds nothing, the const fallback is exercised, and no
        // process env is touched (no save/restore race with sibling tests).
        let root = empty_assistants_root();

        let slack = build_system_prompt_from(None, Some("surface-slack"), Some(root.path()), None);
        // Slack-native: thread-aware, concise, mrkdwn-not-Markdown, canvas-aware.
        assert!(slack.contains("Slack surface UX"));
        assert!(slack.contains("thread"));
        assert!(slack.contains("Slack mrkdwn"));
        assert!(slack.contains("single asterisks"));
        assert!(slack.contains("Slack Canvas"));
        assert!(slack.contains("act only on inbound turns"));
        // Not the old stub one-liner, and not a web/HTML surface.
        assert!(!slack.contains("Responses render as HTML"));

        let canvas =
            build_system_prompt_from(None, Some("surface-canvas"), Some(root.path()), None);
        assert!(canvas.contains("canvas surface UX"));
        assert!(canvas.contains("artifact"));
        assert!(canvas.contains("append over overwrite"));
        assert!(canvas.contains("pair the canvas with a message"));

        let mobile =
            build_system_prompt_from(None, Some("surface-mobile"), Some(root.path()), None);
        assert!(mobile.contains("mobile surface UX"));
        assert!(mobile.contains("phone"));
        assert!(mobile.contains("answer-first"));
        assert!(mobile.contains("small screen"));

        // None of the three may be the bare-label stub: the stub had no
        // surface-specific "UX" section beyond the `## Current client`
        // header, so a real profile must add a dedicated guidance section.
        for (ct, p) in [
            ("surface-slack", &slack),
            ("surface-canvas", &canvas),
            ("surface-mobile", &mobile),
        ] {
            assert!(
                p.contains("surface UX"),
                "{ct} must carry a real surface-UX section, not a bare label"
            );
        }
    }

    #[test]
    fn web_surface_gets_leptos_component_guidance() {
        // Const fallback under test — pin an empty temp assistants root so a
        // real WEB profile on the box can't shadow it (OCEAN-285).
        let root = empty_assistants_root();
        let prompt = build_system_prompt_from(None, Some("surface-web"), Some(root.path()), None);

        assert!(prompt.contains("Leptos components"));
        assert!(prompt.contains("component_render"));
        assert!(prompt.contains("Responses render as HTML"));
    }

    #[test]
    fn tauri_surface_gets_leptos_component_guidance() {
        let root = empty_assistants_root();
        let prompt = build_system_prompt_from(None, Some("surface-tauri"), Some(root.path()), None);

        assert!(prompt.contains("Tauri desktop"));
        assert!(prompt.contains("canonical Leptos/WASM Surface"));
        assert!(prompt.contains("Leptos components"));
        assert!(prompt.contains("component_render"));
        assert!(!prompt.contains("does not render Leptos components"));
    }

    #[test]
    fn extension_surface_knows_it_is_docked_in_chrome() {
        // Tests the BUILT-IN (const) extension prompt — the fallback when no
        // on-disk BRWSR profile exists. Call the const builder directly (it
        // never file-loads) so a real
        // ~/.config/ocean-rs/assistants/BRWSR/system.md (the intended Fix-5
        // hot-reconfigure override) doesn't shadow the source under test.
        let prompt = super::extension_surface_prompt(super::BASE_SYSTEM_PROMPT);

        // Same rich component surface as the web PWA…
        assert!(prompt.contains("Leptos components"));
        assert!(prompt.contains("component_render"));
        // …but it must know it's the in-Chrome side panel attached to the
        // user's real browser, not a detached web app.
        assert!(prompt.contains("Chrome extension side panel"));
        assert!(prompt.contains("attached to the browser the user is looking at"));
        assert!(prompt.contains("browser_read_page"));
        // It must NOT claim to be a browser PWA like surface-web does.
        assert!(!prompt.contains("a browser PWA"));
    }

    #[test]
    fn project_prompt_loads_ocean_agents_md_from_ancestor() {
        let assistants = empty_assistants_root();
        let project = TempDir::new().expect("create project tempdir");
        let ocean_dir = project.path().join(".ocean");
        std::fs::create_dir_all(&ocean_dir).expect("create .ocean dir");
        std::fs::write(
            ocean_dir.join("AGENTS.md"),
            "OCEAN PROJECT CONTRACT FROM DOT OCEAN",
        )
        .expect("write .ocean/AGENTS.md");
        let nested = project.path().join("crates/example/src");
        std::fs::create_dir_all(&nested).expect("create nested cwd");

        let prompt = build_system_prompt_from(
            Some(nested.to_str().expect("nested path utf8")),
            Some("tui"),
            Some(assistants.path()),
            None,
        );

        assert!(prompt.contains(".ocean/AGENTS.md"));
        assert!(prompt.contains("OCEAN PROJECT CONTRACT FROM DOT OCEAN"));
    }

    #[test]
    fn tui_surface_advertises_terminal_component_projection() {
        let root = empty_assistants_root();
        let prompt = build_system_prompt_from(None, Some("tui"), Some(root.path()), None);

        assert!(prompt.contains("Ocean TUI"));
        assert!(prompt.contains("terminal-native"));
        assert!(prompt.contains("Use `component_render`"));
        assert!(prompt.contains("callout, progress, stat, chart"));
        assert!(!prompt.contains("Do not use `component_render`"));
        assert!(!prompt.contains("Leptos components from `component_render` events"));
    }
    // --- Grounded environment block ---------------------------------------
    //
    // The daemon's system prompt historically never stated the session's
    // working directory, so models hallucinated paths (observed live: a
    // provider ran `cd /home/ubuntu/agent-0 && ...` in a session whose
    // real cwd was elsewhere). `build_system_prompt_from` now appends a
    // grounded Environment block whenever the caller passes an explicit,
    // non-empty cwd. These four tests pin that contract.

    /// (a) With an explicit cwd and no project instructions, the prompt
    /// MUST contain the exact cwd path. Against the pre-fix prompt — a
    /// static `BASE_SYSTEM_PROMPT` with no cwd baked in and no instruction
    /// files to load — this assertion fails, which is the bug.
    #[test]
    fn environment_block_states_explicit_cwd() {
        let root = empty_assistants_root();
        // No AGENTS.md / CLAUDE.md here, so `load_project_prompt`
        // contributes nothing — the only way cwd can appear is the
        // Environment block.
        let project = TempDir::new().expect("project tempdir");
        let cwd_str = project.path().to_string_lossy().into_owned();

        let prompt = build_system_prompt_from(Some(&cwd_str), Some("tui"), Some(root.path()), None);

        assert!(
            prompt.contains(&cwd_str),
            "prompt must state the working directory; got a prompt without `{cwd_str}`"
        );
        assert!(prompt.contains("## Environment"));
        assert!(prompt.contains("Working directory:"));
    }

    /// (b) Inside a git repo the workspace-root label and the real branch
    /// name appear. The branch is queried from git (not hardcoded main /
    /// master — git's default differs by config and version), and the
    /// workspace-root path is never compared to the tempdir: `git
    /// rev-parse --show-toplevel` canonicalizes through symlinks
    /// (`/var` -> `/private/var` on macOS), so a path-equality check
    /// flakes locally while passing on Linux CI.
    #[test]
    fn environment_block_reports_git_branch_in_repo() {
        if std::process::Command::new("git")
            .arg("--version")
            .status()
            .map(|status| !status.success())
            .unwrap_or(true)
        {
            // No git on this box (some CI images) — not a regression.
            return;
        }

        let repo = TempDir::new().expect("repo tempdir");
        let repo_path = repo.path();
        let ok = |cmd: &mut std::process::Command, label: &str| {
            let status = cmd.status().expect(label);
            assert!(status.success(), "{label} failed with status {status}");
        };

        ok(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(repo_path),
            "git init",
        );
        // Pin the unborn branch up front so the first commit lands on a
        // known name regardless of `init.defaultBranch`.
        ok(
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo_path)
                .args(["symbolic-ref", "HEAD", "refs/heads/ocean-env-test"]),
            "git symbolic-ref HEAD",
        );
        std::fs::write(repo_path.join("README.md"), "ocean\n").expect("write seed file");
        ok(
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo_path)
                .args(["add", "README.md"]),
            "git add",
        );
        ok(
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo_path)
                .args([
                    "-c",
                    "user.name=Ocean Test",
                    "-c",
                    "user.email=ocean-test@example.invalid",
                    "commit",
                    "-q",
                    "-m",
                    "initial",
                ]),
            "git commit",
        );

        // Read the branch git actually reports (not the one we requested)
        // so the assertion is robust to any local git quirk.
        let branch = String::from_utf8(
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo_path)
                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                .output()
                .expect("git rev-parse HEAD")
                .stdout,
        )
        .expect("branch utf8")
        .trim()
        .to_string();

        let root = empty_assistants_root();
        let cwd_str = repo_path.to_string_lossy().into_owned();
        let prompt = build_system_prompt_from(Some(&cwd_str), Some("tui"), Some(root.path()), None);

        assert!(!branch.is_empty(), "test setup must produce a branch");
        assert!(
            prompt.contains(&format!("Git branch: {branch}")),
            "prompt must name the git branch ({branch})"
        );
        assert!(
            prompt.contains("Workspace root:"),
            "workspace-root line must be present inside a repo"
        );
        // Branch + short commit render as `branch @ <short>`.
        assert!(prompt.contains(" @ "));
    }

    /// (c) Outside any git repo, no git line appears — but the working
    /// directory line still does. (`tempfile` places the dir under the
    /// system temp root, never inside a repo.)
    #[test]
    fn environment_block_omits_git_lines_outside_repo() {
        let root = empty_assistants_root();
        let outside = TempDir::new().expect("non-repo tempdir");
        let cwd_str = outside.path().to_string_lossy().into_owned();

        let prompt = build_system_prompt_from(Some(&cwd_str), Some("tui"), Some(root.path()), None);

        assert!(prompt.contains("Working directory:"));
        assert!(
            !prompt.contains("Git branch:"),
            "no git branch line outside a repo"
        );
        assert!(
            !prompt.contains("Git commit:"),
            "no git commit line outside a repo"
        );
    }

    /// (d) With no cwd at all there is NO Environment block — we never
    /// invent a workspace by falling back to the process cwd. Deterministic
    /// regardless of where the test process happens to run.
    #[test]
    fn environment_block_absent_when_no_cwd() {
        let root = empty_assistants_root();
        let prompt = build_system_prompt_from(None, Some("tui"), Some(root.path()), None);

        assert!(
            !prompt.contains("## Environment"),
            "no Environment block when cwd is None"
        );
        assert!(
            !prompt.contains("Working directory:"),
            "no working-directory line when cwd is None"
        );
    }
    fn seed_memory(path: &Path, text: &str, kind: MemoryKind) {
        let mut store = SqliteMemoryStore::open(path).expect("open temp memory db");
        store
            .put(Memory {
                id: MemoryId::new(),
                scope: MemoryScope::Operator,
                owner: PrincipalId::new("operator"),
                kind,
                body: serde_json::json!({ "text": text }),
                provenance: Provenance {
                    anchors: Vec::new(),
                    tickets: Vec::new(),
                    commit_sha: String::new(),
                },
                trust: ClaimStatus::Asserted,
                seq: 0,
                written_at: 1,
                updated_at: 1,
                history: Vec::new(),
            })
            .expect("seed temp memory");
    }

    #[test]
    fn base_prompt_is_compact_and_defers_to_runtime_tool_schemas() {
        assert!(super::BASE_SYSTEM_PROMPT.len() < 2_500);
        assert!(super::BASE_SYSTEM_PROMPT
            .contains("Tool definitions supplied with the turn are authoritative"));
        assert!(!super::BASE_SYSTEM_PROMPT.contains("## What ocean-os is"));
        assert!(!super::BASE_SYSTEM_PROMPT.contains("browser_navigate"));
    }

    #[test]
    fn memory_prompt_describes_tools_and_auto_recalls_existing_facts() {
        let root = empty_assistants_root();
        let memory_dir = TempDir::new().expect("memory tempdir");
        let memory_db = memory_dir.path().join("memory.sqlite");
        seed_memory(
            &memory_db,
            "John prefers verified work landed to main.",
            MemoryKind::Preference,
        );
        seed_memory(
            &memory_db,
            "Ocean daemon health is GET /health.",
            MemoryKind::Fact,
        );

        let prompt =
            build_system_prompt_from(None, Some("tui"), Some(root.path()), Some(&memory_db));

        assert!(prompt.contains("## Memory"));
        assert!(prompt.contains("recall"));
        assert!(prompt.contains("retain"));
        assert!(prompt.contains("## What you already know"));
        assert!(prompt.contains("John prefers verified work landed to main."));
        assert!(prompt.contains("Ocean daemon health is GET /health."));
        assert!(prompt.contains("Only call `recall` when the request depends on information"));
        assert!(!prompt.contains("Use it at the START of substantive tasks"));
    }

    #[test]
    fn memory_prompt_omits_auto_recall_for_missing_database() {
        let root = empty_assistants_root();
        let memory_dir = TempDir::new().expect("memory tempdir");
        let missing = memory_dir.path().join("missing.sqlite");

        let prompt = build_system_prompt_from(None, Some("tui"), Some(root.path()), Some(&missing));

        assert!(prompt.contains("## Memory"));
        assert!(!prompt.contains("## What you already know"));
        assert!(
            !missing.exists(),
            "prompt build must not create a missing db"
        );
    }

    #[test]
    fn memory_prompt_is_absent_without_database_configuration() {
        let root = empty_assistants_root();
        let prompt = build_system_prompt_from(None, Some("tui"), Some(root.path()), None);

        assert!(!prompt.contains("## Memory"));
        assert!(!prompt.contains("## What you already know"));
    }

    #[test]
    fn auto_recalled_memory_is_truncated_at_two_hundred_characters() {
        let root = empty_assistants_root();
        let memory_dir = TempDir::new().expect("memory tempdir");
        let memory_db = memory_dir.path().join("memory.sqlite");
        let long_fact = "x".repeat(201);
        seed_memory(&memory_db, &long_fact, MemoryKind::Fact);

        let prompt =
            build_system_prompt_from(None, Some("tui"), Some(root.path()), Some(&memory_db));

        assert!(prompt.contains(&format!("{}…", "x".repeat(200))));
        assert!(!prompt.contains(&long_fact));
    }
}
