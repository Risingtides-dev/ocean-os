#!/usr/bin/env bash
set -euo pipefail

# ocean-voice: panic-stop recording, STT, agent waits, and audio playback.
# Assumes this workstation's dictate-hotkey toolchain; kills are best-effort.
STATUS=${DICTATE_STATUS:-$HOME/.local/bin/dictate-status}
VOICE_PORT=${OCEAN_VOICE_PORT:-8787}

pkill -INT -f '^arecord .* /tmp/dictate\.wav' 2>/dev/null || true
sleep 0.2
pkill -TERM -f '^arecord .* /tmp/dictate\.wav' 2>/dev/null || true

pkill -TERM -f "$HOME/tools/dictate-hotkey/process-dictation\.sh" 2>/dev/null || true
pkill -TERM -f "$HOME/tools/dictate-hotkey/process-voice-agent\.sh" 2>/dev/null || true
pkill -TERM -f "$HOME/tools/dictate-hotkey/transcribe-audio\.sh" 2>/dev/null || true
pkill -TERM -f 'xai-stt\.sh' 2>/dev/null || true
pkill -TERM -f 'curl .*api\.x\.ai/v1/stt' 2>/dev/null || true
pkill -TERM -f "curl .*127\.0\.0\.1:${VOICE_PORT}/prompt" 2>/dev/null || true
# Match the ocean-voice TTS scripts by name, wherever they are installed.
pkill -TERM -f 'speak-response\.sh' 2>/dev/null || true
pkill -TERM -f 'speak-status\.sh' 2>/dev/null || true
pkill -TERM -f 'xai-tts-wav\.sh' 2>/dev/null || true
pkill -TERM -f 'curl .*api\.x\.ai/v1/tts' 2>/dev/null || true
pkill -TERM -x aplay 2>/dev/null || true
pkill -TERM -x ffplay 2>/dev/null || true
pkill -TERM -f 'ffmpeg .*atempo=' 2>/dev/null || true

rm -f /tmp/dictate-recording /tmp/dictate.wav
"$STATUS" 'STOPPED' || true
# Leave STOPPED visible briefly, then clear it in background.
( sleep 1.2; "$STATUS" '' || true ) >/dev/null 2>&1 &
