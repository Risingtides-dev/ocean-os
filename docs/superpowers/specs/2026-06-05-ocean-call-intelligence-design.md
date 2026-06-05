# Ocean Call Intelligence — Design

**Status:** Approved design, pre-implementation
**Date:** 2026-06-05
**Author:** John + Ocean (brainstorming session)

## Summary

Give Ocean a real PSTN phone number that can be added to any group / conference
call. Once on the call, Ocean runs **two concurrent modes** over one shared
audio stream:

- **Passive lane (always on):** transcribe → rolling auto-summary →
  task / action-item detection → push to the Ocean app. Never speaks into the
  call.
- **Active lane (wake-word gated):** "hey Ocean…" opens a short listening
  window; Ocean answers *out loud into the call* via TTS, then goes quiet
  again. The wake word is the **mute-gate on Ocean's voice** — contained by
  design, never blurts.

## Key grounding fact

`ocean-surface` **already** has LiveKit, voice capture, streaming-capable STT
(`/api/stt`), and TTS (`/api/tts`):

- `crates/ocean-gui/src/shell/surface_livekit.rs` — LiveKit room join/token
- `crates/ocean-surface-ui/src/voice.rs` — voice capture → STT
- `crates/ocean-surface-ui/src/tts.rs` — assistant TTS playback

So the phone call becomes **just another participant in a LiveKit room**, and
the entire existing audio pipeline is reused. No new audio stack is invented.

The detected-task push reuses the **heartbeat → `/v1/agent/turns`** pattern
already proven in `crates/ocean-heartbeat/src/main.rs` (reqwest client, health
check, `room_id` / `project_id` / `session_id` body fields).

## Architecture

```
 PSTN caller ──SIP──► Twilio/Telnyx ──SIP──► LiveKit SIP gateway ──► LiveKit room
                                                                          │ (audio track)
                                                              ┌───────────┴───────────┐
                                                              │      ocean-call         │
                                                              │  (server participant)   │
                                                              ├─────────────────────────┤
                                                  PASSIVE ◄───┤ STT → summary → detect   │
                                                              │     │                     │
                                                  ACTIVE  ◄───┤ wake-word → STT window    │
                                                              │     │            │        │
                                                              └─────┼────────────┼────────┘
                                                                    ▼            ▼
                                                       POST /v1/agent/turns   TTS ► back into room
                                                                    │
                                                              OceanEvent (SSE) ► push to Ocean app
```

**The bridge:** a real phone number (Twilio or Telnyx SIP trunk) → **LiveKit
SIP gateway** → a LiveKit room.

**The new crate:** `ocean-call` — a daemon-side service that joins the LiveKit
room as a server-side participant, taps the mixed audio track, and runs the two
lanes. It talks to the daemon over the same `/v1/agent/turns` + SSE rail the
heartbeat already uses.

## Components (`ocean-call` crate)

Six focused, independently testable units:

| Unit | Responsibility | Depends on |
|---|---|---|
| `sip_bridge` | Provision the number, configure the SIP trunk (Twilio/Telnyx), accept inbound, hand the call to LiveKit's SIP gateway, map call→LiveKit room. | LiveKit SIP, provider SDK |
| `room_tap` | Join the LiveKit room as a server participant, subscribe to the mixed audio track, emit 20ms PCM frames with speaker labels. | LiveKit server SDK |
| `stt_stream` | Continuous streaming STT over the frames → timestamped, diarized transcript segments. (Deepgram / Whisper streaming — surface's batch `/api/stt` is the non-streaming cousin.) | room_tap |
| `summarizer` | Rolling window of segments → periodic auto-summary via `/v1/agent/turns` into a dedicated call session. Debounced (every N segments or T seconds of silence). | stt_stream, daemon |
| `task_detector` | Same segment stream → structured task / action-item extraction (assignee, due, source quote). Emits `CallTaskDetected` events. | stt_stream, daemon |
| `wake_agent` | Wake-word spotter on the frame stream → opens a listening window → one agent turn → TTS reply back into the room track. The voice mute-gate. | room_tap, stt, daemon, TTS |

## Data flow — the two lanes

One audio source, fanned out:

```
room_tap frames ──┬──► stt_stream ──┬──► summarizer ──► turn ──► CallSummaryUpdated
                  │                 └──► task_detector ─► turn ─► CallTaskDetected ─► push
                  └──► wake_agent (wake-word spotter) ─► window ─► turn ─► TTS ─► room
```

- **Passive lane** writes to a **dedicated call session** so transcript,
  rolling summary, and detected tasks all accumulate in one place you can open
  in the app.
- **Active lane** runs as **ephemeral turns** against that same session (so
  "hey Ocean, what'd we decide?" has the call's context) but its output is
  spoken, not pushed.

## New `OceanEvent` variants

Extending the enum at `crates/ocean-core/src/lib.rs:477`. These ride the
existing `EventEnvelope` + SSE rail, so the TUI, surface, and app all see them
with zero new transport.

- `CallStarted { call_id, room_id, participants }`
- `CallTranscriptSegment { speaker, text, start_ms, final }`
- `CallSummaryUpdated { summary, as_of_ms }`
- `CallTaskDetected { task_id, title, assignee, due, source_quote, confidence }`
- `CallWakeTriggered { utterance }`
- `CallAgentSpoke { text }`
- `CallEnded { call_id, duration_ms }`

**Push to the Ocean app = a client subscribed to the SSE stream filtering for
`CallTaskDetected`** — same mechanism, no separate push service in v1.

## Error handling & safety

- **Call drops:** `room_tap` reconnects with backoff; the call session is
  preserved.
- **STT gap:** mark segment `final:false`, never block the lane.
- **Wake-agent containment (hard-gated):** only speaks inside an open wake
  window; max one reply per trigger; configurable cooldown; a global
  `voice_muted` flag so on a sensitive label call Ocean can be passive-only.
- **Task detection is detect-and-notify ONLY** — it never auto-acts. Acting on
  a detected task is a separate, human-approved turn fired from the app.

## Testing

Each unit tested against recorded fixtures:

- canned PCM → STT golden transcripts
- transcript fixtures → summarizer / detector assertions (no live LLM in unit
  tests)
- wake-word spotter against positive / negative audio clips
- `sip_bridge` against mocked provider webhooks
- integration test replays a recorded call WAV end-to-end → asserts the event
  sequence

## Decisions locked during brainstorming

1. **Call surface:** real PSTN number (not meeting-bot, not in-app-only),
   bridged via LiveKit SIP gateway.
2. **Two modes, concurrent:** always-on passive pipeline + wake-word-gated
   active voice. Wake word's job is to gate Ocean's *voice*, not to trigger
   actions.
3. **Push v1 = SSE filter**, not a separate notification service.
4. **Task detection = detect-only.** Acting is always a separate approved turn.

## Out of scope (v1)

- Outbound calling (Ocean dialing out).
- Multi-call concurrency tuning beyond one active call session.
- A standalone push-notification service (deferred; SSE filter covers v1).
- Auto-acting on detected tasks.
