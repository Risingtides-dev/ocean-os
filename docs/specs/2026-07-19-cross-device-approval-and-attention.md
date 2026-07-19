# Cross-Device Approval & Attention — Design Proposal

**Date:** 2026-07-19
**Status:** Phase 1 accepted by operator (Smaths) on 2026-07-19 (recommendations on Q1–Q4 adopted as rulings) and implemented the same day in `ocean-surface-ui` — see the Revisions section. Phases 2–3 remain proposed, not accepted.
**Type:** Design proposal with phased implementation plan
**Scope:** Cross-repo — `ocean-os` (daemon, core; authority), `ocean-surface` (Leptos PWA, Tauri shell, proxy; presentation). No TUI changes required for Phases 1–2.

---

## Overview

Goal: **run your agent from anywhere.** When an agent turn blocks on a
permission decision, the operator finds out on whatever device is in their
hand, sees what is blocked, and lands an allow/deny safely — without watching
a terminal.

The 2026-07-19 `/web` `/desk` `/beam` work made sessions addressable
(`?session=<id>`, `ocean://session/<id>`) so any device can open the exact
chat. This proposal extends the same fabric to *attention* and *control*: the
blocked state must reach the operator (notification, attention surface), not
just the operator reaching the session.

**Invariant:** the daemon remains the sole permission authority
(`ocean-runtime` gates, `ocean-daemon` routes). Surfaces render pending state
and POST decisions; no surface ever mints, bypasses, or persists a decision.
Decision tokens (OCEAN-185 / OCEAN-314) remain per-turn, unguessable, and out
of URLs, logs, and notification bodies.

---

## Current state (source-anchored audit)

What already works today — the proposal builds on these, it does not rebuild
them:

| Capability | State | Anchor |
|---|---|---|
| Global pending list | `GET /v1/permissions` returns the daemon-wide pending snapshot (not session-scoped) | `crates/ocean-daemon/src/main.rs` (`permissions()`) |
| Decision route | `POST /v1/permissions/{id}/decision`, body `PermissionDecisionRequest { decision, decision_token }`; Allow / AllowSession / Deny{reason} all mapped | `crates/ocean-daemon/src/main.rs` (`permission_decision`), `crates/ocean-core/src/lib.rs` |
| Decision tokens | 32 random bytes minted per turn, replayed on the decision POST | `ocean-surface-ui/src/daemon.rs` (`mint_decision_token`), TUI `Action::PermissionDecided` |
| PWA approval UI | pending list renders, decisions POST from the page | `ocean-surface-ui/src/daemon.rs` (`pending_permissions`, decision POST ~2450) |
| Late-join hydration | `GET /v1/sessions/{id}` detail carries `pending_permissions`; a device opening mid-prompt sees it | `ocean-surface-ui/src/daemon.rs` (`hydrate_active_session`) |
| Cross-device clear | `PermissionDecision` control event removes the prompt on every connected surface, session-scoped | `ocean-surface-ui/src/daemon.rs` (`ControlEvent::PermissionDecision`) |
| Notifications plumbing | `host::notify(title, body)` — Web Notifications on PWA, native via polyfill on Tauri — fires on **turn complete only** | `ocean-surface-ui/src/host.rs`, call site `app.rs` (~1633) |
| Badge | Tauri dock badge mirrors pending-permission count | `ocean-surface-ui/src/app.rs` (badge Effect) |
| Service worker | PWA shell caching only; **no push handling** | `ocean-surface/public/sw.js` |
| TUI approval | ⌃Y / ⌃N on the request prompt | `crates/ocean-tui/src/shell/app.rs` (`Action::PermissionDecided`) |

## Gap analysis

- **G1 — Blocked agents are silent.** Turn-complete notifies; a *blocked*
  agent — the thing that actually needs the operator — does not. Today you
  only see a permission prompt if you are already looking at the surface.
- **G2 — No daemon-wide attention view on surfaces.** The pending list exists
  (`GET /v1/permissions`) but no surface shows "session X is blocked" without
  opening that session first. On a phone this is the difference between one
  tap and a hunt.
- **G3 — No background reach.** Web Notifications require the PWA to be open.
  Pocket-buzz with the page closed needs Web Push (VAPID) through `sw.js`,
  which does not exist yet.
- **G4 — Public-endpoint posture.** On a public tunnel (e.g.
  ocean.agentsworld.org) HTTP basic auth is the only gate. Approving tool
  execution is a control operation; the posture needs a stated floor before
  background push widens the audience.

## Phase 1 — Notify on block (surface-only, small)

When a `PermissionRequest` arrives on the global event stream, fire
`host::notify` in addition to rendering the prompt.

- **ocean-surface-ui:** hook the existing `PermissionRequest` handling
  (mirror of the turn-complete call site). Title: session title. Body:
  **redacted** — tool name and action class only (e.g. "shell command needs
  approval"), never paths, args, URLs, or output; notifications render on
  lock screens.
- **Notification click focuses the session.** Web Notifications
  `onclick` focuses an existing client or opens `/?session=<id>` (the
  2026-07-19 boot handoff is the landing path); Tauri native notification
  activates the window and reuses `ocean://session/<id>`.
- **Settings toggle**, default on, persisted per device (localStorage), so a
  device used as a passive viewer can opt out.
- **Deduplication:** one notification per `permission_id` per device, even if
  the envelope re-arrives after SSE reconnect; cleared when the decision
  lands anywhere (the existing `PermissionDecision` clear already drives
  this).
- **Validation:** `cargo test -p ocean-surface-ui`; wasm32 check; manual PWA
  + Tauri smoke (block a turn, observe notification, click, land in session).

## Phase 2 — "Needs you" attention surface (surface + read-only daemon use, small–medium)

Make blocked state visible daemon-wide without opening sessions.

- **ocean-surface-ui:** session list rows gain a blocked badge driven by
  `GET /v1/permissions` (fetched on session-list open and on
  `PermissionRequest` / `PermissionDecision` stream events — no polling
  loop). A "Needs you" section pins blocked sessions to the top of the
  panel. Opening a row rides the existing `switch_session` + hydration,
  which already carries `pending_permissions`.
- **Tauri:** dock badge already mirrors the count; no change.
- **TUI (optional, same phase only if trivial):** status-line blocked count
  from the same global endpoint on its existing health cadence. Defer if it
  touches the session rail.
- **Validation:** surface UI tests for badge/section projection (pure fns,
  off-target); `cargo test -p ocean-surface-ui`; wasm32 check.

## Phase 3 — Background reach + posture floor (cross-repo, large; requires its own acceptance)

Pocket-buzz with the PWA closed. Separately gated because it widens the
public attack surface.

- **Web Push:** `sw.js` gains a `push` handler (show notification) and
  `notificationclick` handler (focus or open `/?session=<id>`). The proxy
  gains `POST /api/push/subscription` / `DELETE /api/push/subscription`
  (store endpoint + keys, VAPID-signed fan-out on permission-request).
  Payload is the redacted shape from Phase 1 — never paths or args. iOS
  requires the PWA installed to Home Screen (16.4+); document this.
- **Posture floor (land before push fan-out ships):**
  - Rate-limit `POST /v1/permissions/{id}/decision` (per-IP and per-id) at
    the daemon or proxy.
  - Document the supported exposure tiers: tailnet-only (recommended),
    basic-auth tunnel (current), and the observatory HMAC scoped principal
    as the future typed path (`docs/specs/2026-07-17-ocean-observatory-architecture.md`,
    section 9). Push subscriptions inherit the tier in force.
  - Decision tokens stay per-turn and are never logged; the daemon already
    treats them as opaque — assert it in a test.
- **Validation:** proxy subscription round-trip tests; daemon rate-limit
  tests; `cargo test -p ocean-surface-ui`; wasm32 check; end-to-end on a
  staged tunnel (block turn with PWA closed → notification → click → session).

## Security & privacy invariants (all phases)

1. Daemon is the only decision authority; surfaces only render + POST.
2. Decision tokens are per-turn, unguessable, never in URLs, logs,
   notification bodies, or push payloads.
3. Notification/push bodies are redacted to tool name + action class.
4. First decision wins; decision POSTs are idempotent-safe to replay; a
   resolved prompt clears everywhere via the existing control event.
5. No new GET side effects; decisions remain POST-only.
6. Session-addressed URLs (`?session=<id>`) carry no credentials; the
   proxy's auth tier is the gate.

## Open questions for operator ruling

1. **Notification content floor:** tool name only, or tool name + redacted
   target (e.g. file basename)? Recommendation: name only on lock-screen
   surfaces.
2. **Push philosophy:** is a public push endpoint acceptable at all, or
   should Phase 3 be tailnet-only by policy? Recommendation: ship Phases 1–2
   to everyone; gate Phase 3 behind the posture floor above.
3. **AllowSession on phones:** expose "allow for session" on small screens,
   or keep phones to single allow/deny? Recommendation: allow/deny only on
   compact; AllowSession stays a desktop affordance.
4. **TUI in Phase 2:** worth a status-line blocked count, or is the TUI
   always attended by definition? Recommendation: defer.

## Revisions

- **2026-07-19 — Phase 1 accepted + implemented.** Operator ruling: proceed
  with the recommended answers to Q1–Q4 (notification body is tool name
  only; Phase 3 push gated behind the posture floor; compact UI keeps
  allow/deny only; TUI attention deferred). Implemented in `ocean-surface-ui`:
  `permission_notify_decision` (pure, unit-tested) decides silence vs
  content — silent when the operator watches the blocked session (focused
  page + active session), notifying otherwise, including BACKGROUND
  sessions; body redacted to the tool name; OS dedupe via the permission id
  as the notification tag; click focuses the surface and opens the blocked
  session through the existing `switch_session` path (`host::notify_with_click`,
  best-effort click on the Tauri polyfill); per-device opt-out rides a
  localStorage key toggled by the "Permission Notifications" palette command
  (default on). Verified: 424 UI tests pass, clippy clean, wasm32 check
  clean. Operator-run smoke (block a turn, watch the notification, click,
  land in the session) remains pending.

## Non-goals

- No change to permission policy, modes, or the gate itself.
- No multi-operator or shared-device authorization model.
- No session transfer semantics — every surface is a peer view of the same
  daemon session, never a move.
- No daemon route additions in Phases 1–2 (Phase 3 adds proxy push routes
  only).
