#!/usr/bin/env bash
set -euo pipefail

# ocean-voice: synthesize speech with xAI Grok TTS and write a mono 24kHz WAV.
STATE_DIR="${OCEAN_VOICE_STATE_DIR:-$HOME/.ocean-voice}"
XAI_SETTINGS_FILE="${XAI_SETTINGS_FILE:-$HOME/.pi/agent/settings.json}"

TEXT_FILE=${1:?usage: xai-tts-wav.sh TEXT_FILE OUT_WAV}
OUT_WAV=${2:?usage: xai-tts-wav.sh TEXT_FILE OUT_WAV}
API_KEY="${XAI_API_KEY:-}"
if [[ -z "$API_KEY" && -f "$XAI_SETTINGS_FILE" ]]; then
  API_KEY=$(SETTINGS="$XAI_SETTINGS_FILE" python3 - <<'PY'
import json, os
try:
    print(json.load(open(os.environ['SETTINGS'])).get('xai', {}).get('apiKey', ''))
except Exception:
    print('')
PY
)
fi
if [[ -z "$API_KEY" ]]; then
  echo "missing XAI_API_KEY" >&2
  exit 1
fi

VOICE=$(cat "$STATE_DIR/xai-voice.txt" 2>/dev/null | head -n 1 | tr -d '\r' | xargs || true)
VOICE=${VOICE:-leo}
TMP_MP3=$(mktemp /tmp/xai-tts-XXXXXX.mp3)
TMP_JSON=$(mktemp /tmp/xai-tts-XXXXXX.json)
cleanup() { rm -f "$TMP_MP3" "$TMP_JSON"; }
trap cleanup EXIT

python3 - "$TEXT_FILE" "$VOICE" > "$TMP_JSON" <<'PY'
import json, pathlib, sys
text = pathlib.Path(sys.argv[1]).read_text(errors='ignore').strip()[:15000]
voice = sys.argv[2]
print(json.dumps({
    'model': 'grok-tts',
    'text': text,
    'voice': voice,
    'language': 'en',
    'response_format': 'mp3',
}))
PY

curl -fsS --max-time 60 -o "$TMP_MP3" "https://api.x.ai/v1/tts" \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  --data-binary "@$TMP_JSON"

ffmpeg -y -hide_banner -loglevel error -i "$TMP_MP3" -f wav -ac 1 -ar 24000 "$OUT_WAV"
