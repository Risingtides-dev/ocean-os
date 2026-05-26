# Per-twin GitHub App setup

This guide walks an operator through creating a GitHub App for their twin, installing it on the `RisingTides-dev` org, and wiring up the credentials so the twin can comment on issues/PRs and receive webhook events under its own bot identity.

**Template reference:** `smaths-bot` is the first App to go through this process. When you see `smaths-bot`, substitute your own twin's name.

---

## 1. Create the App

1. Go to **github.com/settings/apps/new** (personal account) _or_ **github.com/organizations/RisingTides-dev/settings/apps/new** if you want the App owned by the org directly. Either works; org-owned is cleaner for a shared squad context.

2. Fill in the basics:

   | Field | Value |
   |---|---|
   | **GitHub App name** | `smaths-bot` (must be unique across GitHub) |
   | **Homepage URL** | `https://github.com/Risingtides-dev/ocean-os` |
   | **Webhook URL** | Your ingestion worker's public URL, e.g. `https://github-ingestion.yourdomain.railway.app/webhook` |
   | **Webhook secret** | A random string — generate with `openssl rand -hex 32`. Save it; you'll need it in step 4. |

3. Under **Webhook**, check **Active**.

---

## 2. Set permissions

Under **Repository permissions**, set:

| Permission | Level |
|---|---|
| **Issues** | Read & write |
| **Pull requests** | Read & write |
| **Contents** | Read & write |
| **Metadata** | Read-only (required by GitHub, always on) |

Leave everything else at **No access** unless a future feature requires it.

---

## 3. Subscribe to webhook events

Under **Subscribe to events**, check:

- `Issues` → covers `issues.assigned` and other issue mutations
- `Issue comments` → covers `issue_comment.created`
- `Pull requests` → covers `pull_request.opened` and related events
- `Pull request review requests` → covers `pull_request.review_requested`

> **Why these four?** They map to the core actions a twin needs: reacting when assigned to an issue, responding to review requests, processing new PRs, and seeing PR comments.

---

## 4. Generate a private key

After saving the App, scroll to the bottom of the App settings page and click **Generate a private key**. GitHub downloads a `.pem` file. Store it securely — this is the credential the twin uses to authenticate as the App.

---

## 5. Install the App on the org

1. In your App's settings page, click **Install App** in the left sidebar.
2. Click **Install** next to `RisingTides-dev`.
3. Choose **All repositories** (or select specific repos). For Ocean-OS purposes, at minimum include `RisingTides-dev/ocean-os`.
4. Click **Install**. Note the **Installation ID** from the resulting URL — you'll need it.

> The Installation ID appears in the URL after install: `github.com/settings/installations/<INSTALLATION_ID>`.

---

## 6. Store credentials in the bridge env

Add these variables to your operator's bridge environment (Railway service, `.env` file, or wherever your bridge reads env vars from):

```
GITHUB_APP_ID=<App ID from the App's settings page>
GITHUB_APP_PRIVATE_KEY=<contents of the .pem file, with literal \n between lines, or base64-encoded>
GITHUB_APP_INSTALLATION_ID=<Installation ID from step 5>
GITHUB_WEBHOOK_SECRET=<the random string you set in step 1>
```

> **Private key formatting tip:** If your platform supports multi-line env vars, paste the `.pem` contents directly. If not, base64-encode it: `base64 -w0 smaths-bot.pem` and decode it in your startup code before passing it to the GitHub SDK.

The ingestion worker (`ingestion/github/`) reads `GITHUB_WEBHOOK_SECRET` to validate incoming payloads. The bridge itself reads the App ID, private key, and installation ID to generate short-lived tokens for outbound API calls (commenting, requesting reviews).

---

## 7. Verify webhook signatures

Every payload GitHub sends includes an `X-Hub-Signature-256` header. The ingestion worker already validates this — see `ingestion/github/src/index.ts` — but here's how it works in case you're wiring a new consumer:

```typescript
import crypto from "node:crypto";

function verifySignature(rawBody: string, sigHeader: string, secret: string): boolean {
  if (!secret) return false;
  const hmac = crypto.createHmac("sha256", secret);
  hmac.update(rawBody);
  const expected = `sha256=${hmac.digest("hex")}`;
  // Buffers must be equal length before timingSafeEqual
  if (expected.length !== sigHeader.length) return false;
  return crypto.timingSafeEqual(Buffer.from(expected), Buffer.from(sigHeader));
}
```

Key points:
- Always use **timing-safe comparison** (`crypto.timingSafeEqual`). Never use `===` — it leaks timing information.
- Compute the HMAC over the **raw bytes** of the request body before any JSON parsing. Parse after you've verified.
- If the secret is empty or missing, **reject the request** — don't accept unsigned payloads.

---

## 8. Smoke-test the setup

1. Open a test issue on `RisingTides-dev/ocean-os`.
2. Watch the ingestion worker logs — you should see a `202` response and a row in `github.events`.
3. Have the twin post a comment on the issue using the App installation token. Verify the comment appears under the bot's identity (e.g. `smaths-bot[bot]`).

If the webhook delivers but you see a `401` in the worker logs, the `GITHUB_WEBHOOK_SECRET` doesn't match what you set in App settings — regenerate it on both sides.

---

## 9. Rotating the webhook secret

If the secret leaks, rotate it without downtime:

1. Generate a new secret: `openssl rand -hex 32`.
2. Update `GITHUB_WEBHOOK_SECRET` in your bridge env and redeploy.
3. Update the **Webhook secret** field in the App settings on GitHub.
4. GitHub will use the new secret for all deliveries after the save. There's a brief window where in-flight deliveries may use the old secret — the worker's idempotency key (`X-GitHub-Delivery`) means re-deliveries are safe to accept after the rotation.

---

## Reference — smaths-bot

Once `smaths-bot` is live, its App ID and installation ID will be published here as a reference. Other operators can compare their setup against it to verify they've got the right shape.

| Field | Value |
|---|---|
| App name | `smaths-bot` |
| App ID | _TBD — fill in after creation_ |
| Installation ID | _TBD — fill in after install_ |
| Webhook URL | _TBD — fill in after deploy_ |
