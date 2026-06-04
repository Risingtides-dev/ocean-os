# Ocean Browser Control Plane — spec & roadmap

> **Status:** planning document. Nothing here is built by this doc. It maps the work to turn
> the Ocean Chrome extension into a first-class browser control plane and sequences the
> remaining steps on top of what already exists.
>
> **Repo placement:** this lives in `ocean-os` because the runtime, tools, and protocol types
> are the brain's concern. The extension UI ships in the sibling repo `ocean-surface`, but the
> control surface (CDP tools, session/turn types, system-prompt scoping) is owned here.

---

## 1. Goal & framing

Make the agent a first-class **operator of the browser**, steering it through every available
path:

- **UI** — clicks and typing (`browser_click`, `browser_type`, `browser_key`).
- **API** — CDP, JS eval, network (`browser_eval_js`, `browser_network`).
- **CLI-style** — navigation and movement (`browser_navigate`, `browser_scroll`).
- **Dev tools** — console, network, DOM inspection (`browser_console`, `browser_read_page`).

Today this runs on a **single-browser, daemon-driven model**: the daemon owns one
Chrome-for-Testing instance, launched lazily on first browser tool call, and tools act on its
one "active page." The extension itself is a thin UI shell that talks to the daemon over
`127.0.0.1:4780`; it does **not** drive Chrome directly. This doc keeps that model as the
baseline and sequences how it grows.

> **Terminology — "ACP-level" ≠ `ocean-acp`.** John's phrase "full-blown ACP-level control
> plane" means *breadth of control* (the agent reaching every browser surface). It is **not**
> the `ocean-acp` crate, which is the *Agent Client Protocol* bridge exposing the daemon to Zed
> and other ACP editors over stdio (`crates/ocean-acp/`). Don't conflate the two: this roadmap
> is about browser tooling + context, not the ACP editor bridge.

---

## 2. The floor — what exists today

Each claim below was verified against this repo. Cite these so the next person doesn't re-plan
solved work.

| Capability | Status | Where |
|---|---|---|
| 10 CDP browser tools | ✓ | `crates/ocean-runtime/src/tools/browser/`: `nav.rs` (`browser_navigate`), `input.rs` (`browser_click`/`type`/`key`/`scroll`), `inspect.rs` (`browser_console`/`network`), `perceive.rs` (`browser_read_page`/`screenshot`) |
| Registered as a capability provider | ✓ | `BrowserProvider` — `tools/browser/mod.rs:91,120` |
| Read/mutate permission split | ✓ | Reads ungated; mutators implement `requires_permission()` — `input.rs:29,67,97,130`, `nav.rs:26`, `inspect.rs:22`; contract stated at `mod.rs:2` |
| Lazy single Chrome-for-Testing | ✓ | `crates/ocean-browser/src/launch.rs`; one handle = "one Chrome + its active page" — `lib.rs:19–20` |
| Active page prefers the real tab | ✓ | `active_page()` skips `chrome-extension://` and devtools — `crates/ocean-browser/src/lib.rs:37–82` |
| `CapabilityProvider` registry (ordered, first-claim-wins) | ✓ | `crates/ocean-runtime/src/capability.rs:65`; MCP rides the same path (`crates/ocean-mcp`) |
| `client_type` plumbing end-to-end | ✓ | `AgentTurnRequest.client_type` (`crates/ocean-agent-sdk/src/lib.rs:200`) → daemon (`crates/ocean-daemon/src/main.rs:1757–1853`) → `build_system_prompt` (`crates/ocean-agent/src/lib.rs:452`) → `append_client_type` (`lib.rs:1981`) |
| **Dedicated extension prompt** | **✗ not built** | `append_client_type` (`crates/ocean-agent/src/lib.rs:1981–1991`) has arms for `tui`, `surface-web`, `surface-gpui`, `surface-native`, `cli`, `leo-voice`, and a generic `Some(other)` fallthrough. **There is no `surface-extension` arm anywhere in the repo.** A `surface-extension` client_type currently resolves to the generic *"You are speaking through an unknown client"* message. |

**ocean-surface side (not verified from this doc's session — confirm against the `ocean-surface`
repo before relying on these):** the surface reportedly computes `surface_client_type()` and
emits `"surface-extension"` when running under `chrome-extension://`
(`crates/ocean-surface-ui/src/daemon.rs`), and the MV3 `manifest.json` declares only
`["sidePanel","storage"]` plus a host permission to `127.0.0.1:4780` — no `tabs`, `scripting`,
or `debugger`. If those hold, then the **plumbing carries an identity the daemon doesn't yet
specialize for** (see Phase 1).

---

## 3. Phased roadmap

```
P1 identity ──> P2 active-tab ──> P3 ClientContext ──> P4 multi-tab ──> P5 DOM bridge ──> P6 registry
(plumbed,        (highest          (typed schema      (scale past      (heaviest        (more tools,
 1 arm left)      leverage,         the plane          one page)        escalation,      not a new
                  next)             hangs off)                          gated)           transport)
```

### Phase 1 — Surface identity (plumbing done; one small gap)

The `client_type` string already flows extension → daemon → system prompt. The only missing
piece is a **dedicated `surface-extension` arm** in `append_client_type`
(`crates/ocean-agent/src/lib.rs:1981`), mirroring the existing `surface-web` / `surface-gpui`
arms, with a prompt that tells the agent:

- it is docked inside the user's Chrome (extension side panel), and
- its `browser_*` tools drive a live browser tab (not a headless scratch instance the user
  can't see).

This is the cheapest first move and the prerequisite for everything below making sense to the
model. *(Doc only — not written here.)*

### Phase 2 — Active-tab context (highest leverage, next)

The agent currently knows *which surface* it's speaking through but not *which page* the user is
looking at. Capturing the live tab's **URL / title / selected text / scroll position** and
passing it with each turn lets "summarize this" / "click that" resolve to the live tab without a
probe round-trip. Two implementation options — **decision for John (see §6)**:

- **(a) Daemon-side, zero extension-permission change.** The daemon's `active_page()` selector
  (`crates/ocean-browser/src/lib.rs:37–82`) already finds the real tab. Surface a lightweight
  "current page" snapshot into the turn context the agent sees up front. Works with the existing
  Chrome-for-Testing model; no manifest change, no user permission prompt. **Recommended first.**
- **(b) Extension-side.** Add the `tabs` (and possibly `activeTab`) permission so the background
  worker can `chrome.tabs.query({active, currentWindow})` and POST that with the turn. Required
  only once the target is the user's *own* Chrome rather than the daemon's instance.

### Phase 3 — Typed `ClientContext` (the schema the plane hangs off)

Replace the ad-hoc `client_type: Option<String>` with — or alongside — a typed struct on
`AgentTurnRequest` (`crates/ocean-agent-sdk/src/lib.rs:178`):

```
struct ClientContext {
    client_type: Option<String>,        // keep for back-compat
    browser: Option<BrowserContext>,
}
struct BrowserContext {
    url: Option<String>,
    title: Option<String>,
    selection: Option<String>,
    tab_id: Option<...>,
    window_id: Option<...>,
}
```

**Dual-compat path:** keep the existing flat `client_type` string so older clients keep working;
deserialize both shapes (serde `#[serde(default)]` / `Option`, accept either the legacy field or
the nested struct). Daemon unpacks it where it builds `PromptRequest` today
(`crates/ocean-daemon/src/main.rs:1841–1853`); the agent reads it where it calls
`build_system_prompt` (`crates/ocean-agent/src/lib.rs:452`).

> Note: `guidance: Option<Vec<String>>` on `AgentTurnRequest` (`sdk/src/lib.rs:188`) is
> currently **dead** — the daemon discards it (`main.rs:1763`, `guidance: _`). It can be
> repurposed as a context channel or left alone; call it out so it isn't mistaken for a working
> input.

### Phase 4 — Per-tab / multi-tab steering

Today tools act on one cached page; the handle is explicitly "one Chrome + its active page"
(`crates/ocean-browser/src/lib.rs:19`), mutex-guarded. To target a specific tab, choose one:

- thread a `tab_id` argument through the browser tools, or
- add explicit tab-switch / tab-list tools.

Either way, `ocean-browser` must change from holding a single page to holding **N pages** keyed
by id, and `active_page()` becomes "resolve the requested tab, else the preferred real tab."

### Phase 5 — DOM bridge / content-script reach (optional, gated)

If/when the extension should read or mutate page DOM **directly** (not via the daemon's CDP
session) — e.g. to reach into pages the daemon's Chrome can't see — it needs the `scripting`
permission, content scripts, and a message channel back to the daemon. This is the **heaviest
permission escalation** in the roadmap and should stay behind an explicit, user-visible opt-in.

### Phase 6 — Tool-registry expansion (the "ACP/MCP shape")

New browser-steering tools register as a `CapabilityProvider`
(`crates/ocean-runtime/src/capability.rs:65`) exactly like `BrowserProvider` does today. MCP
already uses this same registry, so reaching "ACP-level" breadth is mostly **more tools + richer
context**, not a new transport or protocol.

---

## 4. Type changes (ocean-core / SDK)

| Concern | Detail |
|---|---|
| New type | `ClientContext` (+ `BrowserContext`) as in §3 |
| Attach point | `AgentTurnRequest` — `crates/ocean-agent-sdk/src/lib.rs:178` |
| Back-compat | Keep flat `client_type: Option<String>`; serde-accept both legacy and nested shapes |
| Daemon unpack | `crates/ocean-daemon/src/main.rs:1841–1853` (where `PromptRequest` is built) |
| Agent read | `crates/ocean-agent/src/lib.rs:452` (into `build_system_prompt`) |
| Dead field | `guidance` discarded at `main.rs:1763` — repurpose or leave; do not assume it works |

---

## 5. Permission & security model

**Baseline contract (keep it).** Read tools (`browser_read_page`, `browser_screenshot`,
`browser_console`, `browser_network`, `browser_scroll`) stay **ungated**; mutating tools
(`browser_navigate`, `browser_click`, `browser_type`, `browser_key`, `browser_eval_js`) stay
**permission-gated** via `requires_permission()`.

**Manifest escalation map.** Each Chrome permission a phase forces is a user-visible escalation —
defer each until the phase that needs it:

| Permission | Forced by | Why |
|---|---|---|
| `tabs` / `activeTab` | Phase 2(b) | read active-tab metadata from the extension |
| `scripting` | Phase 5 | inject content scripts for direct DOM access |
| `debugger` | driving the user's *real* Chrome | CDP attach to the user's logged-in browser |

**Audit.** Browser tools already emit a `ToolSideEffect::BrowserActivity { active: true }`
side-effect (`tools/browser/mod.rs:66`, `perceive.rs:66`) — but that is a *handoff/active flag*,
not a record of what the agent did. As the plane gains power, extend this into a real per-action
audit trail (which action, which tab, when).

**The daemon-Chrome vs. user-Chrome boundary is itself a security feature.** Driving an isolated
Chrome-for-Testing profile means the agent never touches the user's logged-in real Chrome — small
blast radius, no session/cookie exposure. Graduating to the user's real Chrome (via `debugger`
attach) trades that safety for convenience. Treat the boundary as a deliberate stance, not an
accident.

---

## 6. Open decisions for John

1. **Active-tab context (Phase 2):** daemon-side snapshot **(a)** — no permission prompt, works
   with the current model — or extension `tabs` permission **(b)**? Recommendation: (a) now, (b)
   when steering the user's own Chrome becomes the goal.
2. **Isolation:** stay on the isolated Chrome-for-Testing instance, or graduate to driving the
   user's real Chrome (`debugger` attach)? This sets the ceiling for the whole plane.
3. **Multi-tab now or later (Phase 4):** build per-tab steering proactively, or defer until a
   concrete multi-tab workflow demands it?
