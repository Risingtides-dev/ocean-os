/**
 * Ocean-OS Slack ingestion worker.
 *
 * Receives Slack Events API payloads, validates the X-Slack-Signature header
 * (HMAC-SHA256 over "v0:<timestamp>:<raw_body>"), handles URL verification
 * challenges, writes raw events to slack.events (idempotent on event_id),
 * and flattens message events into slack.messages.
 *
 * Env:
 *   PORT                   — default 8082
 *   SLACK_SIGNING_SECRET   — signing secret from your Slack app's Basic Information page
 *   OCEAN_DATABASE_URL     — Postgres connection string (write role for slack schema)
 */

import crypto from "node:crypto";
import Fastify from "fastify";
import { Pool } from "pg";

const PORT = Number(process.env.PORT ?? 8082);
const SIGNING_SECRET = process.env.SLACK_SIGNING_SECRET ?? "";
const DB_URL = process.env.OCEAN_DATABASE_URL;

// Slack replays events for up to 5 minutes; reject anything older.
const MAX_TIMESTAMP_DELTA_SECONDS = 300;

if (!SIGNING_SECRET) {
  console.warn(
    "[ingestion-slack] SLACK_SIGNING_SECRET is empty — refusing to accept events"
  );
}

const pool = new Pool({ connectionString: DB_URL });

const app = Fastify({ logger: true });

app.post("/webhook", async (req, reply) => {
  const sigHeader = req.headers["x-slack-signature"];
  const tsHeader = req.headers["x-slack-request-timestamp"];
  const raw = JSON.stringify(req.body);

  if (typeof sigHeader !== "string" || typeof tsHeader !== "string") {
    return reply.code(400).send({ error: "missing required headers" });
  }

  if (!verifySignature(raw, tsHeader, sigHeader, SIGNING_SECRET)) {
    return reply.code(401).send({ error: "invalid signature" });
  }

  const envelope = req.body as Record<string, unknown>;

  // Slack sends url_verification at the envelope level (no event wrapper).
  if (envelope.type === "url_verification") {
    return reply.code(200).send({ challenge: envelope.challenge });
  }

  if (envelope.type !== "event_callback") {
    // Acknowledge unknown envelope types without writing to DB.
    return reply.code(200).send({ ok: true });
  }

  const eventId = envelope.event_id as string | undefined;
  const teamId = envelope.team_id as string | undefined;
  const event = (envelope.event ?? {}) as Record<string, unknown>;
  const eventType = (event.type as string | undefined) ?? "unknown";
  const channelId = (event.channel as string | undefined) ?? null;
  const userId = (event.user as string | undefined) ?? null;
  const ts = (event.ts as string | undefined) ?? new Date().valueOf().toString();
  const threadTs = (event.thread_ts as string | undefined) ?? null;

  if (!eventId) {
    return reply.code(400).send({ error: "missing event_id" });
  }

  try {
    await pool.query(
      `INSERT INTO slack.events (event_id, event_type, team_id, channel_id, user_id, thread_ts, ts, payload)
       VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
       ON CONFLICT (event_id) DO NOTHING`,
      [eventId, eventType, teamId ?? null, channelId, userId, threadTs, ts, envelope]
    );

    if (isMessageEvent(eventType, event)) {
      await upsertMessage(event, channelId, userId, ts, threadTs);
    }
  } catch (err) {
    req.log.error({ err }, "ingestion failed");
    return reply.code(500).send({ error: "ingestion failed" });
  }

  return reply.code(200).send({ ok: true });
});

app.get("/health", async () => ({ ok: true }));

app.listen({ port: PORT, host: "0.0.0.0" }).catch((err) => {
  app.log.error(err);
  process.exit(1);
});

// ----------------------------------------------------------------------------

function verifySignature(
  rawBody: string,
  tsHeader: string,
  sigHeader: string,
  secret: string
): boolean {
  if (!secret) return false;

  const ts = Number(tsHeader);
  if (Number.isNaN(ts)) return false;

  const nowSeconds = Math.floor(Date.now() / 1000);
  if (Math.abs(nowSeconds - ts) > MAX_TIMESTAMP_DELTA_SECONDS) return false;

  const baseString = `v0:${tsHeader}:${rawBody}`;
  const hmac = crypto.createHmac("sha256", secret);
  hmac.update(baseString);
  const expected = `v0=${hmac.digest("hex")}`;

  if (expected.length !== sigHeader.length) return false;
  return crypto.timingSafeEqual(Buffer.from(expected), Buffer.from(sigHeader));
}

// message, message_replied, and message subtypes (message_changed etc.) all
// carry a user-visible text payload worth storing.
function isMessageEvent(eventType: string, event: Record<string, unknown>): boolean {
  if (eventType === "message") return true;
  const subtype = event.subtype as string | undefined;
  return eventType === "message" || subtype === "message_replied";
}

async function upsertMessage(
  event: Record<string, unknown>,
  channelId: string | null,
  userId: string | null,
  ts: string,
  threadTs: string | null
): Promise<void> {
  if (!channelId) return;

  const text = (event.text as string | undefined) ?? null;
  const botId = (event.bot_id as string | undefined) ?? null;

  await pool.query(
    `INSERT INTO slack.messages (ts, channel_id, thread_ts, user_id, text, bot_id)
     VALUES ($1, $2, $3, $4, $5, $6)
     ON CONFLICT (channel_id, ts) DO UPDATE
       SET thread_ts = EXCLUDED.thread_ts,
           user_id   = EXCLUDED.user_id,
           text      = EXCLUDED.text,
           bot_id    = EXCLUDED.bot_id`,
    [ts, channelId, threadTs, userId, text, botId]
  );
}
