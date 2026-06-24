# Ocean Coworker Onboarding

## Purpose

Get a new coworker — or their agent — from zero to **connected to Ocean** on their own
machine. "Connected" means: they hold a scoped **ocean-bedrock** API token, they can
reach the shared knowledge layer (`/docs`, `/context`, `/handoffs`, `/sessions`,
`/shared`), and their agent knows how to read and write it.

This is the download-and-run onboarding path. A coworker drops this skill into their
agent (Claude Code, Codex, etc.) and says "onboard me to Ocean."

## What Ocean is, in one paragraph

Ocean is an agent knowledge layer. The shared substrate is **ocean-bedrock**: a
token-authenticated shared filesystem with a declared HTTP API (`/api/v1/*`,
`Authorization: Bearer <token>`). Agents and coworkers exchange docs, project memory,
session artifacts, and handoff notes through it. You do not need to clone or build
ocean-os to use Ocean — you need a token and the bedrock URL.

## Onboarding flow

Run these steps in order. Stop and report if any step fails — never fabricate a token
or pretend a connection works.

### 1. Find the bedrock URL

Ask the operator for the **ocean-bedrock base URL** (e.g. `http://localhost:8080` for a
local box, or the deployed URL). If they are running it themselves locally:

```bash
# In the ocean-bedrock repo
npm start            # serves on PORT or OCEAN_BEDROCK_PORT, default 8080
```

The base URL is whatever the server prints: `[ocean-bedrock] <instance> listening on http://<host>:<port>`.

### 2. Get a token

A token is issued by someone who already holds an `admin` token (the operator), using
the bedrock repo:

```bash
# admin runs this and hands the printed token to the coworker
npm run token:create -- --name "<coworker-name>" --role agent --scope /
```

Roles: `readonly`, `contributor`, `agent`/`readwrite`, `admin`. Default scope `/` (whole
tree); narrow it with repeatable `--scope /docs --scope /context` for a limited coworker.
The command prints a JSON record containing the token string. **The token is shown once —
save it.**

If the coworker is bootstrapping a brand-new local box for dev, the operator can instead
set `OCEAN_BEDROCK_BOOTSTRAP_TOKEN=dev-token-change-me npm start` and use that token.
**Footgun:** bootstrap only seeds the token when the auth file is *empty* — the code is
`if (auth.tokens.length === 0 && bootstrapToken)`. If `data/.ocean-bedrock/tokens.json`
already has any token, the bootstrap env var is silently ignored and you'll get
`401 Invalid token`. To force a clean bootstrap, point the server at a fresh auth file
(`OCEAN_BEDROCK_AUTH_FILE=/tmp/fresh-tokens.json`) or delete the existing one. Do not use
a bootstrap token against a shared/production box.

### 3. Store the token

Put the token where the agent will read it — an env var is simplest:

```bash
export OCEAN_BEDROCK_URL="<base-url>"
export OCEAN_BEDROCK_TOKEN="<token>"
```

For persistence, add those two lines to the coworker's shell profile (`~/.zshrc` /
`~/.bashrc`). Never commit the token to a repo or paste it into a shared channel.

### 4. Verify the connection

This is the gate. The agent MUST confirm a real 200 before claiming onboarded:

```bash
curl -s -H "Authorization: Bearer $OCEAN_BEDROCK_TOKEN" \
  "$OCEAN_BEDROCK_URL/api/v1/info" | head -c 800
```

A healthy response is JSON with `instance`, `apiVersion: "v1"`, `defaultFolders`, and a
`principal` block showing the token's name/role/scopes. If you get `401 Missing token`
the token or header is wrong; if you get a connection error the URL/port is wrong or the
server is down. Fix before continuing.

### 5. Smoke-test read + write

Confirm the coworker can actually use the layer:

```bash
# list the shared tree
curl -s -H "Authorization: Bearer $OCEAN_BEDROCK_TOKEN" \
  "$OCEAN_BEDROCK_URL/api/v1/list?path=/&depth=1"

# write a hello note (proves write scope; skip if token is readonly)
curl -s -X PUT -H "Authorization: Bearer $OCEAN_BEDROCK_TOKEN" \
  --data-binary "onboarded $(whoami)" \
  "$OCEAN_BEDROCK_URL/api/v1/file?path=/handoffs/onboard-$(whoami).txt"
```

### 6. Wire the agent

Tell the coworker's agent these durable facts so it uses Ocean going forward:

- Shared knowledge lives at `$OCEAN_BEDROCK_URL/api/v1/*`, auth via
  `Authorization: Bearer $OCEAN_BEDROCK_TOKEN`.
- Read project memory and docs from `/context` and `/docs` before starting work.
- Write handoffs to `/handoffs`, session artifacts to `/sessions`.
- Full route list: `GET /api/v1/openapi.yaml` (no auth needed) and `GET /api/v1/info`.
- Semantic search: `GET /api/v1/semantic/search?q=<term>&path=/context`.

## Done means

The coworker has `OCEAN_BEDROCK_URL` + `OCEAN_BEDROCK_TOKEN` set, `/api/v1/info` returns
200 with their principal, and a read+write smoke test passed. Report the principal's
name/role/scopes back so the operator can confirm the right grant.

## Rails

- Never invent or guess a token. Tokens come only from `token:create` run by an admin.
- Never commit a token or post it in a shared channel. Env var or secret store only.
- A `readonly` token failing the write smoke test in step 5 is expected, not an error.
- If the operator hasn't deployed bedrock anywhere yet, onboarding is local-only —
  say so plainly rather than pointing at a URL that doesn't exist.
