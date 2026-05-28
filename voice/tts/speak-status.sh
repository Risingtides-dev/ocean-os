#!/usr/bin/env bash
set -euo pipefail

# ocean-voice: play a short cached status phrase; queue rendering on a cache miss.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STATE_DIR="${OCEAN_VOICE_STATE_DIR:-$HOME/.ocean-voice}"

TEXT=${1:?usage: speak-status.sh TEXT}
CACHE_DIR=${OCEAN_VOICE_STATUS_AUDIO_DIR:-$STATE_DIR/status-audio}
SPEED=${OCEAN_VOICE_PLAYBACK_SPEED:-1.3}
mkdir -p "$CACHE_DIR"

hash=$(printf '%s' "$TEXT" | sha1sum | awk '{print $1}')
wav="$CACHE_DIR/$hash.wav"
txt="$CACHE_DIR/$hash.txt"

play_wav() {
  local file=$1
  if command -v ffplay >/dev/null 2>&1; then
    ffplay -nodisp -autoexit -loglevel error -af "atempo=${SPEED}" "$file" || true
  elif command -v ffmpeg >/dev/null 2>&1; then
    local sped="${file%.wav}-speed-${SPEED}.wav"
    ffmpeg -y -hide_banner -loglevel error -i "$file" -filter:a "atempo=${SPEED}" "$sped" && aplay -q "$sped" || aplay -q "$file" || true
  else
    aplay -q "$file" || true
  fi
}

if [[ -s "$wav" ]]; then
  play_wav "$wav"
  exit 0
fi

# Cache miss: do not block for 20 seconds in the interaction path.
# Queue generation in background and stay silent this turn.
printf '%s\n' "$TEXT" > "$txt"
nohup "$SCRIPT_DIR/render-status-phrase.sh" "$TEXT" >/dev/null 2>&1 &
exit 0
