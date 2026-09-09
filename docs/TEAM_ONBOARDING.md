# Ocean team onboarding (macOS arm64)

Status: current runbook, written 2026-09-09 against `origin/main` `775b1067`.
Machine side: [`../ops/onboard-teammate.sh`](../ops/onboard-teammate.sh).
Operator side of federation: [`OPERATIONS.md`](OPERATIONS.md) "Rooms and
federation". Package facts: [`../packaging/npm/README.md`](../packaging/npm/README.md).
Budget: about fifteen minutes once the operator prerequisites are in place,
and every step is a command you can paste into a terminal, Claude Code, or
Codex — nothing needs the operator on the call.

## What you get

A room is a durable transcript, roster, and set of contributed folders that
spans daemons: you read and post in `campaigns` from your own daemon on your
own Mac, Bedrock carries the ordered stream between nodes, and the same
messages reach the operator, the other members, and the agents. Agents live
in rooms: a room-authorized agent such as `room-builder` wakes when someone
writes `@room-builder …`, runs on the node that owns it, under that node's
permission policy, and answers in the room. You reach all of that from the
tools you already use: `ocean-mcp` exposes your daemon to Claude Code, Codex,
and Cursor as an MCP server — `ocean_rooms`, `ocean_room_read`,
`ocean_room_post`, `ocean_room_join`, `ocean_room_inspect`,
`ocean_room_resources`, `ocean_agents`, `ocean_sessions`, `ocean_health`, and
`ocean_prompt` to run one Ocean turn in the project you are sitting in. The
daemon stays the authority for permissions and tools; nothing about you
crosses the MCP wire but tool arguments. Folders you contribute to a room stay
on your machine: a grant is a local, path-confined, read-only view that an
admitted agent reads through `room_list` / `room_read` with one audit row per
call. Nothing is mirrored anywhere.

## Prerequisites the operator does for you

Ask John for these before you start; nothing below works without them.

- A Tailscale invite to the `tail168656.ts.net` tailnet. Your Mac shows up
  as a `100.x.y.z` node once you accept it.
- A GitHub invite to the `KINGMAKER-SYSTEMS` organization.
- Membership on the Rising Tides Cloudflare account (used for `wrangler`
  deploys; not needed for rooms, but it arrives with the rest).
- A Bedrock room invite link. The operator mints it from their daemon
  (`POST /v1/rooms/persistent/<key>/invites`); the 201 body carries the
  32-character `code` and an `onboard_url` of the form
  `https://ocean-bedrock-production.up.railway.app/api/v1/invites/<code>/onboard`.
  The link embeds the code, so it is the credential: single-use, expiring,
  never pasted into a ticket or a screenshot.
- A published `@risingtides-dev/ocean` release. As of 2026-09-09 there is
  none: `origin` has no `v*` tag, `gh release list` is empty, and
  `.github/workflows/release.yml` on `main` does not stage the `ocean-mcp`
  binary that `packaging/npm/package.json` and `test-package.sh` require.
  Until the workflow stages `ocean-mcp` and a `v0.1.0` tag is pushed from
  `main` (`../packaging/AGENTS.md`), step (c) below fails with a 404 from
  GitHub Packages, and the script stops there.
- The model alias to start with (`--model` below) and which provider to sign
  in to (Claude or Codex). `curl -s 127.0.0.1:4780/v1/models` lists the
  registry once your daemon is up.

## Steps you run

(a) Tailscale. Install the Tailscale app, sign in with the invited account,
and confirm your machine is on the tailnet:

```bash
tailscale up          # or sign in through the menu-bar app
tailscale status      # your Mac is the row with your login
```

(b) GitHub. Log in, accept the organization invite, then add the scope that
GitHub Packages needs:

```bash
gh auth login                       # browser flow
# accept the KINGMAKER-SYSTEMS invite at https://github.com/orgs/KINGMAKER-SYSTEMS
gh auth refresh -s read:packages
```

(c) Toolchain and the package. bun installs global binaries into
`~/.bun/bin`; make sure that directory is on your `PATH`.

```bash
brew install oven-sh/bun/bun gh
printf '%s\n' \
  "@risingtides-dev:registry=https://npm.pkg.github.com" \
  "//npm.pkg.github.com/:_authToken=$(gh auth token)" >> ~/.npmrc
bun add -g @risingtides-dev/ocean
```

This puts `ocean` (TUI), `ocean-daemon`, `ocean-mcp`, and `ocean-update` on
`PATH`. The script in (d) does all of (c) for you, without ever duplicating
the two `~/.npmrc` lines, so you may skip straight to it.

(d) The machine setup script. It needs a checkout only for the launcher it
copies out of `deploy/`:

```bash
git clone https://github.com/Risingtides-dev/ocean-os
cd ocean-os
ops/onboard-teammate.sh --model <alias-from-the-operator>
```

Idempotent; re-run it whenever you like. In order it checks macOS arm64,
bun, gh, and the `read:packages` scope (it never starts a login for you; it
prints the command and stops); writes the `~/.npmrc` lines; installs the
package; writes `~/.config/ocean-rs/federation.env` (see "Federation facts");
copies the package `ocean-daemon` to `~/.local/libexec/ocean-daemon/current`
and installs the `dev.risingtides.ocean-daemon` LaunchAgent, which runs the
repo launcher from `$HOME` with `OCEAN_YOLO=1` and your `OCEAN_MODEL`, then
waits for `/health`; runs `ocean-mcp setup`; registers the MCP server in
Claude Code when `claude` is on `PATH`; and prints the checklist for (e)–(h).
`--dry-run` prints every mutating step instead of doing it. The manual
equivalents, should you prefer them: [`../ops/README.md`](../ops/README.md)
for the LaunchAgent shape, `ocean-mcp setup` for the MCP lines, and the
federation file described below.

(e) Sign in to a model provider. Run the TUI and use its login flow — the
daemon holds the credential, so this is once per machine, not per terminal:

```bash
ocean
/login              # opens the provider picker; Enter on Claude or Codex starts the browser flow
/model              # pick from the live registry; the pick persists across restarts
```

(f) Wire Ocean into Claude Code and Codex. `ocean-mcp setup` installs the
`ocean` skill into `~/.claude/skills`, `~/.codex/skills`, and
`~/.agents/skills` (only where the tool's directory already exists) and
prints the exact config lines; the two you need:

```bash
ocean-mcp setup
claude mcp add --scope user ocean -- "$(command -v ocean-mcp)" serve
```

```toml
# ~/.codex/config.toml
[mcp_servers.ocean]
command = "/Users/<you>/.bun/bin/ocean-mcp"
args = ["serve"]
```

You post as `OCEAN_MEMBER_ID` (default `$USER`); set it in your shell profile
if your room identity differs. `ocean-mcp doctor` prints which id you are.

(g) Join the team room. Redeem the invite exactly once, with your daemon:

```bash
curl -s -X POST http://127.0.0.1:4780/v1/rooms/persistent/invites/redeem \
  -H 'content-type: application/json' -d '{"code":"<code>"}'
```

A good answer is the room's access projection with `"room_key"` on it;
`state` is `connecting` or `live`. The daemon generates a local bearer,
persists the pending redemption first, and exchanges it with Bedrock, so a
lost response is safe to retry and a restart finishes it. The `onboard_url`
page carries a JSON manifest (`curl -s <onboard_url>`) with the invite's
name, role, scopes, and expiry, plus Bedrock's one-command bootstrap
(`npm run ocean:bootstrap -- --invite <onboard_url>`). That command is the
Bedrock knowledge-layer path — a push-only folder mirror from a Bedrock
checkout — and it consumes the same single-use code. For rooms, redeem with
the daemon as above and leave the bootstrap alone unless the operator asked
for both, in which case ask for two invites.

(h) Verify.

```bash
ocean-mcp doctor
# daemon: ok at http://127.0.0.1:4780 (backend …, rev …) / member id: … / rooms: 1
curl -fsS "http://127.0.0.1:4780/v1/rooms/persistent/<room_key>/snapshot?before_seq=18446744073709551615&limit=1"
# access.state: "live"
```

Then, in Claude Code, ask it to read the room — "read the latest messages in
the campaigns room" — and confirm the `ocean_room_read` tool answers with the
transcript. Post a hello with `ocean_room_post`; it answers 202 and lands in
the transcript once Bedrock's ordered stream confirms it.

## Federation facts

Your daemon needs one thing to reach Bedrock: the origin, in
`~/.config/ocean-rs/federation.env` (`OCEAN_CONFIG_DIR` overrides the
directory). The file must be a regular file owned by you at mode `0600`, in a
directory that is yours and not group- or world-writable (the script makes it
`0700`); anything else and the launcher refuses the whole file, logs a fixed
reason code, and starts the daemon with federation off. The launcher
(`deploy/ocean-daemon.sh`, installed as `~/.local/libexec/ocean-daemon/launch.sh`)
reads the file immediately before it execs the daemon on every start and
never publishes the values anywhere else; the daemon reads the same file
natively when it is started by hand.

The owner token is not needed to redeem an invite or to stay in a room.
`OCEAN_FEDERATION_OWNER_TOKEN` is used for exactly one thing — bootstrapping a
Local room as its Bedrock owner and minting invites — and that is the
operator's job, done with [`../ops/set-ocean-federation.sh`](../ops/set-ocean-federation.sh)
on the operator's machine. Do not ask for it and do not paste one in.

Your daemon is a **member node**: `federation.env` holds the origin and
nothing else, and the launcher logs `federation=on (file, member)`. Only the
room owner's daemon carries a bearer (written there by
`ops/set-ocean-federation.sh`); redeeming an invite never uses one, and an
owner-only route on your node answers `federation_unavailable`, which is
correct. The script marks the file it wrote with a comment line so a re-run
recognises it, and it leaves a file that carries a real credential alone
unless you pass `--force`.

What redemption leaves behind: the room credential the daemon minted lives in
owner-only `rooms.db` beside the config dir, never in `federation.env`, and it
is what keeps you in the room across restarts. Transport is what
`OCEAN_FEDERATION_URL` enables; a missing or invalid origin moves your
credentialed rooms to `recovering` (not out of the room) until it is fixed.

## Team status

Facts as of 2026-09-09; `?` means not checked, not a guess.

| Person | Tailscale | GitHub org | Ocean daemon | ocean-mcp | Bedrock room |
| --- | --- | --- | --- | --- | --- |
| Eric (`ecfromthedc`) | yes — `erics-machine` 100.119.217.76 | yes | yes (older build) | no | no |
| Jake (`jakebalik-bit`) | yes — `jakes-macbook-air` | yes | no | ? | no |
| Jay (`jayvespertine`) | yes — `jays-macbook-air` | yes | no | ? | no |
| Johnny (`johnnybalikmusic`) | yes — `johnnys-mac-mini` | **no** (not a member yet) | no | ? | no |

Update the row when a step lands; this table is the only place the team's
state is written down.

## Troubleshooting

- Health is `GET /health`, not `/v1/health`. A 404 on a path is not "down";
  `curl -fsS http://127.0.0.1:4780/health` is the truth, and its `rev` is the
  build you are running. `launchctl print gui/$(id -u)/dev.risingtides.ocean-daemon`
  shows launchd's view; `tail -f /private/tmp/ocean-daemon.log` the daemon's.
- The launcher writes one line per start:
  `==> ocean-daemon: cwd=… (neutral) bin=… yolo=1 bind=127.0.0.1:4780 federation=on (file)`.
  `federation=off` with `private configuration refused: <reason>` above it
  names the custody failure — `unsafe_mode` (not `0600`), `unsafe_parent`
  (directory writable by others or not yours), `not_regular`, `foreign_owner`,
  `unsupported_entry` (a line that is not one of the three keys),
  `duplicate_entry`, `origin is invalid` (anything after the host, or plain
  `http` to a remote host), `credential is invalid` (no token line at all; see
  "Federation facts"). Fix the file, then
  `launchctl kickstart -k gui/$(id -u)/dev.risingtides.ocean-daemon`.
- Access states, from the room snapshot's `access.state`: `connecting` is the
  first lease after a redeem or restart; `live` is healthy; `recovering` means
  the daemon holds a credential but cannot currently hold the Bedrock stream —
  bad origin, Bedrock down, or a network that cannot reach it — and it clears
  by itself once the cause does; `revoked` is terminal and needs a new invite;
  `local` is an unfederated room. During `recovering` you still see the
  transcript you already have and your posts wait in the outbox.
- The daemon refuses to start inside a git repository:
  `refusing to start: daemon cwd … is inside a git repo`. The LaunchAgent runs
  it from `$HOME`; if you start one by hand, `cd ~` first. Never set
  `OCEAN_ALLOW_REPO_CWD=1`. Two daemons on `:4780` is the other classic —
  `lsof -nP -iTCP:4780 -sTCP:LISTEN` should show exactly one.
- Redeem answers: 503 `federation_unavailable` means your daemon has no valid
  federation configuration (see the launcher line); 403 `invite_forbidden`
  means the code is used, expired, or mistyped — ask for a fresh link;
  502 `federation_protocol` means Bedrock answered something the daemon
  could not accept — tell the operator.
- `no model selected — set OCEAN_MODEL or pick one via POST /v1/model`: the
  daemon never defaults to a model. The script pinned `OCEAN_MODEL` in the
  plist; `/model` in the TUI persists a new pick.
- `bun add -g @risingtides-dev/ocean` answers 401: the `~/.npmrc` token is
  stale or lacks `read:packages` — `gh auth refresh -s read:packages` and
  re-run the script. 404: no release has been published yet (see
  "Prerequisites").
- `claude mcp add` says `ocean` already exists: fine, it is registered;
  `claude mcp get ocean` shows it.
- Updating: `ocean-update` refreshes the package but never touches the
  supervised daemon (that copy is immutable by design); re-run
  `ops/onboard-teammate.sh --model …` to publish the new build and restart it.
