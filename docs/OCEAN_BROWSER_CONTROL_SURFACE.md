# Ocean Browser — Agent Control Surface Spec

> The complete taxonomy of browser-control mechanics available to an Ocean agent,
> mapped to **current status** (what works today over CDP) and **target** (what an
> embedded Ocean browser unlocks), organized into build phases.
>
> Companion to [`OCEAN_BROWSER_CONTROL_PLANE.md`](OCEAN_BROWSER_CONTROL_PLANE.md)
> (the phasing/permission model) and the surface taxonomy in `ocean-agent`
> (`surface_flag`/`surface_dir`, the `[BRWSR]` surface). This doc is the **what**:
> every lever an agent can pull in a browser, and how much of each we have.

## TL;DR — the control delta

Today the agent controls **the page** (CDP on Chrome-for-Testing: ~near-total
perception + actuation inside whatever tab it's on). It does **not** control the
**browser shell** around the page (tabs, omnibox, profiles, downloads, settings)
— that stays Google's territory because we're a guest in Chrome-for-Testing.

| Layer | What it is | Today | Embedded target |
|---|---|---|---|
| 1 — Perception | see the page (DOM, a11y, net, screenshot) | ~90% | ~100% |
| 2 — Actuation | act on the page (click/type/scroll/JS) | ~95% | ~100% |
| 3 — Browser shell | tabs, omnibox, nav, downloads, settings | ~20% | ~100% |
| 4 — Identity & persistence | signed-in profile, cookies, sessions | ~10% | ~100% |
| 5 — Extensibility | extensions, content scripts, CDP depth, emulation | partial | deep |
| 6 — Agent-native primitives | tab-as-task, omnibox-as-intent, native cockpit | 0% | the differentiator |

**Engine internals (how Blink renders, JS engine guts, sandbox)** are out of scope
for *every* level including embedded — that's the only thing a Chromium **fork**
buys, and it's a perpetual multi-engineer C++ rebase tar-pit no agent task needs.
We embed Chromium (CEF / WebView2 / equivalent), we do **not** fork the engine.

---

## Layer 1 — Perception (what the agent can *see*)

| Mechanic | Today (CDP) | Notes / target |
|---|---|---|
| DOM read (tree, attrs, computed styles) | ✅ `browser_read_page` | full |
| **Accessibility tree** | ⚠️ underused | a11y roles/labels are often *better* than DOM for agents — surface it as a first-class read |
| Visible text extraction | ✅ | readable content stripped of chrome |
| Screenshots (viewport / full / element) | ✅ `browser_screenshot` | full |
| Visual coordinates (bounding boxes) | ✅ | enables click-by-pixel |
| Console stream | ✅ `browser_console` | logs/errors/warnings |
| Network observation (req/res/timing/payloads/ws) | ✅ `browser_network` | high-value for scraping; deepen payload + websocket capture |
| Page state (scroll/focus/form values/selection) | ✅ via JS | |
| Cookies / storage (local, session, IndexedDB) | ⚠️ via JS only | promote to a typed read |
| Media state (playing/time/audio) | ⚠️ via JS | useful for TikTok/video workflows |

**Status: ~90%.** Gaps: accessibility tree and structured storage/cookie reads.

## Layer 2 — Actuation (what the agent can *do* on a page)

| Mechanic | Today | Notes |
|---|---|---|
| Click (selector / a11y ref / pixel) | ✅ `browser_click` | |
| Type / keypress (keys, modifiers) | ✅ `browser_type`, `browser_key` | real keystrokes |
| Scroll (element / delta / into-view) | ✅ `browser_scroll` | |
| Form fill (inputs/selects/checkboxes/**file upload**) | ⚠️ partial | file-upload path needs hardening |
| Drag & drop | ❌ | add |
| Hover (triggers menus/tooltips clicks miss) | ❌ | add |
| JS injection (arbitrary script + return) | ✅ `browser_eval_js` | permission-gated |
| **Network interception** (block/modify/mock) | ❌ | big unlock — mock APIs, strip ads, inject during scrape |
| Dialog handling (alert/confirm/prompt/file/basic-auth) | ⚠️ | must be robust — dialogs block the CDP channel |
| Clipboard (read/write) | ❌ | add |
| Wait primitives (selector/nav/net-idle/condition) | ✅ `component_wait` family | |

**Status: ~95%.** Gaps: drag/drop, hover, clipboard, and **network interception**
(the highest-value missing actuation for scraping/automation).

## Layer 3 — Browser shell (the biggest jump from embedding)

| Mechanic | Today | Embedded target |
|---|---|---|
| Tab lifecycle (open/close/switch/reorder/dupe/pin) | ⚠️ clumsy, single-target | ✅ own the tab model — multi-tab workspaces |
| Window management (new/split/arrange/fullscreen/size) | ❌ | ✅ |
| Navigation (back/forward/reload/stop) | ⚠️ partial | ✅ |
| **Omnibox / address bar** | ❌ (Google's chrome) | ✅ build it — URL *or* natural-language intent |
| History (read/search/navigate/clear) | ❌ | ✅ agent-readable |
| Bookmarks (read/create/organize) | ❌ | ✅ |
| **Downloads** (trigger/monitor/locate/route file) | ❌ no clean hook | ✅ native — file flows straight into agent context |
| New-tab page | ❌ | ✅ agent-controlled |
| Find-in-page | ⚠️ via JS | ✅ native |
| Zoom / reader / print-to-PDF | ❌ | ✅ |

**Status: ~20%.** This is where "automate someone else's browser" becomes "own the
browser." Highest-value targets: **multi-tab** and **downloads**.

## Layer 4 — Identity & persistence (makes multi-step workflows real)

| Mechanic | Today | Embedded target |
|---|---|---|
| Profiles (multiple, switchable — per client/campaign) | ❌ throwaway test profile | ✅ first-class |
| **Persistent login sessions** (agent acts signed-in as you) | ⚠️ fragile | ✅ durable identity |
| Cookie/auth jar surviving restarts | ⚠️ | ✅ |
| Saved credentials / autofill | ❌ (and **security-gated** — see safety rules) | ✅ but gated: never auto-enter financial/password creds |
| Per-site permissions (cam/mic/loc/notif/popups) | ❌ | ✅ |

**Status: ~10%.** **This layer is what turns "do one thing on one page" into "log in
and run a real workflow."** For the TikTok/campaign/creator use-cases, persistent
signed-in identity + per-campaign profiles is the single highest-leverage upgrade.

## Layer 5 — Extensibility & deep hooks

| Mechanic | Today | Notes |
|---|---|---|
| Extension management (install/configure/drive) | ⚠️ the cockpit ext auto-loads | broaden |
| Content scripts (persistent JS on every page) | ❌ | enables ambient page augmentation |
| Service-worker / background hooks | ❌ | |
| Custom protocol handlers (`ocean://`) | ❌ | agent-native deep links |
| DevTools protocol at full depth (perf/coverage/memory/emulation) | ⚠️ subset | expose more |
| Device emulation (mobile viewport/geo/timezone/UA) | ❌ | critical for mobile-first TikTok scraping |

**Status: partial.** Device emulation is the standout for social-platform work.

## Layer 6 — Agent-native primitives (0% today; impossible without our own browser)

This is not "more control %" — it's a different category. Only buildable when the
browser is **ours**.

- **A tab that IS an agent task** — a workspace the agent owns, not a page it visits.
- **Omnibox-as-intent** — natural language in the address bar dispatches to the agent.
- **Cockpit as real chrome** — docked natively, not a CSP-fighting extension guest.
- **Always-on browsing context** — "what am I looking at across all tabs" as
  structured state the agent always has, no probe round-trip.
- **Workflow record/replay** — capture a human browse, replay as an agent macro.
- **Provenance/audit** — every agent browser action logged + auditable (the security
  model wants this anyway).

---

## Build phases (proposed sequencing)

**Phase A — close the cheap Layer 1/2 gaps (no new browser needed).**
Still on CDP/Chrome-for-Testing. Add: accessibility-tree read, structured cookie/
storage read, network **interception**, drag/hover/clipboard, robust dialog + file-
upload handling, device emulation. Pure additive tools on the existing
`BrowserProvider`. Highest ROI per effort; unblocks better scraping today.

**Phase B — the embedded shell decision + spike.**
Pick the base: **CEF** (Chromium Embedded Framework — most control, C++/Rust bindings),
WebView2 (Windows-lean), or a Tauri/WRY shell (lightest, less deep). Spike: an Ocean
window that hosts a real Chromium view + the cockpit as native chrome + ocean-os
driving it. Decision criteria: how deep into Layer 3/4 each base actually reaches.

**Phase C — Layer 3 (browser shell control).**
Multi-tab lifecycle + window management + native navigation/omnibox + **downloads**.
This is the "own the browser" jump. Map each mechanic to an agent tool + a
`ClientContext.browser` field so the agent always knows the live shell state.

**Phase D — Layer 4 (identity & persistence).**
Per-campaign/per-client profiles, durable signed-in sessions, per-site permissions.
Security-gated per the action rules (never auto-enter financial/password creds; each
new capability is an explicit user-visible escalation).

**Phase E — Layer 6 (agent-native primitives).**
Tab-as-task, omnibox-as-intent, native cockpit, always-on context, record/replay,
audit trail. The differentiator — a browser whose primitives are built for an agent.

---

## What we are NOT doing

- **Not forking Chromium's engine.** Embedding (CEF/WebView2/WRY) gives ~98% of
  total browser control without owning Blink/V8 source. The ~2% a fork adds (engine
  internals) costs a perpetual C++ rebase team and unblocks no real agent task.
- **Not bypassing the runtime.** Every browser capability registers as a
  `CapabilityProvider` tool (like today's `BrowserProvider`), permission-gated; the
  agent loop stays the single authority.

## Where this plugs into the code

- `crates/ocean-browser/` — the chromiumoxide/CDP driver (Layers 1–2 today).
- `crates/ocean-runtime/src/tools/browser/` — agent-facing tools
  (`nav`/`input`/`inspect`/`perceive`) registered via `BrowserProvider`.
- `crates/ocean-agent` surface taxonomy — `[BRWSR]` surface, `surface_flag`/
  `surface_dir`; the browser host is represented by the browser capability
  profile rather than a desktop-client alias.
- `ClientContext.browser` (planned, per control-plane doc) — the live shell state
  (active tab URL/title/selection, tab list) the agent reads each turn.

## Current status snapshot (2026-06)

- Layers 1–2: live and strong on CDP. Layer 3–4: minimal (guest in Chrome-for-Testing).
- Active in-tree work (uncommitted): browser tool changes (`ocean-browser`,
  `tools/browser/*`), MCP (`ocean-mcp` — the cloud multi-agent/memory hub direction),
  ACP (`ocean-acp`), and a new `POST /v1/agent/sessions` create endpoint. These are
  Phase-A-adjacent + infrastructure; commit in logical groups (browser / mcp / acp /
  session-create), keeping main's shipped #32 surface-awareness intact.
- Direction chosen: **embedded Ocean browser, not an engine fork.**
