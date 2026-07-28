# Ocean OS Site Heartbeat State

This file is the compact handoff for the hourly heartbeat agent. Keep it short.
Update it after each heartbeat slice so future turns do not depend on long chat history.

## Mission
Continue maintaining `docs/ocean-os-site` as a polished, paginated architecture/product documentation site for Ocean OS. Work in small slices, grounded in repo files and real runnable surfaces. Do not fake screenshots.

## Done pages
- `docs/ocean-os-site/index.html` — architecture overview; latest update aligns Product API section with the active Project -> Workspace -> Session -> Turns -> Events contract and confirms no broken internal links.
- `docs/ocean-os-site/pages/surfaces.html` — surface inventory, source-grounded architecture notes, and explicitly labeled controlled demonstrations; sensitive, blank, and host-revealing captures are rejected.
- `docs/ocean-os-site/pages/longhouse.html` — Longhouse architecture and event model; the blank controlled-demo frame is no longer presented as live evidence.
- `docs/ocean-os-site/pages/browser.html` — Chrome extension/browser cockpit page. Includes browser mock, animated system diagram, Remotion storyboard, context pipe, MV3 details, ocean-browser API.
- `docs/ocean-os-site/pages/daemon.html` — daemon API, live checks, route map, turn lifecycle, permissions note, root-route mismatch; freshness pass updated line refs and CTA.
- `docs/ocean-os-site/pages/runtime.html` — runtime loop, capability registry, side effects, permission hooks, cancellation, context/token guardrails; freshness pass updated line refs and CTA.
- `docs/ocean-os-site/pages/protocol.html` — SDK vs legacy protocol layers, session creation before turns, `/v1/agent/turns`, scoped `/v1/agent/events`, event lifecycle, components/browser activity, compatibility map; latest contract refresh updated line refs.
- `docs/ocean-os-site/pages/surface.html` — Ocean Surface PWA/proxy architecture, SSE/session model, component rendering, and explicitly labeled controlled demonstrations; the honesty pass distinguishes proxy-forwarded STT/TTS from the transcript-only `/v1/agent/voice` seam and correctly identifies Tauri as the current native shell with GPUI retained only as legacy source.
- `docs/ocean-os-site/pages/tui.html` — TUI cockpit architecture: daemon client routes, F1-F7 rooms, SDK turn submission, SSE parsing, PM block rendering, sessions/permissions/status; freshness pass updated line refs and CTA.
- `docs/ocean-os-site/pages/desktop.html` — current Tauri shell architecture, shared Leptos UI boundary, native bridge responsibilities, daemon client contract, and `surface-tauri` turn identity.
- `docs/ocean-os-site/pages/cli-sdk.html` — CLI command surface, one-shot prompt flow, SDK identifiers/sessions/requests/events, and compatibility map; the sensitive prior CLI capture was removed.
- `docs/ocean-os-site/pages/providers.html` — provider registry, model catalogue, credential resolution, readiness, daemon model API, protocol transport mapping, live `/ready`/`/v1/models` check; freshness pass updated line refs and CTA.
- `docs/ocean-os-site/pages/sessions.html` — local-first session storage, workspace buckets, atomic JSON saves, strict resume semantics, history caps, session APIs, and privacy-safe source-grounded evidence.

## Public evidence inventory
- Retained as controlled demonstrations: `model-dropdown-halt.png`, `map-render-test.png`.
- Removed from the current tree: `tool-group-collapsed.png` (local host metadata), `cli-capture.txt` (session IDs and local paths), `longhouse-deck-live.png` (blank demo frame).
- Governing contract: `docs/ocean-os-site/PUBLIC_EVIDENCE_POLICY.md`.

## Next page order
All originally queued core/client pages are filled. Next recommended small slice: inspect `pages/sessions.html` against the current strict Project -> Workspace -> Session -> Turns -> Events contract. The larger product-site gap remains truthful motion media: no Remotion project or rendered `.mp4`/`.mov` pipeline exists in this repo.

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
- Refreshed `docs/ocean-os-site/pages/surface.html` against current `ocean-os` and sibling `ocean-surface` sources.
- Corrected the client boundary: one Leptos/WASM bundle serves PWA, Tauri, and extension; GPUI is legacy. The proxy forwards STT/TTS to daemon-owned endpoints and holds no provider credential.
- Clarified that `POST /v1/agent/voice` accepts an already-transcribed `transcript`, tags `client_type: "leo-voice"`, and delegates to the normal turn path; it is not microphone capture, daemon STT, wake word, TTS, or a complete voice UX.
- Preserved the public-evidence policy: retained images are labeled controlled demonstrations, the runtime observation is dated 2026-07-20, and the rejected host-revealing tool capture remains deleted.
- Files changed: `docs/ocean-os-site/pages/surface.html`, `docs/ocean-os-site/HEARTBEAT_STATE.md`, `events.md`.
- Assets captured: none. Queued: only real runnable TUI/desktop captures or a future genuine motion pipeline; do not manufacture video evidence.
- QA: HTML parse, local link/image validation, `git diff --check`, and `cargo xtask docs-check` pass.
- Gotcha: sibling `../ocean-surface` has unrelated uncommitted work; it was read only and not modified.

## Compaction protocol
At the start of every heartbeat:
1. Read this file.
2. Read the target page if it exists.
3. Read source files needed for the target page.

At the end of every heartbeat:
1. Update this file with completed work, files changed, assets captured/queued, next recommended target, and gotchas.
2. Keep this file concise; overwrite stale detail rather than growing forever.
