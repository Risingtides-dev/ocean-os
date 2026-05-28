# ocean-voice

A **voice surface for the Ocean runtime** — a thin steering client, in the same
spirit as `ocean-cli` and `ocean-tui`. It captures speech (or text), hands the
prompt to the Rust `ocean-daemon`, and speaks the answer back.

ocean-voice owns **no agent logic, no provider credentials, and no sessions**.
The Ocean runtime owns all of that. This package is just ears, a mouth, and a
thin wire to the daemon.

## Architecture

```
  Browser / phone (PWA)            Desktop (hands-free)            Terminal
        │  mic + UI                      │  wake / hotkey               │
        ▼                                ▼                              ▼
  web-server.mjs  ──┐             daemon.mjs  ──┐               cli.mjs  ──┐
  (STT/TTS proxy)   │             (TTS + status) │              (one-shot) │
        :8790       │                 :8787      │                         │
                    └────────────► ocean-client.mjs ◄──────────────────────┘
                                   POST /v1/agent/turns
                                   GET  /v1/agent/events (SSE)
                                          │
                                          ▼
                                   ocean-daemon  (Rust)
                                   :4780 — owns sessions, tools,
                                   provider calls, permissions
```

All three surfaces share `src/ocean-client.mjs`, the single module that speaks
the daemon's product-shaped agent API. The assistant's reply text is recovered
from the `assistant_text_delta` events on the SSE stream; tool activity drives
spoken status phrases while the runtime works.

## Layout

| Path | Purpose |
|------|---------|
| `src/ocean-client.mjs` | The only thing that talks to `ocean-daemon`. |
| `src/web-server.mjs`   | Web/PWA connector (`:8790`): serves the PWA, proxies xAI STT/TTS, forwards prompts. |
| `src/daemon.mjs`       | Desktop voice daemon (`:8787`): local TTS + spoken status phrases. |
| `src/cli.mjs`          | One-shot CLI: `node src/cli.mjs "your prompt"`. |
| `public/`              | Installable PWA (mic capture, UI, service worker). |
| `tts/`                 | xAI Grok TTS scripts for the desktop daemon. |
| `desktop/`             | Optional workstation glue (OSD, wake toggle, panic-stop). |
| `config/`              | Voice profile + status-phrase list. |
| `cache/`               | Pre-render status phrases into the WAV cache. |

## Prerequisites

- **`ocean-daemon` running** (default `http://127.0.0.1:4780`). ocean-voice is
  useless without it.
- **Node ≥ 18** (uses built-in `fetch`).
- **xAI API key** (`XAI_API_KEY`) for speech: browser STT, web TTS, and desktop
  TTS all use xAI Grok. Without it, you can still type prompts and read replies.
- Desktop audio path also needs `ffmpeg`/`ffplay` (or `aplay`) on the box.

## Run

```bash
cp .env.example .env   # fill in XAI_API_KEY, OCEAN_VOICE_CWD, etc.

npm run web      # web/PWA connector at http://127.0.0.1:8790
npm run daemon   # desktop voice daemon at http://127.0.0.1:8787
node src/cli.mjs "summarise the ocean roadmap"
```

Configuration is entirely via env vars — see `.env.example`. The only thing
ocean-voice strictly needs is `OCEAN_DAEMON_URL`.

## Phone / Tailscale (tide-net)

Phone microphone capture requires HTTPS. Keep `ocean-daemon` bound to localhost
(it has no HTTP auth) and expose only the connector, token-gated, behind
Tailscale Serve:

```bash
VOICE_WEB_HOST=0.0.0.0 \
VOICE_WEB_PORT=8790 \
VOICE_WEB_TOKEN='<long-random-token>' \
npm run web

# then, in another shell, put HTTPS in front:
tailscale serve https / http://127.0.0.1:8790
```

The connector **refuses to bind beyond localhost without a token** (or HTTP
Basic auth via `VOICE_WEB_USER`/`VOICE_WEB_PASSWORD`).

## Health

- Connector: `GET http://127.0.0.1:8790/api/health` → connector + daemon reachability.
- Desktop daemon: `GET http://127.0.0.1:8787/health`.

## Security

- **No secrets live in this repo.** Credentials come from the environment (or,
  for the desktop daemon's xAI key, an optional `XAI_SETTINGS_FILE`). The Ocean
  runtime holds the model/provider keys.
- `ocean-daemon` has no HTTP auth and must stay on localhost; the token-gated
  connector is the only thing that should ever be network-exposed.
- Runtime state, sessions, cached audio, and `.env` are git-ignored.

## Speech

xAI Grok is the **only** TTS engine (voice configurable via
`~/.ocean-voice/xai-voice.txt`, default `leo`). If xAI is unavailable the voice
stays silent — the reply text is still written to the response file and shown in
the UI.
