# Ocean-Chrome — Design Spec

**Date:** 2026-06-02
**Status:** Approved (brainstorm), pending implementation plan
**Repos touched:** `ocean-os` (daemon control tool), `ocean-surface` (extension packaging)

## One-line summary

Ocean drives its own Chrome instance over the DevTools Protocol (screenshots,
real mouse/keyboard, full page control), and the existing `ocean-surface` web UI
is repackaged as a Chrome extension side panel that rides along inside that
browser. The single daemon-side conversation auto-follows into the side panel
while browser work happens, then releases back to the origin surface (TUI / PWA)
when it's done.

This is "everything Claude-in-Chrome can do, but Ocean" — built on the clean
launched-Chrome path so there are no Google extension sandbox limits and no
permanent debugger warning bar fighting us for control.

## Goals

- Ocean can fully drive a real Chrome: navigate, click, type, scroll, take
  screenshots, run JS, read console + network, read the page structurally.
- Perception is **hybrid**: cheap structured DOM/accessibility read by default,
  automatic fallback to screenshot + coordinate (x/y) clicking for visual pages
  (canvas, video, maps).
- The **existing** `ocean-surface` Leptos/WASM UI is transported into a Chrome
  extension side panel — no new chat UI is written.
- **One conversation.** Sessions stay in the daemon (as they already do). The
  side panel is a thin client that attaches to the active `session_id`.
- **Auto handoff.** When a browser tool fires, the conversation auto-focuses
  into the side panel; when the browser work ends, focus releases back to the
  origin surface. Trigger model: **auto on browser activity, auto return.**

## Non-goals

- Driving the user's *everyday* signed-in Chrome. Ocean uses its **own** Chrome
  profile. The user logs into their accounts in it once; the profile persists
  them. (The MV3-extension-controls-your-daily-browser approach was explicitly
  rejected in favor of the cleaner launched-Chrome path.)
- Chrome Web Store distribution. The extension is loaded locally via
  `--load-extension` at launch.
- Rewriting `ocean-surface`'s chat UI. We repackage it, we don't rebuild it.

## Architecture

Three pieces.

### 1. The hands — CDP browser-control tool (ocean-os daemon)

A new Rust module in the daemon that owns a Chrome instance over the Chrome
DevTools Protocol.

- **Launch model:** the daemon launches Chrome with
  `--remote-debugging-port=<port>`, a dedicated persistent
  `--user-data-dir` (so logins survive restarts), and
  `--load-extension=<ocean-chrome-ext>` so the cockpit is present on open.
- **CDP client:** Rust CDP driver (candidate crate: `chromiumoxide`, or a thin
  hand-rolled CDP client — to be decided in the plan). The daemon holds the
  reins; the extension never drives the page.
- **Agent tools** (all permission-gated, same as every other Ocean tool):
  - `navigate { url }`
  - `read_page` — hybrid perception: structured DOM/accessibility tree first;
    returns element refs + text + roles. Falls back to / augments with a
    screenshot when the page is visual or the structured read is insufficient.
  - `click { ref? , x? , y? }` — click by element reference (precise, cheap) or
    by raw pixel coordinate (works on canvas/video/anything).
  - `type { text }`, `key { combo }` — real keystrokes.
  - `scroll { dx, dy | to_ref }`
  - `screenshot { full_page? }`
  - `eval_js { source }`
  - `read_console { pattern? }`
  - `read_network { filter? }`
- **Eventing:** every browser action emits an `AgentTurnEvent` on the existing
  `/v1/agent/events` SSE stream so any attached surface renders it live.

### 2. The cockpit — ocean-surface as a Chrome extension

`ocean-surface` is a Leptos/WASM app built with Trunk. It already:
- talks to the daemon at `http://127.0.0.1:4780`,
- streams `GET /v1/agent/events` (SSE via `gloo_net` EventSource),
- posts turns to `POST /v1/agent/turns`,
- carries a `session_id` and lists sessions.

So it is *already* a thin client. To transport it into an extension:

- Add a Trunk build profile / step that emits a Chrome **MV3** extension bundle:
  - `manifest.json` (manifest v3) declaring a `side_panel`, host permission for
    `http://127.0.0.1:4780/*`, and the WASM bundle as the panel document.
  - CSP adjustments required for WASM in an extension context
    (`wasm-unsafe-eval`).
  - The side panel document loads the same compiled WASM bundle the PWA uses.
- UI code changes are expected to be **minimal** — manifest, CSP, daemon URL
  resolution, and the handoff hook (below). The chat, transcript, sessions, TTS,
  and voice code are reused as-is.

### 3. The handoff glue

One `session_id`, one transcript, living in the daemon. The side panel is just
another thin client that attaches to it.

- Daemon emits a **`browser-active`** signal on the event stream when an agent
  turn begins calling browser tools, and **`browser-idle`** when the turn ends
  with no browser tools pending. (Mechanism: either a new lightweight event
  variant or a flag on existing turn events — decided in the plan.)
- The extension side panel, already streaming events, listens for these:
  - on `browser-active` → auto-attach to the active `session_id` (if not
    already) and take focus, so the live conversation is now in the panel and
    the user watches + chats there;
  - on `browser-idle` → release focus back to the origin surface.
- The handoff is a **session-attach**, not a data migration. The conversation
  is never duplicated or forked.

## Data flow (happy path)

```
User (TUI/PWA): "go book me a flight on example.com"
   │  POST /v1/agent/turns { prompt, session_id }
   ▼
ocean-daemon  ── agent loop runs, decides to use the browser
   │  emits browser-active on /v1/agent/events
   │     │
   │     ▼  side panel (in Ocean's Chrome) auto-focuses, attaches to session_id
   │  CDP: navigate → read_page (DOM/a11y) → click(ref) → type → screenshot
   │     every action streams as AgentTurnEvent → rendered in side panel live
   │  task complete, turn ends
   │  emits browser-idle
   ▼     │
focus releases back to TUI/PWA, same conversation continues there
```

## Error handling & safety

- Browser tools are **permission-gated** like all Ocean tools. Risky actions
  (form submits, purchases, posting content, logins) prompt before firing,
  matching Ocean's existing permission model.
- CDP disconnects, dead/closed tabs, and navigation timeouts surface as
  **tool errors the agent can retry** — they do not crash the daemon.
- If Chrome isn't running / the profile is locked, the launch step reports a
  clear error and the browser tools are unavailable rather than hanging.

## Testing

- **Control layer:** drive a local fixture HTML page headless; assert clicks,
  typing, scrolling, and structured reads land on known DOM nodes. Assert
  screenshot + coordinate-click path works on a canvas fixture.
- **Handoff:** assert `browser-active` / `browser-idle` events fire on the
  stream at the correct turn boundaries.
- **Extension:** manual smoke test in Ocean's launched Chrome — side panel
  loads the WASM bundle, attaches to the session, auto-focuses on browser
  activity, releases after.

## Open decisions for the implementation plan

1. CDP driver: `chromiumoxide` vs hand-rolled thin CDP client.
2. `browser-active`/`browser-idle`: new event variant vs flag on existing events.
3. Exact `read_page` heuristic for when to fall back to screenshot.
4. Trunk → MV3 packaging: separate `Trunk.toml` profile vs post-build script.
5. Where the launched-Chrome lifecycle lives (daemon startup vs first browser
   tool call / lazy launch).
