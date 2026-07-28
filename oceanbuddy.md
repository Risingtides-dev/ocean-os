# Ocean Buddy

Ocean Buddy is Ocean's **free-device workstation**: work is not trapped on one machine or assigned to one permanent screen. The devices around the operator take bounded roles while Ocean remains the routing and record authority.

- **Apple Watch — Buddy:** the approval puck and command remote. It renders small approval, result, and error cards.
- **iPhone — Sensor:** the device-capability broker. It performs an approved capture and returns bounded attachment metadata.
- **iPad — Station:** the ambient workstation display for context, status, handoff, and results.
- **Ocean OS — Authority:** routes requests, enforces the capability boundary, and ultimately records the event lifecycle and resulting context.

The product principle is: **agents request context; Watch grounds permission; iPhone captures; iPad displays; Ocean routes and records.** Ocean Buddy is a **capability broker, not raw device control**. Agents never receive arbitrary camera, microphone, filesystem, or device-compute access. Ocean brokers a typed request, the user approves its declared action and target, the broker runs only that format, and Ocean checks again when a later step needs a new capability.

## Implemented slices

### Context-attachment mock

```text
Watch approval card
  “Attach Photo to Current Ocean Context.”
        |
        | user approves typed action targeting iPhone
        v
iPhone mock camera broker
        |
        | attached event (zero-byte JPEG metadata only)
        v
POST /v1/ocean-buddy/events
        |
        | result card, or local phone-unavailable error card
        v
Watch card renderer
```

The Rust contract is `crates/ocean-agent-sdk/src/buddy.rs`; daemon ingress is `crates/ocean-daemon/src/ocean_buddy.rs`. The daemon accepts only a mocked `attached` event and returns a result card. It does not upload or persist image bytes. The Watch app renders this typed approval card as an actionable inbox item — swipe right to approve, left to dismiss, buttons as the accessible equivalent. Approval crosses interactive WatchConnectivity to a reachable iPhone; only the iPhone runs the mock camera broker and real daemon ingress, then returns Ocean's result or error card. An unreachable phone fails closed before any backend event. Capture itself remains the in-package mock; real camera support belongs behind the **Camera Broker** on iPhone, and camera behavior must not be placed inside the Watch view.

### Pairing and configuration

Setup is pair-not-type: the desktop shows an `ocean-buddy://pair?v=1&daemon=…[&session=…]` QR code (`integrations/ocean-buddy/scripts/pairing-qr.sh` today; a desktop/TUI surface is a follow-up), the iPhone scans it with VisionKit or opens it as a deep link, and the Watch — now a companion app of the iPhone app — inherits the configuration automatically over WatchConnectivity application context. Pairing payloads carry the daemon address and optional session ID only, never provider keys or minted credentials, and are validated against the same endpoint policy at parse and connect time.

### PWA wrapper decision

Wrapping the Ocean Surface PWA was evaluated for this slice: WebKit/WKWebView does not exist on watchOS, so the Watch experience must stay native regardless. An iPhone WKWebView shell hosting Surface remains a possible later addition beside the native voice surface; it was deferred to keep the voice-first native slice small.

### Foreground Realtime voice

`integrations/ocean-buddy/OceanBuddy.xcodeproj` now contains real SwiftUI iPhone and independent watchOS app targets around `OceanBuddyCore`:

```text
OceanBuddyPhone / OceanBuddyWatch
        |
        | POST /v1/voice/realtime/client-secret
        v
Ocean daemon
  session briefing + tool policy + ephemeral credential
        |
        | short-lived credential only
        v
OpenAI Realtime WebSocket
        ^
        | AVAudioEngine: 24 kHz mono PCM16 mic + speaker
        |
foreground native Buddy conversation
```

The apps reuse Ocean's existing Realtime agent rather than creating another voice stack. They implement generation-guarded connect/stop, terminal provider-error teardown, bounded input/output queues, conservative server-VAD barge-in truncation, local assistant transcripts, and explicit tool fulfillment. `render_component` becomes one inert bounded card; it cannot create an approval or device action. A foreground connection permits at most four renders and one durable `write_handoff`, which uses the existing daemon session-message route. Workspace and unknown tools are reported unavailable instead of being silently claimed. Release builds require HTTPS for non-loopback daemons; Debug-only cleartext LAN access is an explicit opt-in, and sensitive requests reject redirects.

The native shells are voice-first: status, the Ocean wave mark, one state message, and one persistent start/stop action occupy the primary surface. Endpoint and optional session configuration live in a secondary sheet. The durable product and visual contracts are `integrations/ocean-buddy/PRODUCT.md` and `integrations/ocean-buddy/DESIGN.md`.

The native voice loop is accepted only as a simulator-tested foreground slice. Physical Watch connectivity, battery behavior, and watchOS lifecycle remain a separate hardware gate; always-on/background operation is not claimed.

## Context Attachment Gestures

Context Attachment Gestures are small, explicit ways to ground an agent in the operator's physical environment. These are concept directions, not implemented features:

- **Photo to Context:** request one photo and attach it to the current Ocean context after approval. The metadata-only mock is the implemented slice.
- **Bug Lens:** capture a visible defect, screen, device, or environment and package it as debugging context.
- **Whiteboard Harvest:** capture a board, preserve the source image, and later derive structured notes with provenance.
- **Meeting Capture:** request a bounded, consent-aware meeting capture and produce an attributed summary or handoff.
- **Camera Broker:** mediate a declared camera format, target, duration, and confirmation instead of exposing the camera directly.
- **Resource Broker Card:** request bounded device compute, storage, network, or sensor capability with visible limits.
- **Dead Man Switch:** require an explicit heartbeat or escalation decision for a predeclared workflow; never infer consent from silence.

## First MOP action schema

Every action is explicit and self-describing:

- `id`: UUID identifying the action instance.
- `label`: operator-facing button label; the first flow uses `Approve`.
- `kind`: typed capability request; the first flow uses `photo_to_context`.
- `requires_confirmation`: whether a human approval is required; `true` for the first flow.
- `target_device`: device broker expected to fulfill it; `i_phone` for the first flow.

The Watch renders the approval card. Approval emits an event carrying the complete action, and the action targets iPhone—not the Watch. The iPhone broker may fulfill only that typed action. A different format, capability, target, or follow-on operation requires another checked contract and, when appropriate, another confirmation.

Example action:

```json
{
  "id": "20000000-0000-4000-8000-000000000001",
  "label": "Approve",
  "kind": "photo_to_context",
  "requires_confirmation": true,
  "target_device": "i_phone"
}
```

## Event schema and lifecycle

Each event is an appendable lifecycle fact with these common fields:

- `event_id`: unique event UUID.
- `flow_id`: stable UUID shared by the entire gesture.
- `causation_id`: optional preceding event UUID.
- `state`: one lifecycle state.
- `occurred_at`: RFC 3339 timestamp.

State-specific fields are additive:

- `action`: required for `requested` and `approved`; also identifies the broker for `capture_started`.
- `attachment`: used by `capture_completed`, `uploaded`, and `attached`.
- `target`: attachment destination; the first flow uses `current_ocean_context`.
- `card`: Watch-facing result or error card.
- `failure`: stable code, human message, and retryability.

Lifecycle states are:

1. `requested` — Ocean created a typed request and Watch can render it.
2. `approved` — the operator confirmed the action on Watch.
3. `capture_started` — the target broker began the declared capture.
4. `capture_completed` — the broker completed the capture locally.
5. `uploaded` — bounded attachment transfer completed.
6. `attached` — Ocean associated the attachment with its declared context.
7. `result` — Watch received a clear success/result card.
8. `failed` — the flow stopped with a stable failure and clear error card.

The fixtures show the complete schema sequences:

- Happy path: `docs/examples/ocean-buddy/happy-path.json`
- Failure path: `docs/examples/ocean-buddy/phone-unavailable.json`

The happy fixture models the full future lifecycle. The package flow performs request/approval, mocked capture, `attached`, and `result`; the native Watch approval invokes it only through the reachable iPhone app. It does not implement upload streaming or durable lifecycle recording.

### Failure: phone unavailable

After approval, the iPhone broker can report `phone_unavailable`. No backend attachment event is sent. Watch renders an `error_card` titled **“Photo was not attached.”** with the detail **“iPhone is unavailable. Bring it online and try again.”** The failure is retryable, but retry UI is not implemented.

## Second-wave cards — documentation only

These proposed components are not implemented:

- Bug Lens card
- Whiteboard Harvest card
- Meeting Capture card
- Camera Broker card
- Resource Broker card
- Dead Man Switch card
- Handoff card
- Pulse card

They must reuse the capability-broker rule: typed request, visible scope, explicit target, grounded approval, bounded fulfillment, clear result or failure. They must not become raw device-control surfaces.

## Deliberately left for later

- Physical Watch Realtime acceptance, battery profiling, and lifecycle testing
- Always-on/background or Siri-style Watch operation
- Real iPhone camera capture and media upload
- Background/non-interactive WatchConnectivity delivery and real cross-device sensor data
- Server lifecycle streaming and durable Buddy event recording
- iPad Station UI
- Retry and second-confirmation flows
- Any editor, terminal, dashboard, arbitrary compute, or broader component surface

## Verify

```bash
cargo fmt --all --check
cargo test -p ocean-agent-sdk buddy
cargo test -p ocean-daemon ocean_buddy
swift test --package-path integrations/ocean-buddy
xcodebuild -project integrations/ocean-buddy/OceanBuddy.xcodeproj -scheme OceanBuddyPhone -configuration Debug -destination 'generic/platform=iOS Simulator' -derivedDataPath /tmp/ocean-buddy-derived-phone CODE_SIGNING_ALLOWED=NO build
xcodebuild -project integrations/ocean-buddy/OceanBuddy.xcodeproj -scheme OceanBuddyWatch -configuration Debug -destination 'generic/platform=watchOS Simulator' -derivedDataPath /tmp/ocean-buddy-derived-watch CODE_SIGNING_ALLOWED=NO build
```
