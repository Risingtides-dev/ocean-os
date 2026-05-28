#!/usr/bin/env bash
set -euo pipefail

# ocean-voice: render one short status phrase to the WAV cache via xAI Grok TTS
# (locked, idempotent).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STATE_DIR="${OCEAN_VOICE_STATE_DIR:-$HOME/.ocean-voice}"

TEXT=${1:?usage: render-status-phrase.sh TEXT}
CACHE_DIR=${OCEAN_VOICE_STATUS_AUDIO_DIR:-$STATE_DIR/status-audio}
mkdir -p "$CACHE_DIR"

hash=$(printf '%s' "$TEXT" | sha1sum | awk '{print $1}')
wav="$CACHE_DIR/$hash.wav"
txt="$CACHE_DIR/$hash.txt"
lock="$CACHE_DIR/$hash.lock"

[[ -s "$wav" ]] && exit 0
exec 9>"$lock"
flock -n 9 || exit 0
[[ -s "$wav" ]] && exit 0

printf '%s\n' "$TEXT" > "$txt"
tmp="$wav.tmp"
"$SCRIPT_DIR/xai-tts-wav.sh" "$txt" "$tmp"
mv "$tmp" "$wav"
