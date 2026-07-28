# Ocean Buddy apps

Ocean Buddy now has installable iPhone and Apple Watch shells around the shared
`OceanBuddyCore` Swift package.

## Shipped first slice

- `OceanBuddyPhone` — native SwiftUI iOS app.
- `OceanBuddyWatch` — native SwiftUI companion watchOS app (embedded by the
  iOS app, still runs independently of the phone).
- Both use a voice-first Ocean surface; connection and session details stay in
  a secondary settings sheet. Product and visual contracts live in
  [`PRODUCT.md`](PRODUCT.md) and [`DESIGN.md`](DESIGN.md).
- Both reuse Ocean's daemon-owned Realtime voice contract:
  `POST /v1/voice/realtime/client-secret`.
- The apps connect to OpenAI Realtime with the short-lived daemon-minted secret,
  never a stored provider key.
- Native foreground audio uses `AVAudioSession` + `AVAudioEngine`, 24 kHz mono
  PCM16, and `URLSessionWebSocketTask`.
- Server VAD supports interruption. Buddy stops queued assistant audio and sends
  `conversation.item.truncate` using conservative, completion-backed playback.
- Every connection has a monotonic generation. Stop, interruption, socket
  failure, and app backgrounding tear down the microphone, speaker queue,
  WebSocket, and stale tasks.
- `render_component` becomes one inert, bounded native card. Model-authored
  actions never become device actions. One connection may render at most four
  cards and persist at most one handoff.
- `write_handoff` uses Ocean's existing session-message seam. Workspace tools
  and unknown tools are explicitly reported unavailable on Buddy.

This remains **foreground voice**. Always-on/background Watch behavior and real
camera capture are separate hardware/privacy gates.

## Open and build

The generated Xcode project and shared schemes are committed, so XcodeGen is not
required for ordinary builds:

```bash
open integrations/ocean-buddy/OceanBuddy.xcodeproj
```

Simulator-first verification:

```bash
swift test --package-path integrations/ocean-buddy

xcodebuild \
  -project integrations/ocean-buddy/OceanBuddy.xcodeproj \
  -scheme OceanBuddyPhone \
  -configuration Debug \
  -destination 'generic/platform=iOS Simulator' \
  -derivedDataPath /tmp/ocean-buddy-derived-phone \
  CODE_SIGNING_ALLOWED=NO build

xcodebuild \
  -project integrations/ocean-buddy/OceanBuddy.xcodeproj \
  -scheme OceanBuddyWatch \
  -configuration Debug \
  -destination 'generic/platform=watchOS Simulator' \
  -derivedDataPath /tmp/ocean-buddy-derived-watch \
  CODE_SIGNING_ALLOWED=NO build
```

`project.yml` is the canonical project definition. Maintainers with XcodeGen may
regenerate the checked-in project after changing targets or build settings:

```bash
(cd integrations/ocean-buddy && xcodegen generate)
```

## Pairing

Nobody should type daemon URLs on a phone or a watch:

- **Desktop → iPhone:** the desktop shows a QR code containing
  `ocean-buddy://pair?v=1&daemon=…[&session=…]`. The iPhone app scans it
  (Connection sheet → *Scan QR from Ocean desktop*, VisionKit) or opens it as
  a deep link, confirms, and stores the connection. Generate a code today
  with:

  ```bash
  integrations/ocean-buddy/scripts/pairing-qr.sh http://risings-mac-mini.local:4780
  ```

  Payloads carry the daemon address and optional session ID only — never
  provider keys or minted credentials. Parsing validates against the same
  endpoint policy used at connect time; codes that need the Debug
  cleartext-LAN switch say so in the confirmation dialog.

- **iPhone → Watch:** the Watch is a companion app and receives the
  connection configuration automatically over WatchConnectivity whenever it
  changes on the phone. Manual entry on the Watch remains as a fallback.

- Camera scanning needs a real device; the Simulator shows a manual-entry
  fallback. A QR surface in the Ocean desktop/TUI is a follow-up; the payload
  contract above is stable.

## Watch approval cards

The Watch renders the typed `BuddyPhotoApprovalContract` approval card as an
actionable inbox item: swipe right to approve, left to dismiss, with
full-size buttons as the accessible alternative. Approval crosses interactive
WatchConnectivity to the reachable iPhone app; only the iPhone runs the mock
camera broker and posts the attached event to `POST /v1/ocean-buddy/events`,
then returns Ocean's result or error card to the Watch. An unreachable iPhone
fails closed before any backend event. Debug builds can preview the card
from Watch settings (*Development → Preview photo approval*) or via launch
environment (`SIMCTL_CHILD_OCEAN_BUDDY_DEMO_APPROVAL=1`, optionally
`…_AUTOAPPROVE=1`). Model-authored realtime voice cards remain inert and are
unrelated to this path.

## Runtime configuration

Debug builds prefill the local development endpoint, but it remains blocked
until the visible **Allow unencrypted LAN HTTP** switch is enabled:

```text
http://risings-mac-mini.local:4780
```

Release builds prefill no endpoint and require the operator to enter a real
HTTPS URL; only HTTP loopback is additionally accepted. The connection sheet
also accepts an optional Ocean session ID, which gives the voice agent its
daemon briefing and one bounded handoff target. Debug-only opt-in covers
cleartext `.local`, RFC1918, and Tailscale-IP origins. Arbitrary public
cleartext origins are always rejected, and credential/handoff requests never
follow redirects.

The daemon must have its dedicated `openai-realtime` credential configured.
Buddy receives only the resulting ephemeral client secret.
