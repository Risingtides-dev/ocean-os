# Artifact lifecycle definition

## What is an artifact?

An artifact is any discrete work product that a twin creates, modifies, or is responsible for completing, and whose lifecycle state the orchestrator must track. Artifacts have:

- A canonical **home** — the system that owns its authoritative state
- A well-defined set of **terminal states** — conditions under which the orchestrator stops watching
- A defined **detection mechanism** — how the orchestrator learns about state changes
- A **`max_rounds` cap** — a ceiling on the number of orchestrator-initiated interventions before escalation

### Canonical artifact types

| Type | Canonical home | Created by |
|---|---|---|
| Pull request | GitHub | Twin or human |
| Issue | GitHub | Human (task spec) or twin (sub-task) |
| Notion document | Notion | Twin or human |
| Slack canvas | Slack | Twin |
| Google Drive file | Google Drive | Twin or human |
| Media job | Content Lab (internal) | Twin |

Other Ocean-OS entities (campaign bookings, payment records, Telegram distributions) are **data records**, not artifacts. The orchestrator does not watch them for lifecycle state; it reads them as context when processing an artifact.

---

## Artifact types — detail

### Pull request

**Home:** GitHub (`github.com/Risingtides-dev/*`)

**States:**

| State | Terminal? |
|---|---|
| `draft` | No |
| `open` | No |
| `changes_requested` | No |
| `approved` | No |
| `merged` | Yes |
| `closed` (unmerged) | Yes |

**Terminal condition:** `merged` or `closed`.

**Detection:** GitHub webhook — `pull_request` and `pull_request_review` events. The ingestion worker writes every event to `github.events`. The orchestrator subscribes to the `pull_request.closed` and `pull_request_review.submitted` event types. No polling required; webhook coverage is reliable.

**`max_rounds`:** 6. A round is one orchestrator action on a PR (push a commit in response to review feedback, resolve a comment thread, update the PR description). If 6 rounds pass without reaching a terminal state, escalate.

---

### Issue

**Home:** GitHub

**States:**

| State | Terminal? |
|---|---|
| `open` | No |
| `open` + labeled `blocked` | No (but triggers escalation check) |
| `closed` (completed) | Yes |
| `closed` (not planned) | Yes |

**Terminal condition:** `closed`.

**Detection:** GitHub webhook — `issues` event type, `closed` action. The orchestrator also watches for the `labeled` action to catch `blocked` labels that might need intervention. No polling required.

**`max_rounds`:** 4. A round is one orchestrator action on an issue (post a comment, update the body, add a label). Issues are generally driven by humans after the twin's initial work; the orchestrator's role is narrow.

---

### Notion document

**Home:** Notion

**States:**

| State | Terminal? |
|---|---|
| `draft` (no status property) | No |
| `in_review` | No |
| `approved` | Yes |
| `archived` | Yes |

Status is a Notion select property named `Status` on the page. If no `Status` property exists, the orchestrator treats the page as `draft`.

**Terminal condition:** `Status` = `Approved` or `Archived`.

**Detection:** Notion webhooks exist but are unreliable — they have no retry guarantees and miss events under load. Strategy: **subscribe to webhooks as primary**, **poll every 10 minutes as fallback**. Poll queries pages modified in the last 15 minutes using the Notion search API (`filter: { last_edited_time: { after: ... } }`). The ingestion worker deduplicates by `last_edited_time`; the event log is idempotent on `(page_id, last_edited_time)`.

**`max_rounds`:** 3. A round is one orchestrator write to Notion (update page content, add a comment, change the status). Notion documents typically need a human to advance to `approved`; the twin's role is preparation only.

---

### Slack canvas

**Home:** Slack

**States:**

| State | Terminal? |
|---|---|
| `live` (editable) | No |
| `locked` (Slack native lock) | Yes (human decision, no further edits expected) |
| `linked_to_channel` | No (intermediate milestone, not terminal) |
| `archived` (channel archived) | Yes |

**Terminal condition:** canvas is locked or its parent channel is archived.

**Detection:** Slack Events API — `canvas_deleted`, `canvas_archived`, and `message_canvas_updated` events where the update includes a lock state change. Subscribe using the existing Slack ingestion worker event subscription. The `canvas_updated` event fires on every save, so the worker filters to lock-state changes only before writing to `slack.events`.

**`max_rounds`:** 4. A round is one orchestrator write to a canvas (append a section, update a block).

---

### Google Drive file

**Home:** Google Drive

**States:**

| State | Terminal? |
|---|---|
| `active` | No |
| `trashed` | Yes |
| `shared_externally` | No (milestone, used as signal) |

There is no formal approval state in Drive. The orchestrator treats a file as **done** when it is shared with a client email domain (detected from file permissions) or when it is trashed.

**Detection:** Google Drive push notifications (Drive API watch channels). The ingestion worker registers a watch channel per file when the twin creates or claims a Drive artifact. Watch channels expire after 7 days and must be renewed; the worker tracks expiry and re-registers automatically. Fallback: poll every 30 minutes via `files.get` if the watch channel reports an error.

**`max_rounds`:** 3. Drive files are usually campaign deliverables. The twin creates the file; a human reviews and shares it. Orchestrator interventions after creation are rare.

---

### Media job

**Home:** Content Lab (internal job queue)

**States:**

| State | Terminal? |
|---|---|
| `queued` | No |
| `rendering` | No |
| `done` | Yes |
| `failed` | Yes (but may trigger a retry round) |
| `distributed` | Yes |

**Terminal condition:** `done`, `distributed`, or `failed` after max retries exhausted.

**Detection:** Polling only — Content Lab has no webhook surface. The orchestrator polls the Content Lab job API every 60 seconds for jobs it owns. Results are written to `content.jobs` in the event log.

**`max_rounds`:** 2. A round is one orchestrator-initiated retry on a failed job. If two retries fail, escalate rather than loop.

---

## `max_rounds` cap

`max_rounds` is a **per-artifact-instance integer counter** stored in `agent.events` (the feedback log), keyed by `(artifact_type, artifact_id, session_id)`. The counter increments each time the orchestrator takes an action on the artifact (a "round"). It does **not** count passive observation events (receiving a webhook, logging a state change).

### Enforcement

```
on orchestrator action attempt:
  rounds = count(agent.events where artifact_id = X and action_type = 'intervention')
  if rounds >= max_rounds[artifact_type]:
    escalate(artifact)
    halt()
  else:
    perform action
    log to agent.events (increments counter)
```

The counter is **not reset** when a state change occurs. It is a lifetime counter per `(artifact_id, session_id)`. A new session (new twin invocation for the same artifact) starts a fresh counter.

---

## Escalation path

An artifact escalates when:
1. The `max_rounds` cap is reached without hitting a terminal state, **or**
2. A GitHub issue is labeled `blocked` and has been open for > 48 hours with no orchestrator-initiated state change, **or**
3. A Notion page has been `in_review` for > 72 hours with no state change, **or**
4. A media job fails twice.

### Escalation procedure

1. Log a row to `agent.events` with `action_type = 'escalation'` and `outcome = 'stalled'`.
2. Post a message to `#claude-ops` (Slack) with:
   - Artifact type and ID/URL
   - Current state
   - Number of rounds taken
   - Reason for escalation (cap hit, timeout, repeated failure)
   - Last action taken and its outcome
3. Set a flag in `agent.events` (`escalated = true`) so the orchestrator skips this artifact on future sweeps.
4. **Halt all further orchestrator actions on this artifact** until a human clears the escalation by removing the `blocked` label (GitHub), advancing the Notion status, or posting a specific Slack emoji reaction (`:white_check_mark:`) on the escalation message.

### Clearing an escalation

The orchestrator watches for:
- GitHub: `unlabeled` event removing the `blocked` label → resume watching
- Notion: status advance beyond `in_review` → resume watching
- Slack: `:white_check_mark:` reaction on the escalation message from any human user → resume watching, reset round counter

---

## Summary table

| Type | Home | Terminal states | Detection | `max_rounds` |
|---|---|---|---|---|
| Pull request | GitHub | merged, closed | Webhook (primary) | 6 |
| Issue | GitHub | closed | Webhook (primary) | 4 |
| Notion document | Notion | approved, archived | Webhook + 10 min poll | 3 |
| Slack canvas | Slack | locked, archived | Events API subscription | 4 |
| Google Drive file | Drive | trashed, shared externally | Push notification + 30 min poll | 3 |
| Media job | Content Lab | done, distributed, failed×2 | 60 s poll | 2 |
