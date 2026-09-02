# Ocean Rooms — Definition of Done (production grade)

Lives here because `ROADMAP.md` is where open work is tracked; the Ocean Loop's
LOOP.md mirrors it as the loop's finish line. Disagreements are surfaced, not
silently reconciled.

Scope: Ocean Rooms across ocean-os (daemon), ocean-surface (web, Tauri desktop,
Chrome extension) and ocean-bedrock (data plane, compute, identity).

The exit rule is: a line is done when a member can DO the thing from a shipped
surface, can TELL that it is there, and an executable check pins it. This is an
inventory of unfinished work, so a line that does not yet name an executable
check is not eligible to close; its owning repo must add that check first.
A field with a reader is not done (before_seq had a consumer and no member
could ask for older history). A route with a curl example is not done. A merge
production never received is not done. Each line names its owning repo.
Written 2026-09-01 from three read-only assessments at ocean-bedrock 8753038,
ocean-os 616293e, ocean-surface d58a145.

## 0. Shipped, provably
- 0.1 ocean-bedrock production is at origin/master: `npm run deploy:status`
  exits 0 with the Bedrock service AND the room runtime Worker stamped at
  master. Today: 24 commits behind, Worker unstamped. [bedrock]
- 0.2 Every migration under db/ is applied to production and `npm run db:check`
  proves it: the check must probe 009, 010, 011, 012 (and 013 once merged),
  which it does not today. [bedrock, S]
- 0.3 The triage cron service runs the same master build as the service. [bedrock]
- 0.4 deploy-drift CI is armed (OCEAN_ROOM_RUNTIME_URL repo variable set) and
  green, with no acknowledged gap older than one working day. [bedrock, user]
- 0.5 The operated ocean-daemon runs a build from origin/main that includes the
  2026-09-01 paging fixes (#436 to #439), installed by the documented single
  command, with the revision on /health recorded in events.md. Today: 7bf80cdc
  from 08-31. [os]
- 0.6 Federation is ON in the operated daemon: a supported, untracked,
  owner-only login loader restores `OCEAN_FEDERATION_URL` and
  `OCEAN_FEDERATION_OWNER_TOKEN` into every fresh per-user launchd GUI domain
  before the daemon is bootstrapped. The loader reads the bearer from an
  owner-only (`0600`) file or Keychain item and calls `launchctl setenv`; it
  never places either value—and especially never the owner bearer—in the
  tracked or rendered `deploy/dev.risingtides.ocean-daemon.plist`. Its service
  ordering must make the variables available before the daemon's `RunAtLoad`
  start, not only before one installer run. Do not print or record the launchd
  environment while verifying this criterion. After a fresh login or reboot,
  a real credentialed room must still reach `live` against production Bedrock.
  Today: off, with no supported login loader shipped. [os, user]
- 0.7 The surface web bundle, Tauri build and extension are published from
  origin/main through the promotion guard with the build identity visible in
  the surface. [surface]

## 1. What a member can do (each with its check)
- 1.1 Sign in by email one-time code (Cloudflare Access) and land with their own
  folder and zero rooms. Access configured in production so the login route
  answers 200, not 501. Check: coworker:login-smoke plus one production login
  recorded in HANDOFF. [bedrock, user config]
- 1.2 Enroll as an operator, register a device by key proof, declare a directory
  allocation, from the surface. Check: PR #117 gates plus a surface acceptance
  test. [bedrock #117, surface]
- 1.3 Discover, create (public or invite-only), join, invite by operator id,
  accept, from the rooms rail; legacy admin-only registration is no longer the
  only path. Check: rooms:pg-smoke operator section plus surface acceptance. [bedrock #117, surface]
- 1.4 Create a room that agents can work in: the surface sends workspace_root
  on create and offers a bind control for existing rooms, and a room without one
  says so. Today every agent @mention in a surface-created room fails closed
  with workspace_unavailable. Check: surface acceptance plus daemon test. [surface, M]
- 1.5 Read history: opens at the newest page, can ask for older history and see
  whether more exists, orphaned thread replies are reachable, live-follow never
  yanks a scrolled reader, return-to-latest re-pins. Check: surface acceptance. [surface, M]
- 1.6 See every room: the rail pages past 100 rooms (decode next_cursor and
  has_more) and unread state arrives without an 8-second full-list poll.
  Check: surface test. [surface, M]
- 1.7 Send: pending outbox, SSE confirmation, failed-only retry, and a removed
  member refused everywhere including the event stream. Check: rooms protocol
  gate plus surface acceptance. [os, bedrock; largely green today]
- 1.8 Roster: humans and agents, agent ownership visible with owner presence,
  add and remove, mention autocomplete. Today agent_owners is decoded by
  nothing. Check: surface acceptance. [surface, S]
- 1.9 Times read as local time with correct day separators. Today everything is
  UTC rendered as if local. Check: surface unit test with a fixed zone. [surface, S]
- 1.10 Closed room: audit view, no composer, no tailing, no minting; and one
  consistent daemon answer for "room not open" across get, transcript,
  snapshot, events, summarize, attachments. Check: surface acceptance plus
  daemon test. [surface green; os, S]
- 1.11 Compute in the room from the surface and the CLI: provision, exec, files,
  ports with preview URLs that load over TLS from the open internet, secrets,
  repo bind and clone, build, CI pull with results in the room. Check:
  rooms:compute-smoke, a production probe room in HANDOFF, surface acceptance.
  Today previews fail at the TLS handshake and CI-into-the-room is unbuilt. [bedrock, surface, user spend]
- 1.12 Desktop parity: the Tauri app can run the agent authorization ceremony,
  opens ocean://room/<key> deep links, and notifies on mentions. Today the
  desktop is read-only for the ceremony and has no room deep link. [surface, L]

## 2. What an agent can do
- 2.1 Bound to a room through the authorization ceremony from any host. Check:
  `cargo test -p ocean-daemon room_agent_authority::tests` in ocean-os and
  `cargo test -p ocean-daemon
  local_room_agent_bootstrap_is_authenticated_previewable_and_non_authorizing`
  for route-level authentication/bootstrap wiring, plus
  `cargo test -p ocean-surface-ui --test room_agent_authorization_regressions`
  in ocean-surface; the Surface test must carry an explicit web, Tauri and
  extension host matrix before this line can close. [os, surface, M]
- 2.2 Woken by @mention; the reply lands attributed in the transcript; a
  refusal (workspace_unavailable, room_history_unavailable) is rendered in the
  transcript, not lost in an audit row. Check: end-to-end wake against a real
  agent, recorded. [os, surface co_dispatch, M]
- 2.3 Able to act: PHASE1_SAFE_CAPABILITIES is non-empty under an accepted
  Stage 2 manifest, so a room agent can at least read the bound repo and run
  the room's build. Today the set is empty. Check:
  `cargo test -p ocean-daemon room_agent_authority::tests` must pin the exact
  accepted Stage 2 capability set and an end-to-end read/build admission before
  this line can close. [os, L, architecture decision]
- 2.4 Triggers that are offered fire: on_thread_reply works in federated rooms
  or is not offered there and a stored dead value can be cleared;
  on_component_event and on_schedule are hidden until wired. Check:
  `cargo test -p ocean-daemon room_update_refuses_enabling_thread_reply_once_the_room_federates`,
  `cargo test -p ocean-daemon room_update_accepts_a_federated_room_resending_a_thread_reply_it_already_stores`,
  and, in ocean-surface, `cargo test -p ocean-surface-ui --test
  ci_failure_trigger_control` plus `cargo test -p ocean-surface-ui
  thread_reply_is_dead_in_a_federated_room`. The route tests must prove both
  refusal of a new dead value and clearing/re-sending the stored value; the
  Surface checks must prove the offered rows and their access-state holds.
  [os, surface, S]
- 2.5 Agents drive rooms through MCP and the CLI for every route a human has:
  build, CI, secrets, purge, port close. Today MCP stops at expose_port. Check:
  `node --test test/toolbox-manifest.test.mjs` in ocean-bedrock must pin the
  complete tool inventory, and `npm run rooms:compute-smoke` must execute each
  MCP/CLI verb against its disposable room workspace. [bedrock, M]

## 3. Safe
- 3.1 The daemon's room routes authenticate the caller; identity is not a
  caller-asserted author_id; invite mint and redeem require an operator; CORS
  does not trust every chrome-extension origin. [os, M to L, design ruling]
- 3.2 Token ids are never projected (gate green; every new auth_tokens
  reference claimed the day it is written). [bedrock, green]
- 3.3 Preview URL tokens are salted and rotatable, or a written ruling names who
  may recompute one. [bedrock, ruling]
- 3.4 Port attribution names a member (reads done in #108; table ruling owed). [bedrock, ruling]
- 3.5 cargo deny is green on ocean-os (RUSTSEC-2026-0274 rtrb, yanked spin) and
  the ci.yml deny job is a required check or the reason it is not is written. [os, user ruling]
- 3.6 Room secrets are write-only, room commands never see Bedrock's environment,
  the Access verifier's adversarial suite is green, a new coworker never gets
  root scope. [bedrock, green]
- 3.7 Concurrent exec and flush in one room are serialized or bounded;
  last-write-wins on the durable tree is closed. [bedrock, M]
- 3.8 The audit-history renderer has no raw fallback and author_id is bounded
  on render as well as write; a fifth audit writer turns a test red. [os, S]

## 4. Operated
- 4.1 Room and federation metrics exist: access state by room, outbox depth and
  age, SSE reconnects and lag, redemption failures, admission refusals, store
  lock wait. Today there are none. [os, M]
- 4.2 A watchdog reads deploy:status and both /health cards on a schedule and
  reaches a human (triage Telegram path) on drift or 5xx. [bedrock, S]
- 4.3 Room lifecycle: a route closes a room, transcript retention and attachment
  orphan GC exist, idle workspaces are reaped; rooms.db and the blob tree stop
  growing without bound. [os M, bedrock S]
- 4.4 The store is durable under load: WAL, busy_timeout, synchronous set in
  production; store work off the tokio workers; per-room wake buses. [os, M to L]
- 4.5 docs/OPERATIONS.md has a rooms and federation runbook; a migration
  rehearsal of a real rooms.db with rollback is recorded (manifest gate 4). [os, M]
- 4.6 ledger:check is green in all three repos and the manifest-only question
  has a ruling written into the checker. [all, user ruling]
- 4.7 HANDOFF and DEV-LOG describe production within one working day of any
  deploy; "Deployed master tip" is never stale. [bedrock, surface handoff.md]
- 4.8 The loop that builds this is itself healthy: the build host keeps the
  measured disk budget (a surface target is ~9.4 GiB, an ocean-os one 14 to
  16 GiB, and the classifier refuses reclaim to every agent), so surface work
  is scheduled by priority, not by free space; and "landed" is verified on
  GitHub, never inferred from a wave record, because a wave can die mid-land
  with finished work stranded in a worktree (wave 55). `git log trunk..branch`
  is not evidence either: these repos squash-merge, so a merged branch reads as
  commits ahead forever; `gh pr list --head <branch> --state all` is the only
  honest test, for loop/* and worktree-* alike. [loop, user]

## 5. Contracts held by checks, not prose
- 5.1 openapi parity green, and API.md plus OCEAN-ROOM-COMPUTE.md cover every
  route (repo, build, CI, execs, purge are missing today), checked by the
  parity test. [bedrock, S to M]
- 5.2 OCEAN_ECOSYSTEM_CONTRACT.md lists all 40 daemon room routes and
  ARCHITECTURE.md's count is pinned by a test (11 and 80 today vs 40 and 124). [os, M]
- 5.3 The Bedrock event vocabulary pin is refreshed by CI, not by hand; a new
  action reds the daemon build. [os, S]
- 5.4 OCEAN_ROOMS_PRODUCT.md is corrected to the real wire shapes (access
  states, message body, agent turn path, pagination, timestamps, invites) and
  AGENTS.md stops binding dead .rooms-panel CSS. [surface, S]
- 5.5 The disposable-Postgres gates run in ocean-bedrock CI on a service
  container and are merge conditions. [bedrock, M]
- 5.6 The Worker's request layer (13 routes) has executable tests, not regex
  pins. [bedrock, M to L]
- 5.7 The isolated macOS Tauri rooms acceptance harness (#110) exists and runs
  in CI; ocean-tauri is clippy-gated. [surface, L]
- 5.8 Cross-repo drift checks (ROADMAP item) cover the daemon-to-surface and
  daemon-to-Bedrock room contracts. [os, M]

Exit: every line checked with its check named. Then the loop moves to a
maintenance cadence rather than replenishing polish at the tail.

Rulings only the user can make, collected: 0.4 and 0.6 config; 1.1 Access
config; 1.11 preview TLS spend (ACM pack or a dedicated zone); 2.3 Stage 2
manifest; 3.1 auth model; 3.3, 3.4; 3.5 cargo deny; 4.6 manifest-only ledger.
