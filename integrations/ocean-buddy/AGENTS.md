# Ocean Buddy — Apple device adapter

## Purpose

This adapter projects bounded Ocean capabilities onto native iPhone and Apple
Watch apps while Ocean OS remains the session, credential, and tool authority.

## Ownership

- `Sources/OceanBuddyCore/` owns shared typed flows, native Realtime transport,
  audio lifecycle, capability narrowing, and the `BuddyPairingCode` payload
  contract.
- `Apps/Shared/` owns theme, WatchConnectivity configuration sync plus the
  foreground Watch-approval → iPhone-fulfillment request boundary
  (`BuddyDeviceSync`), and the typed card inbox controller.
- `Apps/iOS/` and `Apps/watchOS/` own thin SwiftUI shells; the iOS shell owns
  the VisionKit QR scanner and `ocean-buddy://` deep-link handling.
- `PRODUCT.md` owns audience, purpose, personality, and design principles;
  `DESIGN.md` owns the shared visual and interaction system.
- `project.yml` is the canonical Xcode target/build definition;
  `OceanBuddy.xcodeproj` is its checked-in generated artifact.
- The daemon owns Realtime briefing, tools, ephemeral credential minting, and
  session handoff persistence.

## Local Contracts

- Reuse `POST /v1/voice/realtime/client-secret`; never store or ship an OpenAI
  provider key. Mint through a fresh ephemeral, cache-disabled URL session.
- Release/default credential minting permits HTTPS and cleartext loopback only.
  Cleartext `.local`, RFC1918, and Tailscale-IP origins require both a Debug
  build and an explicit insecure-local-network opt-in. Sensitive HTTP requests
  reject redirects and cap response bytes before decoding.
- Realtime is foreground-only. App backgrounding, audio interruption, explicit
  stop, or transport failure must synchronously release the microphone and
  invalidate stale generations.
- Native WebSocket audio is 24 kHz mono PCM16 with a five-second output queue;
  playback/truncation credit derives only from completed player buffers.
- `render_component` may produce only inert bounded cards; model-authored JSON
  cannot create a device action or approval.
- `write_handoff` targets only the session frozen for the current mint and
  succeeds only after a decoded explicit `ok: true` acknowledgement.
  Unsupported workspace/unknown tools return explicit unavailable results.
- Realtime quotas span chained responses for one foreground connection: at
  most four renders, one durable handoff, and 32 total tool calls. Exhausting a
  quota terminates tool continuation.
- Buddy remains a typed capability broker, not arbitrary camera, filesystem,
  shell, network, or device control.
- Pairing (`ocean-buddy://pair?v=1&daemon=…[&session=…]`) carries the daemon
  address and optional session ID only — never provider keys or minted
  credentials. Payloads are validated against `BuddyDaemonEndpointPolicy` at
  parse time and again at connect time; a payload that needs the Debug
  cleartext-LAN switch surfaces that in the confirmation UI instead of
  silently enabling it.
- Watch configuration arrives from the iPhone over WatchConnectivity
  application context (config only, no secrets); manual entry stays as the
  fallback. Approved `photo_to_context` actions use interactive
  WatchConnectivity and fail closed when the iPhone is unreachable; the Watch
  never instantiates the iPhone camera broker or posts the attachment event.
  The Watch target is a companion app
  (`…phone.watchkitapp`, `WKRunsIndependentlyOfCompanionApp`) embedded by the
  iOS target.
- The Watch card inbox executes only the exact `BuddyPhotoApprovalContract`
  typed flow; inert model-authored realtime cards stay inert and separate.
- Do not treat simulator success as physical Watch lifecycle/battery proof.
- Simulator/CI may drive the inbox with Debug-only launch environment
  variables `OCEAN_BUDDY_DEMO_APPROVAL=1` and `OCEAN_BUDDY_DEMO_AUTOAPPROVE=1`
  (via `SIMCTL_CHILD_…`); these must never gate or alter Release behavior.

## Work Guidance

- Keep application targets thin; reusable logic and pure reducers belong in
  `OceanBuddyCore` with package tests.
- The checked-in Xcode project must stay reproducible from `project.yml`.
- Ordinary builders do not need XcodeGen; only target/build-setting changes
  require regeneration.
- Debug apps prefill the local `.local` daemon but require the visible
  cleartext-LAN opt-in. Release apps prefill no endpoint and require HTTPS for
  physical-device use; HTTP loopback remains available for local testing.
- Keep the primary app surface voice-first: status, Ocean wave identity, one
  state message, and one start/stop action. Endpoint and session fields belong
  in the connection sheet, not the main conversation flow.
- Follow `PRODUCT.md` and `DESIGN.md`; preserve Dynamic Type, VoiceOver,
  Reduce Motion, and the 40 mm Watch layout.

## Verification

```bash
swift test --package-path integrations/ocean-buddy
swift test -c release --package-path integrations/ocean-buddy
(cd integrations/ocean-buddy && xcodegen generate)
xcodebuild -project integrations/ocean-buddy/OceanBuddy.xcodeproj -scheme OceanBuddyPhone -configuration Debug -destination 'generic/platform=iOS Simulator' -derivedDataPath /tmp/ocean-buddy-derived-phone CODE_SIGNING_ALLOWED=NO build
xcodebuild -project integrations/ocean-buddy/OceanBuddy.xcodeproj -scheme OceanBuddyWatch -configuration Debug -destination 'generic/platform=watchOS Simulator' -derivedDataPath /tmp/ocean-buddy-derived-watch CODE_SIGNING_ALLOWED=NO build
```

Run a foreground connect/stop smoke test on an iOS simulator. For the Watch
inbox, pair the iPhone and Watch simulators, then drive the demo approval:

```bash
SIMCTL_CHILD_OCEAN_BUDDY_DEMO_APPROVAL=1 SIMCTL_CHILD_OCEAN_BUDDY_DEMO_AUTOAPPROVE=1 \
  xcrun simctl launch <watch-udid> dev.risingtides.oceanbuddy.phone.watchkitapp
```

Physical Watch acceptance additionally requires a real paired device and is a
separate gate.

## Child devlog Index

No nested `AGENTS.md` boundaries are currently defined.
