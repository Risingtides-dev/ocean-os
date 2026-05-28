#!/usr/bin/env bash
set -euo pipefail

# ocean-voice: speak a response file aloud via xAI Grok TTS.
# If xAI is unavailable, stay silent — the text is still written to the
# response file and shown in the UI.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUN_UID=$(id -u)

TEXT_FILE=${1:-/tmp/ocean-voice-response.txt}
OUT=${2:-/tmp/ocean-voice-response-${RUN_UID}.wav}
SPEED=${OCEAN_VOICE_PLAYBACK_SPEED:-1.3}
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$RUN_UID} PULSE_SERVER=${PULSE_SERVER:-unix:/run/user/$RUN_UID/pulse/native}

[[ -s "$TEXT_FILE" ]] || exit 0

play_wav() {
  local wav=$1
  if command -v ffplay >/dev/null 2>&1; then
    ffplay -nodisp -autoexit -loglevel error -af "atempo=${SPEED}" "$wav" || true
  elif command -v ffmpeg >/dev/null 2>&1; then
    local sped="${wav%.wav}-speed-${SPEED}.wav"
    ffmpeg -y -hide_banner -loglevel error -i "$wav" -filter:a "atempo=${SPEED}" "$sped" && aplay -q "$sped" || aplay -q "$wav" || true
  else
    aplay -q "$wav" || true
  fi
}

if "$SCRIPT_DIR/xai-tts-wav.sh" "$TEXT_FILE" "$OUT"; then
  play_wav "$OUT"
else
  echo "[speak-response] xAI TTS unavailable; staying silent" >&2
fi
