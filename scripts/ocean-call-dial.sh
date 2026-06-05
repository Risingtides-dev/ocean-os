#!/usr/bin/env bash
# ocean-call-dial.sh — one-shot: provision the LiveKit outbound trunk (if
# needed), export the call env, restart the daemon, and place the real call.
#
# Run this the moment your LiveKit Cloud + Twilio SIP trunk exist. It turns the
# documented multi-step setup into a single command. Nothing secret is baked in;
# everything comes from env vars / args you supply at call time.
#
# Usage:
#   LIVEKIT_URL=https://xxx.livekit.cloud \
#   LIVEKIT_API_KEY=... LIVEKIT_API_SECRET=... \
#   TWILIO_TERMINATION_URI=ocean-call.pstn.twilio.com \
#   TWILIO_TRUNK_USER=... TWILIO_TRUNK_PASS=... \
#   OCEAN_CALL_CALLER_NUMBER=+18338424000 \
#   ./scripts/ocean-call-dial.sh 703-508-1859
#
# If OCEAN_CALL_OUTBOUND_TRUNK is already set (trunk pre-created), the script
# skips provisioning and goes straight to the dial.
set -euo pipefail

TO="${1:-703-508-1859}"
DAEMON_URL="${OCEAN_DAEMON_URL:-http://127.0.0.1:4780}"

require() {
  local name="$1"
  if [ -z "${!name:-}" ]; then
    echo "✗ missing required env: $name" >&2
    exit 1
  fi
}

require LIVEKIT_URL
require LIVEKIT_API_KEY
require LIVEKIT_API_SECRET
require OCEAN_CALL_CALLER_NUMBER

# --- 1. Provision the outbound trunk if we don't already have its id ---
if [ -z "${OCEAN_CALL_OUTBOUND_TRUNK:-}" ]; then
  require TWILIO_TERMINATION_URI
  require TWILIO_TRUNK_USER
  require TWILIO_TRUNK_PASS

  if ! command -v lk >/dev/null 2>&1; then
    echo "✗ LiveKit CLI 'lk' not found. Install: https://docs.livekit.io/home/cli/cli-setup/" >&2
    echo "  Then re-run, OR create the trunk in the LiveKit dashboard and set OCEAN_CALL_OUTBOUND_TRUNK." >&2
    exit 1
  fi

  TRUNK_JSON="$(mktemp)"
  # Field names verified against docs.livekit.io outbound-trunk config.
  cat >"$TRUNK_JSON" <<JSON
{
  "name": "ocean-call",
  "address": "${TWILIO_TERMINATION_URI}",
  "numbers": ["${OCEAN_CALL_CALLER_NUMBER}"],
  "authUsername": "${TWILIO_TRUNK_USER}",
  "authPassword": "${TWILIO_TRUNK_PASS}"
}
JSON

  echo "→ creating LiveKit outbound trunk…"
  CREATE_OUT="$(lk sip outbound create "$TRUNK_JSON" 2>&1)" || {
    echo "✗ trunk create failed:" >&2; echo "$CREATE_OUT" >&2; rm -f "$TRUNK_JSON"; exit 1
  }
  rm -f "$TRUNK_JSON"
  # Extract the trunk id (ST_...) from the CLI output.
  OCEAN_CALL_OUTBOUND_TRUNK="$(echo "$CREATE_OUT" | grep -oE 'ST_[A-Za-z0-9]+' | head -1)"
  if [ -z "$OCEAN_CALL_OUTBOUND_TRUNK" ]; then
    echo "✗ couldn't parse trunk id from output:" >&2; echo "$CREATE_OUT" >&2; exit 1
  fi
  echo "  ✓ trunk: $OCEAN_CALL_OUTBOUND_TRUNK"
  export OCEAN_CALL_OUTBOUND_TRUNK
fi

# --- 2. Confirm the daemon is up with the call env, else tell the operator ---
echo "→ checking daemon health at $DAEMON_URL …"
if ! curl -fsS "$DAEMON_URL/health" >/dev/null 2>&1; then
  cat >&2 <<EOF
✗ daemon not reachable at $DAEMON_URL.
  Start it WITH the call env so /v1/calls/place can dial:

    LIVEKIT_URL=$LIVEKIT_URL \\
    LIVEKIT_API_KEY=*** LIVEKIT_API_SECRET=*** \\
    OCEAN_CALL_OUTBOUND_TRUNK=$OCEAN_CALL_OUTBOUND_TRUNK \\
    OCEAN_CALL_CALLER_NUMBER=$OCEAN_CALL_CALLER_NUMBER \\
    ./target/release/ocean-daemon

  Then re-run this script.
EOF
  exit 1
fi

# --- 3. Place the call ---
echo "→ dialing $TO …"
RESP="$(curl -fsS -X POST "$DAEMON_URL/v1/calls/place" \
  -H 'content-type: application/json' \
  -d "{\"to\":\"${TO}\"}" --max-time 70)" || {
  echo "✗ dial request failed (daemon may be missing call env)." >&2; exit 1
}
echo "$RESP"

case "$RESP" in
  *'"ok":true'*) echo "✓ call placed — watch $DAEMON_URL/v1/events for the live transcript." ;;
  *'telephony not configured'*) echo "✗ daemon is running WITHOUT the call env. Restart it with the 5 vars (see step 2)." >&2; exit 1 ;;
  *) echo "✗ dial did not succeed; see response above." >&2; exit 1 ;;
esac
