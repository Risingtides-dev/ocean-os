# OMP → Ocean Port Map

**Date:** 2026-07-03 · **Status:** Research complete; implementation audit refreshed 2026-07-15 · **Owner:** Smaths / Ocean
**Source:** deep source inspection of [oh-my-pi](https://github.com/can1357/oh-my-pi) (MIT, ~15.7k★) by 5 parallel research agents reading actual `.rs`/`.ts` files — not docs. This is the standing backlog: every feature worth reverse-engineering into Ocean, mapped to a destination crate, so features never need to be re-named one by one.

> **Ownership update (2026-07-13):** backlog entries that place subagent definitions, `task`/spawn, lifecycle, or orchestration in core are superseded. Those capabilities must ship as extensions over generic permission-gated execution/capability seams; do not port their original core placement.

## Organizing principle: harness profiles

Per John (2026-07-03): the IDE-grade harness is **surface-scoped, not global**. The daemon
resolves a **harness profile from `client_type`** (already on every turn):

| Profile | client_type | Harness level |
|---|---|---|
| `tui` | `tui` | Full IDE harness — LSP, hashline, rules/TTSR, rich context, artifacts, minimizer |
| `web` | `surface-web`, `surface-extension` | Lean conversational — memory + roles, no IDE machinery |
| `voice` | `leo-voice` | Minimal — short context, no tools beyond essentials |
| `acp` | ACP/editor clients | IDE harness minus TUI-only rendering |

Every feature below is tagged with its profile home. The **profile seam itself is the first
build**: a `HarnessProfile` resolved per-turn in ocean-daemon that gates which capabilities are
live. New features then attach to a profile in one line.

## Proposed crate layout (ocean-os workspace)

| Crate | Contents | Port source |
|---|---|---|
| `ocean-hashline` (new) | file-hash anchors, patch dialects, snapshot store, recovery | `packages/hashline/` (TS→Rust reimplement) |
| `ocean-ast` (new) | ast-grep-core wrapper, structural edits, `summarize_code` fold/unfold | `crates/pi-ast` (lift patterns) |
| `ocean-walker` (new) | parallel FS walker, TTL+mtime scan cache, invalidate_path | `crates/pi-walker` |
| `ocean-search` (new or in walker) | in-process grep (grep-regex/searcher), typed results | `crates/pi-natives/src/grep.rs` |
| `ocean-minimizer` (new) | per-command output filters (TOML defs), artifact spill footer | `crates/pi-shell/src/minimizer/` |
| `ocean-iso` (new, later) | CoW worktree isolation (APFS clonefile) for subagents | `crates/pi-iso` |
| `ocean-daemon` (extend) | harness profiles, tool harness (output-meta, artifacts, bm25 discovery, resolve protocol), TTSR, compaction, checkpoint/rewind | `packages/coding-agent/src/{tools,session}` |
| `ocean-agent` (extend) | models catalog (ocean.toml), role grammar v2, path-scoped policy, retry/fallback chains | `src/config/{model-registry,model-resolver,settings-schema}.ts` |
| `ocean-context` (extend) | retain/recall/reflect memory verbs, session→mental-model reflection | `tools/memory-*.ts`, hindsight |
| `ocean-tui` (extend) | streaming markdown (prefix-freeze), diff cards, composer stack, theme tokens, status segments, image budget | `packages/tui/` + `modes/components/` |
| `ocean-acp` (extend) | models as session config option, host-tool callback pattern | `src/modes/acp/` |

---

## Slice 1 — Tool layer (32 tools + harness)

Source: `packages/coding-agent/src/tools/` · behavior contracts in `prompts/tools/*.md`
(Handlebars-templated per session — schemas mutate with enabled features).

### Harness mechanics (as valuable as the tools) — profile: all, gated per-profile

- **Output-meta + artifact spill** (`tools/output-meta.ts`, `session/streaming-output.ts`) —
  every tool's output goes through a universal wrapper: truncation notices state direction
  (head/tail/middle) + exact shown ranges + `nextOffset`; full output spills to a session
  **artifact** readable via `read artifact://<id>`. Nothing is ever lost; context stays small.
  → ocean-daemon tool layer. **Foundational; build early.**
- **BM25 tool discovery** (`tools/search-tool-bm25.ts`) — only 6 essential tools ship in the
  request (`read,bash,edit,write,glob,eval`); the rest are BM25-indexed by
  name/label/description/schema-keys and activated per-session via `search_tool_bm25(query)`.
  Huge context savings. → ocean-daemon. Trivial port.
- **resolve / preview-accept** (`tools/resolve.ts`) — any tool stages a preview (diff/plan) +
  apply/reject closures; model calls `resolve {action, reason}`. Non-forcing (reminder first,
  forced tool_choice only on non-compliance) to preserve prompt cache. Failed applies keep the
  preview pending. → ocean-daemon.
- **ToolSession DI** — tools are classes taking a fat context object (~90 capability closures),
  no globals; subagents get filtered copies. `approval` tier + `loadMode` + `intent()` one-line
  human gloss per tool. → the shape for ocean-daemon's tool registry.
- **Deferred LSP diagnostics** — queued to next yield with staleness checks keyed on per-file
  mutation counters. **Guards**: `auto-generated-guard` (refuse edits to generated files),
  `plan-mode-guard`, hashline `noop-loop-guard` (repeated byte-identical edits escalate to errors).

### File I/O — profile: tui/acp

- **read** (`tools/read.ts`, 124KB — the crown jewel) — ONE `path` param handles: code files
  (tree-sitter structural summary by default, bodies elided, recovery selector in footer),
  multi-range selectors (`:50-200,960-973`, `:raw`, `:conflicts`), directories, archives
  (`zip:inner/file:50-60`), SQLite (`db:table:key`, `?q=SELECT`), PDFs/Office, notebooks,
  images, URLs (reader mode), internal URIs (`artifact://`, `skill://`, `pr://N/diff/<i>`,
  `ssh://host/path`). Records hashline snapshots. The selector grammar is the reusable core.
- **edit** — one tool, four dialects: `replace` (fuzzy-whitespace), `patch` (anchor hunks),
  `apply-patch` (Codex), `hashline`. LSP write-through: format-on-write + diagnostics batched
  per message, deduped via a ledger. → ocean-daemon + ocean-hashline.
- **hashline** (see dedicated section below) · **write** (archives/SQLite rows/`conflict://N`
  splicing) · **ast_grep/ast_edit** (structural codemods, preview→resolve) ·
  **grep/glob** (native Rust already — typed results, mtime-sorted, cross-line auto-detect).

### Execution — profile: tui/acp

- **bash** — persistent session; `cwd`/`env` params instead of `cd`/exports; optional PTY;
  `async: true` → job ID; auto-background after 60s. **Output minimizer**: ~70 per-command
  filters (cargo, git, gh, npm, pytest…) + TOML defs collapse pass-noise to failures, with
  `[raw output: artifact://id]` preserving exact bytes. → ocean-minimizer.
- **eval** — persistent kernels (IPython, JS VM, Ruby, Julia); state survives across calls AND
  subagents; prelude: `tool.<name>(args)` re-enters session tools from code, `completion()`,
  `parallel()`, `budget`. Heavy port; consider embedded deno/python later.
- **job** — background lifecycle; results self-deliver on completion; `poll` blocks until any
  watched job or a steering message. **ssh** — remote exec, `ssh://` paths in read/write/grep.
- **debug** — full DAP client (breakpoints/step/stack/variables/evaluate; gdb, lldb-dap,
  debugpy, dlv). Moderate: DAP is JSON-RPC.

### Code intelligence — profile: tui/acp

- **lsp** (`lsp/index.ts`, 82KB) — multiplexed servers; definition/references/hover by
  `file+line+symbol-substring` (`#N` disambiguator — errors rather than guesses); workspace
  `rename`/`rename_file` (rewrites imports/barrel files); `code_actions` apply-by-title;
  glob diagnostics; raw `request` escape hatch. Heavy but top-tier value.

### Orchestration — profile: tui/acp

- **task** — batch `tasks[]` + shared `context`; enforced assignment format
  (Target/Change/Acceptance); async spawn, results self-deliver; `isolated: true` = CoW
  worktree returning patches (→ ocean-iso); concurrency caps; agent roster from `.md` files
  (≈ Ocean's folder-as-agent — converges).
- **irc** — in-process agent-to-agent DMs/broadcast; sends wake parked agents; parent→child
  becomes steering. **todo** — phased list, tasks addressed by *verbatim content string*, no
  IDs (eliminates fabricated-ID bugs); earliest-open auto-promotes.
- **yield** (hidden) — subagent structured completion, schema from caller's outputSchema.
  **ask** — multi-question option picker, recommended defaults, auto-"Other".
  **checkpoint/rewind** — mark → explore → rewind context keeping only the report:
  context-cost control *as a tool*.

### Web / external / media — profile: tui/acp (browser: ocean has its own CEF direction)

- **web_search** — 20+ providers behind one interface with fallback. **github** — op-based gh
  wrapper: `pr_checkout` into worktrees, `run_watch` fast-fails on first job failure + logs to
  artifact, response cache invalidated by intercepting mutating `gh` bash commands.
- **browser** — named persistent tabs; `run` executes JS with `tab.observe()` (a11y snapshot,
  stable ids), ariaSnapshot w/ `[ref=eN]`; 14 stealth patches. NOTE: Ocean's browser direction
  is CEF-embed (see docs/OCEAN_BROWSER_CONTROL_SURFACE.md) — steal the *control surface*
  (observe/ref/act pattern), not puppeteer.
- **inspect_image** — sub-LLM vision call, returns text only (keeps images out of main
  context). **generate_image/tts** — multi-provider, auto-fallback.

### Memory — profile: all (this is Ocean Context's territory)

- **retain/recall/reflect/memory_edit/learn** — pluggable backends; BM25 recall;
  LLM-synthesized reflect; `learn` mints managed SKILL.md dirs. Maps directly onto
  ocean-context/OKF — port the *verbs*, keep Ocean's storage.

---

## Slice 4 — Terminal UI (for ocean-tui)

Source: `packages/tui/` (engine, fork of pi-tui) + `modes/components/` (~70 files).
Docs: `docs/tui-core-renderer.md`, `docs/tui-runtime-internals.md`, `docs/theme.md`.

- **Append-only native scrollback** — THE load-bearing design. No alt-screen for the
  transcript: finalized blocks are physically committed to the terminal's native scrollback
  (native selection, persists after exit) and never rewritten; only the live tail repaints via
  row-diffs, with a stable-prefix ratchet committing the settled head of a long streaming
  reply. Port note: ratatui `Viewport::Inline` + `Terminal::insert_before`; hybrid for the
  CTRL workbench — keep panes on alt-screen, adopt freeze-finalized-blocks inside the chat
  pane. *This is the architecture that makes streaming rock-solid.*
- **Streaming markdown w/ prefix-freeze** (`markdown.ts`, 2,075 lines) — cache the largest
  blank-line-bounded prefix whose tokens are frozen; re-lex only the grown tail
  (`lex(prefix) ++ lex(tail) === lex(prefix+tail)`). Tables in sharp box-drawing, LaTeX→unicode,
  optional Kitty double-height H1s. Port: same design over pulldown-cmark; cache rendered
  `Line`s per frozen block.
- **Edit-preview diff cards** — framed blocks (rounded corners), line numbers, syntax-highlighted
  content, **word-level intra-line diffs via SGR inverse** (`Diff.diffWords` → `similar` crate +
  `Style::REVERSED`), dim `·`/`→` indentation guides, hunk truncation with expand hints, LSP
  diagnostics appended. **Live previews from partial streaming JSON args**, trailing unbalanced
  `-` lines stripped so previews never flash bogus deletions.
- **Tool cards** — status icon + title + collapsed tail-window preview; ctrl+o global expand;
  consecutive reads coalesce; parallel spinners phase-locked.
- **Composer stack** (`editor.ts` 3,140 lines + `autocomplete.ts` 1,026) — undo, kill ring,
  persisted prompt history (SQLite), ctrl+r fuzzy history search, bracketed-paste with
  large-paste collapse, ctrl+v image paste, ctrl+g external $EDITOR. One autocomplete provider
  trait (`getSuggestions(textBeforeCursor) → items + replace-range`) handling `/` commands,
  `@file` fuzzy refs (native fuzzyFind), Tab paths, `:emoji:`. Mode prefixes: `!` bash,
  `$` python (border recolors).
- **Slash commands** (`builtin-registry.ts`, 2,554 lines) — ~50 built-ins incl. `/goal`
  (set/budget/pause), `/advisor`, `/export` HTML, `/collab` (live relay + QR), `/fork`,
  `/tree` (session DAG), `/handoff`, `/compact`, `/memory`. File commands with `$1..$n`
  templating; unknown `/x` falls through to the LLM. Discovery from `.omp/`,
  `~/.claude/commands`, plugins (priority-deduped).
- **Images** — protocol ladder: Kitty graphics → iTerm2 → sixel (Rust `icy_sixel`) → text.
  `ImageBudget`: only newest N live; older demote to height-preserving text fallback so
  committed rows never shift.
- **Status line** — 24 composable segments (model, git ±, PR, subagents, token rate, cost,
  context %, cache hit…), 5 presets. Shimmer loaders (sweep/KITT, velocity fixed
  cells-per-second). Subagent HUD with live nested progress trees. Desktop notifications via
  OSC 9/99 when backgrounded.
- **Session picker** — fullscreen overlay, mouse, fuzzy across id/title/cwd/messages + SQLite
  history, current-folder vs all-projects toggle, lifecycle glyphs, delete-from-picker.
- **Advisor + TTSR rendering** — advisor = distinct-voice card (severity-tinted rail, "+N
  more" collapse); TTSR = inverse-warning box "⚠ Injecting rule: name ⏪", consecutive
  violations merge. ~100 lines each in ratatui.
- **Theming** — ~66 color tokens (JSON), 80+ built-in themes, auto dark/light via OSC 11,
  symbol presets (unicode/nerd/ascii). **Adopting the token schema as serde structs gives
  ocean-tui compatibility with their entire theme library for free.**

Priority for ocean-tui: (1) streaming markdown prefix-freeze, (2) diff cards w/ word-level
inverse, (3) composer stack, (4) theme token schema, (5) ImageBudget/sixel, (6) status segments.

---

## Slice 5 — Config + model routing (for ocean-agent / ocean.toml)

Source: `src/config/` (`settings-schema.ts` ~5,200 lines, `model-registry.ts`,
`model-resolver.ts`), `docs/models.md`, `docs/providers.md`, `docs/settings.md`.

- **models catalog** (`models.yml`) — per-provider: `baseUrl`, `apiKey`
  (env-name-or-literal, **`!cmd` runs a shell command for the secret**), `api` (8 wire
  protocols), `auth` (apiKey/none/oauth), `discovery.type` (ollama/llama.cpp/lm-studio
  auto-discovered keylessly), `modelOverrides`, `models[]` with `cost` quad, `contextWindow`,
  `reasoning`, `contextPromotionTarget`, ~35-knob `compat` block (thinking formats,
  reasoning-effort maps, strict-mode). Merge: bundled catalog → user file → overrides →
  runtime-discovered (SQLite cache + fingerprint). Bad file never breaks the registry.
- **Role grammar v2** — roles: `default, smol, slow, vision, plan, designer, commit, tiny,
  task, advisor` + custom via modelTags. Values: `provider/model`, canonical id, or another
  role (`pi/smol`) — cycle-safe resolution with visited set; `:off|minimal|low|medium|high|xhigh`
  thinking suffix on any value; referring role's suffix wins. Fallback chains per role
  (`advisor` aliases `slow`, never the primary — deliberate). `tiny` runs background jobs:
  session titles, memory extraction, auto thinking-difficulty classification.
  → **subsumes Ocean's v1 [roles]; this is roles v2.**
- **Routing** — path-scoped `enabledModels`/`disabledProviders`
  (`{pathPrefix: …, models: […]}`) = per-repo model policy without per-repo files;
  **contextPromotionTarget**: on context_length_exceeded, promote to bigger-context sibling
  (temporary, role mapping untouched) BEFORE compacting; `cycleOrder` for Ctrl+P;
  canonical-vs-pinned model selection semantics.
- **Provider layer** — ~45 providers; auth resolution: flag → config key → stored key →
  stored OAuth (auto-refresh, multi-account rotation) → env cascade (4 .env files) → fallback.
  OAuth subscriptions: anthropic/codex/copilot/cursor/gemini-cli/qwen/kimi. Retry:
  maxRetries=10 exp backoff + **model fallback chains** with cooldown-expiry revert;
  per-model cost quad feeds session stats.
- **Settings** (~300 keys) — thinking budgets per level, sampling knobs, service tiers,
  `tools.approvalMode` (always-ask/write/yolo) + per-tool allow/deny/prompt,
  compaction strategies (`snapcompact|handoff|shake|context-full|off`), TTSR knobs, task
  isolation modes, memory backends, loop guards, magic keywords. Precedence: defaults <
  global < project (no ancestor walk) < --config < flags. Objects deep-merge, arrays replace.
- **RPC/ACP** — RPC: JSONL stdio; **host tools** (`set_host_tools` → host_tool_call/result) and
  **host URI schemes** (virtual files bounced to the host) — lets an editor own tools over the
  pipe. ACP: official SDK; models exposed as a session **config option**; slash commands via
  available_commands_update; "launch TUI for terminal auth" flow. → steal both for ocean-acp.

---

## Slice 2 — Rust crates (crates/ workspace, all MIT, workspace v16.3.4)

Real crate list is 8: pi-walker, pi-ast, pi-iso, pi-shell, pi-uu-grep, pi-uutils-ctx,
pi-natives, + vendor/ (brush 0.5.0 fork, 11 patched uutils crates). Everything
MIT-compatible; carry LICENSE + pi-shell NOTICE. Git-dependency viable for the library
crates (pi-shell needs fork-and-vendor because its `[patch.crates-io]` brush entries don't
propagate through git deps).

- **pi-walker** (~4,600 LOC) — **lift as-is; highest value.** Pure-Rust traversal engine with
  hand-rolled platform fast paths: `getdents64` (Linux), `getattrlistbulk` (macOS),
  `NtQueryDirectoryFile` (Windows). Builder API, per-level gitignore stack, heartbeat
  closures for cancellation, ranked top-N (`MtimeDescPathAsc`), streaming visitors with
  pruning. Cache: global DashMap keyed `(root, options)`, 1s TTL, `invalidate_path` clears
  any root containing a mutated path. → `ocean-walker` (or direct git dep). Feeds glob/grep/
  context-ingest immediately.
- **pi-iso** (~3,500 LOC) — **lift as-is.** One trait (`probe/start/stop/diff`), backends:
  APFS `clonefile(2)` (whole-tree reflink, one syscall — the Mac path), btrfs/ZFS snapshots,
  Linux FICLONE reflink, overlayfs (+fuse fallback), Windows ProjFS/ReFS, and `rcopy`
  fallback that uses `git worktree add --detach` when lower is a repo. diff.rs: git diff +
  untracked, or parallel two-tree walk with (size,mtime) short-circuit. → `ocean-iso`;
  slots straight into subagent worktree isolation.
- **pi-ast** (~3,300 LOC) — **lift as-is, prune grammars.** `language/` is a vendored fork of
  ast-grep-language (60 grammars — trim to Ocean's languages; the build cost is the only
  tax). `ops.rs` wraps pattern search/rewrite; `block.rs` resolves "the syntactic block
  starting at line N" (powers `SWAP.BLK`); `summary.rs` (1,380 LOC) is the crown jewel:
  elidable-span forest + BFS unfold outer→inner to a visible-line budget, with an
  unfold-limit so one huge function can't starve siblings. → `ocean-ast`.
- **pi-shell** (~30k LOC incl. filters) — **fork and SPLIT; don't take whole.**
  (a) `minimizer/` (~25k): output-compression engine — brush-parser command analysis that
  refuses to touch pipes, ~25 Rust filters (git.rs 92KB, jvm.rs 124KB, cargo, docker…) + 9-stage
  declarative TOML pipelines with ~75 built-in per-command defs, xxHash64 trust-hash on user
  defs, 4MiB cap → raw fallback, never fakes success. **Port first** → `ocean-minimizer`.
  (b) `process.rs` (1,755): cross-platform process-tree management (children/kill/group).
  (c) `fixup.rs`: AST pre-exec rewrites stripping `| head` / redundant `2>&1`.
  (d) The brush+uutils embedded shell (coreutils as in-process builtins via `uutil_builtin!`
  + thread-local stdio/cwd ctx in pi-uutils-ctx): heavy, ~2.1MB vendored — **phase 2, only
  if Ocean commits to owning a shell runtime.**
- **pi-uu-grep** (2,459 LOC) — lift as-is: grep/rg from scratch on ripgrep's libraries,
  recursion via pi-walker, GNU exit codes, never exits the process. Independently useful as
  Ocean's embedded grep.
- **pi-natives** — **skip the crate (it's the N-API cdylib), harvest modules:** `text.rs`
  (ANSI-aware width/truncate), `grep.rs` (grep-searcher orchestration), `highlight.rs`
  (syntect → 11 semantic categories), `snapcompact.rs` (renders conversation text to PNG via
  bundled pixel fonts for VISION-model compaction — wild and worth keeping in mind),
  `pty.rs`, `sixel.rs`, `tokens.rs` (tiktoken-rs), `keys.rs` (Kitty protocol).

Port order per the crates agent: (1) pi-walker, (2) pi-iso, (3) pi-ast trimmed,
(4) minimizer + process, (5) shell engine only on commitment, (6) cherry-pick natives modules.

## Slice 3 — Agent runtime (the harness intelligence)

Source: `packages/agent/` (loop, compaction primitives) + `packages/coding-agent/src/`
(`session/`, `capability/`, `discovery/`, `task/`, `plan-mode/`, `hindsight/`, `irc/`,
`export/ttsr.ts`). Agent kept a clone at `/tmp/oh-my-pi` for follow-ups.

- **TTSR (time-traveling stream rules)** — rules are markdown + YAML frontmatter:
  `condition` (regex), `astCondition` (ast-grep, edit/write streams), `scope`
  (`text|thinking|tool:edit(*.ts)`), `interruptMode`, `globs`. Per-stream buffers keyed by
  source; regex runs per-delta on the whole buffer; ast matching throttled by
  identical-snapshot skip. On interrupt-worthy match: abort the stream →
  `replaceMessages(slice(0, targetAssistantIndex))` (drop the partial turn) → append hidden
  `ttsr-injection` message with the rule body → continue: **the model regenerates from
  before its mistake with the rule in context.** Non-interrupting matches fold into the tool
  result. Cost controls: `repeatMode: once` + repeatGap, injected names persisted per
  session. ~27 builtin rules (e.g. `ts-no-any`). Port: a `StreamRuleEngine` between provider
  stream and event bus; rewind = truncate message vec + hidden system-reminder + re-issue.
  → ocean-daemon/ocean-runtime, profile: tui/acp.
- **Hindsight memory** — two backends. Remote: auto-recall on first turn (`<memories>` block
  w/ anti-injection framing), auto-retain every N user turns (async), **mental models** =
  named server-refreshed summaries spliced into developer instructions on a TTL. **Local
  (the porting target):** SQLite two-stage pipeline at session startup — stage 1 summarizes
  idle rollouts per thread (lease-claimed jobs, concurrency 8), stage 2 consolidates into
  `memory_summary.md` injected as developer instructions. Port the retain/recall/reflect
  verbs over ocean-context's storage; the rollout+lease pipeline maps to the daemon's bus.
  Profile: all.
- **Checkpoint/rewind** — conversation-only context-cost tool: `checkpoint(goal)` marks;
  explore; `rewind(report)` → `branchWithSummary` (the session store is a **branching entry
  tree** — exploration becomes a dead branch) → live context rebuilt with a rewind-report
  message. Yield is blocked while a checkpoint is open. Port requires Ocean sessions to gain
  branch links (entry id + parent). Profile: tui/acp.
- **Compaction pipeline** — ordered: (1) **promote** to `contextPromotionTarget` sibling
  (ephemeral model switch, skips compaction entirely), (2) **prune** (drop old tool outputs,
  superseded-first, keep protectTokens recent), (3) **shake** (replace whole tool results +
  large fences with placeholders), (4) **summarize** (`context-full` LLM summary tracking
  read/modified files — or **snapcompact**: render history to pixel-font PNGs a vision model
  reads back; deterministic, no LLM call). Checked post-turn, pre-prompt, AND mid-tool-loop.
  **Protection matchers** keep skill reads / the active plan / pinned entries alive through
  prune+shake. reserveTokens floored at 15% of window. Profile: all (strategy per profile).
- **Plan mode** — threefold enforcement: hard write-guard (writes only inside the `local://`
  plan sandbox), system-prompt block, and settle-time escalating reminders until the model
  calls `ask`/`resolve`; exit only via `resolve(apply)` approved by the host. Plan file is
  compaction-protected and handed to subagents. Autonomous wake turns suppressed in plan
  mode. Port: mode flag in tool dispatch + prompt block + turn-settle hook. Profile: tui/acp.
- **Subagents (task)** — spawns are subprocesses of the same binary; **isolation = CoW
  filesystem snapshots via pi-iso** (APFS clonefile on Mac — NOT git worktrees): capture git
  baseline incl. staged/untracked, run in the CoW mount, diff, commit to a task branch,
  merge back. Typed returns: declared outputSchema, `yield` assembly (incremental sections +
  terminal yield), schema-validated with fallback extraction. **IRC bus**: process-global
  mailboxes; send never blocks — busy agents get an aside at next step boundary, idle get a
  wake turn, parked get revived. Concurrency: **per-provider semaphores around HTTP** (not
  per-spawn caps) so deep trees can't deadlock; spawn-policy gates depth. Port: IRC → bus
  topics per agent id + delivery policy; isolation → ocean-iso. Profile: tui/acp.
- **Context assembler** — system prompt built per rebuild under a hard prep deadline with
  per-source `withDeadline` fallbacks (a slow repo scan never blocks startup): AGENTS.md
  walk-up (one per depth, `@`-imports expanded), skills, workspace tree (capped), rule
  index. Per-turn additions are event-driven, not re-collected. Internal URL schemes
  (`rule://`, `skill://`, `local://`, `artifact://`) are the load-bearing pattern. Profile:
  budgets differ per profile.
- **Rules/config capability system** — typed capabilities (rules, context files, skills,
  prompts, hooks, MCP) fed by prioritized discovery providers reading native + Cursor +
  Windsurf + Claude + Codex + Gemini formats, first-wins dedup. **Three-tier rule
  delivery — the key design:** always-apply → verbatim in system prompt; description-only →
  indexed in a rulebook, fetched on demand via `rule://`; condition-bearing → TTSR
  enforcement. Maps directly onto OKF/folder-as-agent capability binding. Profile: all.

## Hashline (already fully decoded — prior session research)

Anchor = whole-file tag: `xxHash32(normalized file, seed 0) & 0xffff` → 4 hex chars, after
LF-normalization + trailing-whitespace strip (hash input only). Wire: `[path#HASH]` +
`SWAP 5.=7:` / `DEL` / `INS.PRE/POST/HEAD/TAIL` / `SWAP.BLK` (tree-sitter span) / `MV/REM`.
Stale: re-hash live file vs tag → structured MismatchError ("re-read, never invent the tag").
Recovery: 3 ordered strategies against the named snapshot, **zero fuzz** — applies cleanly or
fails loud. Snapshot store: LRU 30 paths × 4 versions, realpath-keyed, records `seenLines`
(edits into unread regions rejected). Their test suite (`packages/hashline/test/`) is the
port contract. ~61% output-token reduction claimed. → `ocean-hashline`, profile: tui/acp.

## Implementation status audit (2026-07-15, source + live-composition sweep)

This is the current checkpoint against the original map, not a claim that every Ocean feature
originated in OMP. Source symbols and the canonical package index are authoritative; this dated
summary exists to keep the backlog honest.

| Wave | Status | Current evidence | Remaining gap |
|---|---|---|---|
| W0 — profile seam | **Partial** | `ocean-daemon::harness_profile` resolves per turn and defines all bundles. `PromptControl::with_hashline_edits` and `with_artifact_spill` carry two gates into runtime composition. | LSP and memory are registered as universal providers rather than controlled through the profile seam; stream rules, rich context, and minimizer remain declarative. ACP is not selected by `client_type`. |
| W1 — edit reliability | **Built** | `ocean-hashline`, session snapshots, `HashlineEditTool`, `NoopLoopGuard`, `SpillingTool`, and `artifact://` retrieval are live. | Output metadata is thinner than OMP's complete directional/range protocol, and the broader hashline dialect remains out of scope. |
| W2 — TUI streaming | **Strong partial** | `shell/markdown.rs` implements prefix-freeze; `shell/diff.rs` has word-level inverse diffs; the composer has persisted history, Ctrl-R, a kill ring, mentions, and a fuzzy slash palette; tool drawers and a basic Kitty image viewer are live. | No append-only native-scrollback architecture, OMP theme-token/80-theme compatibility, full image protocol ladder/ImageBudget, or complete composer integration set. |
| W3 — context economy | **Partial** | Artifact spill is live. `ocean-agent::compact_history` deterministically elides older tool bodies and `ocean-runtime::trim_to_context_window` provides the hard suffix bound. `ocean-ast` implements structural summaries as a standalone crate. | No BM25 tool discovery, command minimizer, promote/prune/shake/summarize pipeline, protection matchers, live general AST-read wiring, or conversational checkpoint/rewind. |
| W4 — roles/catalog | **Partial** | Flat `[roles]`, per-turn model/advisor control, provider readiness, OAuth/API-key routing, retry, and cross-provider fallback are live. | No cycle-safe role aliases, thinking suffix grammar, per-role fallback, `tiny` role, path policy, configurable rich catalog, promotion target, or timed fallback cooldown/revert. |
| W5 — code intelligence | **Substantial partial** | `ocean-lsp::LspProvider` is live with diagnostics, definition, references, hover, symbols, rename, and reload. Built-in grep supports Rust regex with literal fallback. | No shared walker/search engine, cross-line typed/mtime-ranked search, `ast_edit`, generic preview/resolve, rename-file/code-actions/raw request, or mutation-counter write-through diagnostics. |
| W6 — rules/TTSR | **Absent** | The profile names `stream_rules`; no runtime engine is wired. | Typed rule capability taxonomy, `rule://`, three-tier delivery, abort/rewind/inject, and a builtin pack remain unimplemented. |
| W7 — orchestration | **Superseded in core** | Session-scoped todo plus `retain`/`recall` are live Ocean-shaped mechanisms. | General task/spawn/IRC/typed-yield orchestration is extension-owned under the current architecture. CoW isolation, reflect/hindsight, and plan mode need separately approved extension-facing designs rather than the original core placement. |

### Corrections to the 2026-07-05 snapshot

- `NoopLoopGuard` is wired into `hashline_edit`; it is no longer dead code.
- Grep is regex-first with explicit literal fallback; it is still not the planned typed search engine.
- `ocean-lsp` is a live capability provider rather than a dormant profile flag.
- `retain` and `recall` are live over the typed SQLite memory store; BM25 and `reflect` remain open.
- W2 advanced materially: prefix-freeze, inverse diff cards, composer history/search/kill ring,
  slash discovery, expandable tool drawers, and basic Kitty image display are implemented.
- Compaction is no longer only drop-oldest: old tool-result bodies are elided deterministically
  before the runtime's hard context-window trim. The richer OMP tiers remain absent.
- The original W7 core destination is invalid. General orchestration must ship through extensions
  over generic permission-gated execution, cancellation, capability, and event/tool seams.
- Post-turn advisor attribution, its dedicated two-permit/no-backlog limiter, fixed 30-second
  timeout, and fixed-cardinality outcome/latency metrics are live; richer review evidence and
  durable notes remain separate privacy/persistence decisions.
- The default-on Longhouse consult now has a path-redacted explained-ranking route and a shared
  fixture-backed exact-token ranker for skills/workflows. It remains local, deterministic,
  read-only, time-bounded, fail-open, permission-neutral, and separate from council/convene.

### Next high-leverage sequence

1. Reconcile which capabilities are truly surface-scoped and carry that policy through one runtime
   composition seam rather than logging unused booleans.
2. Build the command minimizer, then the shared walker/search substrate.
3. Treat every W7 item as extension work; do not revive the superseded core task/spawn design.

## Scoping principle (John, 2026-07-03)

**Port the mechanism, skip the integration long-tail.** ~99% of what OMP does to steer the
harness is valuable; the wide per-integration option surface is not. Concretely: the LSP
*tool* (multiplexing, rename, deferred diagnostics) ships with servers for the languages
Ocean actually works in (Rust, TS/JS, Python, Go, shell) — no Ruby-on-Rails-grade long tail.
Same rule everywhere: minimizer filters for our commands (cargo/git/gh/npm/pytest), grammars
for our languages, web-search providers we'd use. The mechanism must make adding an
integration a config entry, never a code change.

**Closed TUI gap:** ocean-tui now has the fuzzy `/` command palette, keyboard/mouse navigation,
and an extensible registry. Continue using that palette as the discoverability surface for later
harness mechanisms rather than adding hidden commands.

## Original build waves and current disposition

0. **W0 — profile seam: partial.** Resolve the remaining policy mismatch before adding more flags.
1. **W1 — edit reliability: built.** Preserve the tests and profile-gated runtime wiring.
2. **W2 — TUI streaming lock-in: strong partial.** Finish only the missing mechanisms that fit
   the current terminal workbench; do not replace it to imitate OMP.
3. **W3 — context economy: partial.** Minimizer and shared search are the next independent wins;
   richer compaction/checkpoint behavior needs explicit session and cache contracts.
4. **W4 — roles v2 + models catalog: partial.** Current flat roles and provider catalog are stable
   compatibility surfaces; extend them additively rather than replacing them wholesale.
5. **W5 — code intelligence: substantial partial.** Build on `ocean-lsp`; do not create a second
   LSP authority.
6. **W6 — rules + TTSR: absent.** Requires a separate design for streaming rewind, persistence,
   prompt-cache behavior, and rule trust.
7. **W7 — orchestration: original placement superseded.** Extensions own definitions, dispatch,
   lifecycle, budgets, joins, and policy. Core may add only generic permission-gated seams that an
   approved extension design requires.

## What Ocean already has (do not rebuild)

Hashline edits and artifact spill · an Ocean-native TUI workbench and slash registry · flat model
roles plus runtime advisor control · a live LSP provider · provider readiness/OAuth/fallback ·
typed memory with `retain`/`recall` · Ocean's own browser direction · OKF/ocean-context as the
memory/handoff substrate · folder-as-agent content and capability binding · daemon-owned sessions,
permissions, cancellation, and event transport. Extend these owners; do not create parallel OMP
subsystems beside them.
