#!/usr/bin/env bash
set -euo pipefail

# ocean-voice: toggle the optional wake-word listener service on/off.
STATUS=${DICTATE_STATUS:-$HOME/.local/bin/dictate-status}
WAKE_SERVICE=${OCEAN_VOICE_WAKE_SERVICE:-pi-voice-wake.service}
RUN_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}

if env XDG_RUNTIME_DIR="$RUN_DIR" systemctl --user is-active --quiet "$WAKE_SERVICE"; then
  env XDG_RUNTIME_DIR="$RUN_DIR" systemctl --user stop "$WAKE_SERVICE"
  "$STATUS" 'wake OFF' || true
else
  env XDG_RUNTIME_DIR="$RUN_DIR" systemctl --user start "$WAKE_SERVICE"
  "$STATUS" 'wake ON' || true
fi
( sleep 1.2; "$STATUS" '' || true ) >/dev/null 2>&1 &
