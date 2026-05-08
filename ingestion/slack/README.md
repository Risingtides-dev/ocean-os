# ingestion/slack

Ocean-OS ingestion worker for Slack. Receives Slack Events API webhooks, verifies the request signature, and writes events to `slack.events` and `slack.messages` in the Ocean Postgres database.

## Env vars

| Variable | Required | Description |
|---|---|---|
| `SLACK_SIGNING_SECRET` | yes | Signing secret from your Slack app's **Basic Information** page |
| `OCEAN_DATABASE_URL` | yes | Postgres connection string with write access to the `slack` schema |
| `PORT` | no | HTTP port (default: `8082`) |

## Running locally

```bash
cd ingestion/slack
pnpm install
pnpm dev
```

The server starts on `http://localhost:8082`. Send a test event:

```bash
# URL verification handshake
curl -s -X POST http://localhost:8082/webhook \
  -H "Content-Type: application/json" \
  -H "X-Slack-Request-Timestamp: $(date +%s)" \
  -H "X-Slack-Signature: <compute locally — see below>" \
  -d '{"type":"url_verification","challenge":"test_challenge"}'
```

To generate a valid signature for local testing:

```bash
TIMESTAMP=$(date +%s)
BODY='{"type":"url_verification","challenge":"test_challenge"}'
SIG="v0=$(echo -n "v0:${TIMESTAMP}:${BODY}" | openssl dgst -sha256 -hmac "$SLACK_SIGNING_SECRET" | awk '{print $2}')"
```

## Slack app setup

1. Go to [api.slack.com/apps](https://api.slack.com/apps) and create a new app (or use an existing workspace app).
2. Under **Basic Information → App Credentials**, copy the **Signing Secret** into `SLACK_SIGNING_SECRET`.
3. Under **Event Subscriptions**, enable events and set the Request URL to your deployed endpoint (e.g. `https://ingestion-slack.your-domain.com/webhook`). Slack will send a `url_verification` challenge — the worker handles this automatically.
4. Subscribe to the following **Bot Events**:
   - `message.channels` — messages in public channels
   - `message.groups` — messages in private channels
   - `message.im` — DMs
   - `message.mpim` — group DMs
5. Install the app to your workspace.

## What gets written

### `slack.events`
Every inbound `event_callback` envelope is written here. Insert is idempotent on Slack's `event_id` — re-delivery is a no-op.

### `slack.messages`
Flattened rows for `message` events. Keyed on `(channel_id, ts)`. Re-delivery upserts the row (last-writer-wins on text/thread_ts). Thread replies are stored with their `thread_ts` so callers can reconstruct conversations.

## What's out of scope (separate workers)

- Embedding Slack messages into `embeddings` (will be a separate embedding worker)
- Backfilling channel history via `conversations.history`
- Reaction events (`reaction_added`, `reaction_removed`)
