# Ocean OS Site Heartbeat State

This file is the compact handoff for the hourly heartbeat agent. Keep it short.
Update it after each heartbeat slice so future turns do not depend on long chat history.

## Mission
Continue maintaining `docs/ocean-os-site` as a polished, paginated architecture/product documentation site for Ocean OS. Work in small slices, grounded in repo files and real runnable surfaces. Do not fake screenshots.

## Done pages
- `docs/ocean-os-site/index.html` — architecture overview; latest update aligns Product API section with the active Project -> Workspace -> Session -> Turns -> Events contract and confirms no broken internal links.
- `docs/ocean-os-site/pages/surfaces.html` — surface inventory, live daemon/surface status, screenshot assets, CLI capture, Longhouse capture, honest remaining capture queue.
- `docs/ocean-os-site/pages/longhouse.html` — Longhouse architecture, event model, live deck capture from `:8795` after `/v1/longhouse/demo`.
- `docs/ocean-os-site/pages/browser.html` — Chrome extension/browser cockpit page. Includes browser mock, animated system diagram, Remotion storyboard, context pipe, MV3 details, ocean-browser API.
- `docs/ocean-os-site/pages/daemon.html` — daemon API, live checks, route map, turn lifecycle, permissions note, root-route mismatch; freshness pass updated line refs and CTA.
- `docs/ocean-os-site/pages/runtime.html` — runtime loop, capability registry, side effects, permission hooks, cancellation, context/token guardrails; freshness pass updated line refs and CTA.
- `docs/ocean-os-site/pages/protocol.html` — SDK vs legacy protocol layers, session creation before turns, `/v1/agent/turns`, scoped `/v1/agent/events`, event lifecycle, components/browser activity, compatibility map; latest contract refresh updated line refs.
- `docs/ocean-os-site/pages/surface.html` — Ocean Surface PWA/proxy architecture, live local check, SSE/session model, component rendering, voice/proxy routes, runnable captures; freshness pass updated line refs, native shell note, and CTA.
- `docs/ocean-os-site/pages/tui.html` — TUI cockpit architecture: daemon client routes, F1-F7 rooms, SDK turn submission, SSE parsing, PM block rendering, sessions/permissions/status; freshness pass updated line refs and CTA.
- `docs/ocean-os-site/pages/desktop.html` — native GPUI/egui desktop architecture, daemon client contract, surface-gpui turn submission, session-scoped SSE, canvas/web host; freshness pass updated sibling repo line refs.
- `docs/ocean-os-site/pages/cli-sdk.html` — CLI command surface, one-shot prompt flow, existing CLI capture, SDK identifiers/sessions/requests/events, compatibility map; freshness pass updated line refs and CTA.
- `docs/ocean-os-site/pages/providers.html` — provider registry, model catalogue, credential resolution, readiness, daemon model API, protocol transport mapping, live `/ready`/`/v1/models` check; freshness pass updated line refs and CTA.
- `docs/ocean-os-site/pages/sessions.html` — local-first session storage, workspace buckets, atomic JSON saves, strict resume semantics, history caps, session APIs, SDK bridge gap; freshness pass updated line refs.

## Assets added
- `docs/ocean-os-site/assets/surfaces/model-dropdown-halt.png`
- `docs/ocean-os-site/assets/surfaces/tool-group-collapsed.png`
- `docs/ocean-os-site/assets/surfaces/map-render-test.png`
- `docs/ocean-os-site/assets/surfaces/longhouse-deck-live.png`
- `docs/ocean-os-site/assets/surfaces/cli-capture.txt`

## Next page order
All originally queued core/client pages are filled. Next recommended small slice: refresh `daemon.html` or `sessions.html` for the session-creation/project-workspace contract; otherwise capture real TUI/desktop screenshots if runnable.

## Slice rules
- One small page/slice per heartbeat.
- Inspect current files before editing.
- Ground claims in source paths.
- Preserve the visual language: dark glass, cyan/blue/violet gradients, cards, matrix tables, code blocks, diagrams where helpful.
- Do not kill user processes. Do not destructive-clean. Do not rewrite completed pages except nav/link/source consistency.
- Leave a final summary with changed paths and a `file://` URL.
- Use the browser to open the edited page when possible.

## Current heartbeat mechanics
- Old LaunchAgent: `~/Library/LaunchAgents/dev.risingtides.ocean-site-heartbeat.plist`
- Old script: `scripts/ocean-site-heartbeat.sh`
- Old state dir: `~/.local/state/ocean-site-heartbeat`
- New Rust crate exists: `crates/ocean-heartbeat`
- New docs-site routine config: `docs/ocean-heartbeat/ocean-site-docs.toml`
- New routine session file path: `~/.local/state/ocean-heartbeat/ocean-site-docs.session`

## Last known issue/fix
Old shell LaunchAgent was brittle under launchd. A Rust runner crate `ocean-heartbeat` now exists with `run`, `init`, `launchd`, and `component` commands, but the old LaunchAgent has not been migrated/installed to the Rust runner yet.

## Latest slice
- Refreshed `docs/ocean-os-site/pages/protocol.html` for the active session-first product contract: `POST /v1/agent/sessions`, then scoped SSE, then session-bound turns.
- Files changed: `docs/ocean-os-site/pages/protocol.html`, `docs/ocean-os-site/HEARTBEAT_STATE.md`.
- Assets captured: none.
- QA result: parsed all local HTML links and image sources across `index.html` and `pages/*.html`; no broken internal links/images found.
- Gotcha: `AgentTurnRequest` still allows omitted `session_id` for compatibility, but first-party surfaces should create/choose a session first per `docs/OCEAN_ECOSYSTEM_CONTRACT.md`.

## Compaction protocol
At the start of every heartbeat:
1. Read this file.
2. Read the target page if it exists.
3. Read source files needed for the target page.

At the end of every heartbeat:
1. Update this file with completed work, files changed, assets captured/queued, next recommended target, and gotchas.
2. Keep this file concise; overwrite stale detail rather than growing forever.
