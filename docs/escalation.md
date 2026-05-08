# Orchestrator escalation policy

When a task the orchestrator is running stalls — because the agent hit its round cap, a twin became unreachable, or errors kept repeating — the orchestrator escalates the task back to the human who owns it. This document defines when escalation fires, what the human receives, and how the state is recorded.

---

## When escalation fires

A task is considered stalled — and escalation is triggered — when **any one** of the following conditions becomes true:

| Condition | Threshold | `stall_reason` value |
|---|---|---|
| Round cap reached | `rounds >= max_rounds` (default 10) | `max_rounds` |
| Repeated failures | `consecutive_failures >= 3` | `repeated_failure` |
| Manual trigger | `POST /tasks/:id/simulate-stall` called | `simulated` |

Escalation fires **once per task** — if a task is already in `escalated` status the orchestrator will not send a second DM or overwrite `escalated_at`.

---

## What happens during escalation

1. The orchestrator's stall detector (runs every 60 s) finds tasks where a stall condition is true and `escalated_at IS NULL`.
2. It posts a Slack DM to the task's `human_owner` (a Slack user ID).
3. It updates the task row: `status = 'escalated'`, `escalated_at = now()`.

The Slack message looks like this:

```
⚠️ Task stalled — needs your attention

Task:    <task description>
Reason:  <stall reason>
Rounds:  <rounds> / <max_rounds>
Twin:    <twin_id>
Task ID: <uuid>

Check your queue:
GET /queue/<your-slack-user-id>
```

---

## Task lifecycle

```
created → running → stalled → escalated
                 ↘ completed
```

- `running` — orchestrator is actively working rounds
- `stalled` — stall condition hit; escalation has not yet fired (between detector ticks)
- `escalated` — DM sent, human has been notified
- `completed` — task resolved before stall

---

## Queue view (stalled tasks per human)

`GET /queue/:slack_user_id`

Returns the JSON list of tasks in `stalled` or `escalated` status for a given human owner. No auth (Ocean-OS is internal-only at this stage). Example response:

```json
{
  "human": "U0A1BXYZ",
  "stalled": [],
  "escalated": [
    {
      "id": "4e1c2a3b-...",
      "description": "Sync campaign brief to Content Lab",
      "twin_id": "smaths-bot",
      "stall_reason": "max_rounds",
      "rounds": 10,
      "max_rounds": 10,
      "escalated_at": "2026-05-08T15:00:00Z"
    }
  ]
}
```

---

## Out of scope (this iteration)

- Cross-human reassignment when a human is OOO — separate issue
- UI for the queue — `GET /queue/:id` returns JSON; dashboard is future work
- Escalation SLA tracking (how long a task sat escalated before resolution)
