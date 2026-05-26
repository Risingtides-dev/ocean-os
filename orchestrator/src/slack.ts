/**
 * Slack DM helper.
 * Sends a message to a Slack user using the bot token from env.
 * Uses the Slack Web API chat.postMessage with channel = user ID (opens a DM).
 */

const SLACK_API = "https://slack.com/api/chat.postMessage";

export async function sendSlackDm(slackUserId: string, text: string): Promise<void> {
  const token = process.env.SLACK_BOT_TOKEN;
  if (!token) {
    throw new Error("SLACK_BOT_TOKEN not set — cannot send DM");
  }

  const res = await fetch(SLACK_API, {
    method: "POST",
    headers: {
      "Content-Type": "application/json; charset=utf-8",
      Authorization: `Bearer ${token}`,
    },
    body: JSON.stringify({ channel: slackUserId, text }),
  });

  if (!res.ok) {
    throw new Error(`Slack API HTTP ${res.status}`);
  }

  const body = (await res.json()) as { ok: boolean; error?: string };
  if (!body.ok) {
    throw new Error(`Slack API error: ${body.error ?? "unknown"}`);
  }
}

export function buildEscalationMessage(task: {
  id: string;
  human_owner: string;
  description: string;
  stall_reason: string;
  rounds: number;
  max_rounds: number;
  twin_id: string | null;
}): string {
  return (
    `:warning: *Task stalled — needs your attention*\n\n` +
    `*Task:*   ${task.description}\n` +
    `*Reason:* ${task.stall_reason}\n` +
    `*Rounds:* ${task.rounds} / ${task.max_rounds}\n` +
    `*Twin:*   ${task.twin_id ?? "(unassigned)"}\n` +
    `*Task ID:* ${task.id}\n\n` +
    `Check your queue:\n` +
    `\`GET /queue/${task.human_owner}\``
  );
}
