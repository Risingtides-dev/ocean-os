# OceanWebKit Browser Program Manifest

Status: **operator-ratified program direction, locked in 2026-07-19.** The
Chromium backend is quarantined and scheduled for deletion; OceanWebKit is the
replacement browser engine program. This manifest fixes the target
architecture, acceptance gates, security invariants, and milestone order. An
unchecked milestone is a direction, not permission to alter public contracts;
each gate closes with verification, review, and an `events.md` entry.

## 1. Operator ruling

On 2026-07-19 the operator ruled:

1. **Drop the heavy Chrome dependencies.** `chromiumoxide` is quarantined
   behind the default-off `legacy-chromium` feature chain
   (`ocean-browser` → `ocean-runtime` → `ocean-agent` → `ocean-daemon`).
   Default workspace builds compile zero chromiumoxide packages.
2. **Build a full custom WebKit engine ("OceanWebKit") with parity to Chrome
   DevTools** as Ocean's permanent browser backend. Not system `WKWebView`,
   not an MITM proxy, not a bundled Chromium/Firefox sidecar.
3. **Keep the legacy backend running interim** on the supervised daemon via
   `ops/install-ocean-daemon.sh`, which builds with
   `--features legacy-chromium` until the OceanWebKit browser host ships.

## 2. Why

- The chromiumoxide stack cost every workspace build its generated CDP
  bindings, WebSocket stack, and duplicate reqwest graph (~1.7 GB of build
  artifacts across profiles), while giving only a second-class
  attach-to-Chrome automation story.
- Public `WKWebView` cannot deliver the required contract: complete network
  capture (parser resources, WebSocket frames, service-worker internals,
  response bodies), trusted input, named profiles, and an embedded DevTools
  endpoint are all outside its stable API. Apple ships the machinery
  (`UIProcess/Automation`, `NetworkProcess`, JavaScriptCore inspector,
  WebInspectorUI) without a supported third-party contract.
- Owning the engine turns WebKit's existing internal machinery into a
  supported Ocean contract — without carrying a second Chromium.

## 3. Target architecture

```text
Ocean Surface
  ├─ local application shell
  └─ OceanWebKit embedded view
        ├─ WebContent process
        ├─ Network process        ← network capture lives HERE, not in JS
        ├─ GPU process
        ├─ Automation controller  ← trusted input via WebKit Automation
        └─ CDP-compatible DevTools endpoint
                 │
ocean-daemon permission authority
                 │
Ocean browser tools / Chrome DevTools frontend / CDP clients
```

The agent-facing contract does not change: the 19 `browser_*` tool schemas
(quarantine-pinned in `ocean-runtime`) execute against the daemon's
permission-gated broker; the model never receives raw CDP access.

## 4. CDP compatibility layer

- Pin a specific Chromium CDP schema revision; expose standard discovery
  endpoints (`/json/version`, `/json/list`, `/devtools/browser/<id>`,
  `/devtools/page/<id>`) so the unmodified Chrome DevTools frontend and
  existing CDP clients connect.
- Domain order: (1) Target, Browser, Page, Runtime; (2) DOM, Accessibility,
  CSS; (3) Network, Fetch, Storage; (4) Input; (5) Log, Console, Debugger;
  (6) Performance, Tracing, IO; (7) workers, service workers, WebSockets,
  downloads.
- Every command and event gets a generated conformance test and a tracked
  parity status. "Parity" is earned per-domain by tests, never claimed
  informally. Chrome-product-specific concepts without a WebKit equivalent
  are emulated where possible and explicitly marked otherwise.
- Literal full-protocol parity is a continuously moving target (Chromium
  changes the protocol constantly). Ship gating is §7, not protocol
  completeness.

## 5. Repository and build model

OceanWebKit lives in a **dedicated `ocean-webkit` repository** — never a
Cargo dependency, never compiled by `cargo build`:

- Upstream WebKit revision pin + minimal reviewed patch queue.
- Reproducible macOS ARM64 build producing signed frameworks, WebContent /
  Network / GPU helper processes, symbols, the protocol schema, checksums,
  and a source archive (LGPL/BSD compliance).
- Dedicated CI publishes versioned binary SDK artifacts; Ocean Surface
  consumes a prebuilt SDK. Normal Rust build speed is preserved despite
  owning the engine.
- Upstream rebase / CVE automation is part of the program, not an
  afterthought: an unmaintained engine fork is a security liability.

**Tauri feasibility checkpoint (gates UI migration):** WRY links system
WebKit, and one process cannot safely load system WebKit plus an identically
named custom framework. Prove exactly one of: (a) the entire Tauri app —
including the local shell — links OceanWebKit; (b) the fork is packaged as a
renamed `OceanWebKit` framework with a dedicated native view bridge; (c)
WRY's macOS backend is replaced with an OceanWebKit backend.

## 6. Security invariants

- DevTools endpoint disabled unless daemon-authorized; private Unix socket,
  0700 runtime directory, peer-credential validation, per-session capability
  token, target/session binding, request nonces.
- Network body buffers bounded with a redaction policy.
- Remote pages receive no Tauri capabilities and no daemon credentials; only
  the local Surface shell does.
- Two execution worlds: an Ocean isolated world (locators, refs, DOM reads,
  actions, typed native messaging) and a page world limited to untrusted
  telemetry sensors whose messages never authorize native operations.
- Every CDP operation still passes through daemon permissions; consequential
  actions (purchases, sends, deletes) keep explicit operator confirmation.

## 7. Acceptance gates

The `agent-safari` evaluation (MIT-licensed Swift/WKWebView reference, v0.0.8)
showed the shape of a working host and the exact public-API ceiling. Its
limitations are adopted as OceanWebKit acceptance gates — each must be solved
inside the engine, not worked around:

| Gate | OceanWebKit solution |
| --- | --- |
| Complete network capture | Capture inside `NetworkProcess`: documents, scripts, CSS, images, fonts, media, fetch/XHR — with bounded response-body retention |
| WebSocket frames | Engine-level frame events |
| Service-worker / cache internals | Engine-level inspection; no page-world instrumentation substitutes |
| Response bodies | Request-ID-correlated bounded body storage as a first-class API |
| Named profiles | Stable profile directories with isolated cookies/storage/history/permissions (e.g. UUID-backed data stores) |
| Trusted input | WebKit internal Automation infrastructure (mouse, keyboard, scroll, drag, key combinations) with delivery verification |
| Isolated execution worlds | Named isolated content world for Ocean control; page world untrusted |
| No private-API shims | No `developerExtrasEnabled`-style KVC; public or fork-owned surfaces only |
| Authenticated control channel | 0700 runtime dir, 0600 socket/token, peer validation, nonces |
| Embedded surface | Child web views embedded in the Ocean Surface window — not a separate browser window |

**First hard checkpoint (before any Chromium deletion beyond the existing
default-build quarantine):**

1. Build WebKit MiniBrowser from the pinned fork.
2. Add one custom inspector/CDP command end-to-end.
3. Capture document, image, fetch, XHR, WebSocket, and service-worker traffic
   with bodies.
4. Drive trusted click/type through WebKit Automation.
5. Connect the unmodified Chrome DevTools frontend to the custom endpoint.
6. Embed the custom view in a minimal Tauri application (§5 checkpoint).
7. Sign, package, launch, and tear down every helper process.
8. Measure clean-build time, artifact size, memory, and update mechanics.

**Ship gate for the usable Ocean Browser:** the current 19 tools, profiles,
trusted input, network bodies, security invariants, signing, and updates all
pass review. Broader CDP compatibility expands continuously after that.

## 8. Milestones

Assumes a small dedicated team starting 2026-07; one engineer stretches this
roughly 2×.

| Milestone | Target |
| --- | --- |
| Custom WebKit build + Tauri embedding proof | Mid-August 2026 |
| CDP Page, Runtime, Network, Input proof | September 2026 |
| Internal alpha: visible browsing, tabs, profiles, core tools | October 2026 |
| Existing 19 Ocean tools working on OceanWebKit | January 2027 |
| Signed public beta with engine updater | Q1–Q2 2027 |
| Hardened production release | Q2–Q3 2027 |
| Broad Chrome DevTools protocol compatibility | Late 2027 onward |

## 9. Interim state (implemented 2026-07-19)

- Default builds: no browser engine; the 19 `browser_*` tools keep
  byte-identical schemas and return structured `browser_host_unavailable`;
  `/v1/browser/screencast` + `/v1/browser/input` serve the frozen
  `no-browser` contract from `browser_stream_stub.rs`.
- `legacy-chromium` preserves the full Chrome-for-Testing backend (agent
  tools + CDP screencast/input relay) and is enabled for the supervised
  daemon by `ops/install-ocean-daemon.sh`.
- CI holds both modes: compatibility lane lints
  `ocean-daemon --features legacy-chromium`; the MSRV lane checks it.
- `agent-safari` (github.com/handlecusion/agent-safari, MIT) is retained as
  the reference implementation for snapshot/ref modeling, full-page
  screenshot stitching, native click coordinate conversion, download
  lifecycle, tab-targeted RPC semantics, and wait/error contracts.

## 10. Non-goals

- No bundled or sidecar Chromium/Firefox as the permanent answer.
- No WebKit compilation inside the Rust workspace or ordinary Cargo builds.
- No raw CDP exposure to models; the daemon brokers approved operations only.
- No custom-WebKit distribution obligations on other platforms until the
  macOS program passes §7.
