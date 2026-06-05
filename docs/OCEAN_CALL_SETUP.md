# Ocean Call — Setup Runbook (the last mile)

The `ocean-call` agent is **built and verified end to end**. The only thing
between it and a real call to 703-508-1859 is provisioning two paid accounts and
setting five environment variables. This is that checklist.

Sources: Twilio steps from the official LiveKit guide
(`docs.livekit.io/sip/quickstarts/configuring-twilio-trunk`); LiveKit outbound
trunk fields verified against `livekit-api 0.5.0` source.

---

## What's already done (no action needed)

- `POST /v1/calls/place {"to":"703-508-1859"}` — the dial trigger. Today it
  returns `503 { blocked_on: "telephony not configured", missing: ... }`
  naming exactly which env var is unset. Once the vars below are set, the same
  request dials for real.
- The full pipeline (dial → audio tap → transcribe → summarize → detect tasks →
  wake-word) runs live in the daemon — proven via `POST /v1/calls/demo`, which
  streams real `call_*` events on `GET /v1/events`.
- 703-508-1859 is already a **verified caller ID** on the Twilio account.

---

## Step 1 — Twilio: upgrade + outbound SIP trunk (PAID)

The trial account cannot do production SIP trunking. Upgrade first (this is the
paid click), then:

1. **Buy/confirm a phone number** (the auto-provisioned toll-free works once
   verified; a local number is fine too). This is Ocean's caller ID →
   `OCEAN_CALL_CALLER_NUMBER` (E.164, e.g. `+18338424xxx`).
2. **Create an Elastic SIP Trunk:**
   ```
   twilio api trunking v1 trunks create \
     --friendly-name "ocean-call" \
     --domain-name "ocean-call.pstn.twilio.com"
   ```
   Save the returned **trunk SID**.
3. **Create a Credential List** (Console → Voice → Credential lists): pick a
   username + password. Note them.
4. **Attach credentials to the trunk:** Elastic SIP Trunking → Manage → Trunks →
   your trunk → Termination → Authentication → Credential Lists → select it.
   Note the **Termination SIP URI** shown here (e.g.
   `ocean-call.pstn.twilio.com`).
5. **Link the phone number to the trunk:**
   ```
   twilio api trunking v1 trunks phone-numbers create \
     --trunk-sid <TRUNK_SID> --phone-number-sid <PN_SID>
   ```

## Step 2 — LiveKit Cloud: account + outbound trunk

1. Create a **LiveKit Cloud** project (SIP is available on Cloud out of the
   box). From the project settings, grab:
   - **URL** → `LIVEKIT_URL` (e.g. `https://ocean-xxxx.livekit.cloud`)
   - **API Key** → `LIVEKIT_API_KEY`
   - **API Secret** → `LIVEKIT_API_SECRET`
2. **Create the outbound trunk** pointing at Twilio's termination URI. Via the
   LiveKit CLI or `SIPClient::create_sip_outbound_trunk(name, address, numbers,
   options)` — argument/field names verified against `livekit-api 0.5.0`:
   - `address` = the Twilio Termination SIP URI from Step 1.4
     (`ocean-call.pstn.twilio.com`)
   - `numbers` = `[OCEAN_CALL_CALLER_NUMBER]`
   - `options.auth_username` / `options.auth_password` = the credential-list
     creds from Step 1.3
   - `options.transport` = `SipTransport` (UDP/TCP/TLS; match the trunk)
   Save the returned trunk's `sip_trunk_id` → `OCEAN_CALL_OUTBOUND_TRUNK`
   (e.g. `ST_...`).

## Step 3 — set the five env vars and dial

Export these where the daemon runs (keep secrets out of shell history /
transcripts — use a file the daemon sources):

```
LIVEKIT_URL=https://ocean-xxxx.livekit.cloud
LIVEKIT_API_KEY=...
LIVEKIT_API_SECRET=...
OCEAN_CALL_OUTBOUND_TRUNK=ST_...
OCEAN_CALL_CALLER_NUMBER=+18338424xxx
```

Restart the daemon, then:

```
curl -X POST http://127.0.0.1:4780/v1/calls/place \
  -H 'content-type: application/json' \
  -d '{"to":"703-508-1859"}'
```

The phone rings. Subscribe to `GET /v1/events` to watch the live transcript,
summary, and detected-task stream.

---

## Verifying it worked

- `place` returns `200 { ok:true, dialed:"+17035081859", room:"call:...", ... }`
  instead of `503`.
- `/v1/events` shows `call_started` → `call_transcript_segment` → … →
  `call_ended`.
- If `place` still 503s, the JSON `missing` field names the exact unset var.

## Still TODO in code (small, not account-gated)

- **TTS back into the room** for the wake-word active lane (Ocean speaking its
  answer out loud). The decision logic + event (`CallAgentSpoke`) exist; only
  the audio-publish-into-room adapter remains.
- Inbound trunk + dispatch rule (so people can *call* Ocean), if/when wanted —
  outbound (Ocean calls out) is the current target.
