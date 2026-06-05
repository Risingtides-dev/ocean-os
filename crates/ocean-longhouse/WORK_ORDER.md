# Work Order — Longhouse Quorum Engine (tuning + finish)

_For the next coder. Written 2026-06-01. Read this whole file before touching anything._

## TL;DR
The longhouse is **real and working** end-to-end: a Rust `QuorumEngine` + real multi-model
LLM agents (DeepSeek-flash + Kimi) that actually deliberate, emitting `LonghouseEvent`s that a
Game Boy–style browser deck renders live. **Bones are solid; it needs refinement.** Your job is
to **tune the engine** and finish the stubbed governance bits — and the smart move is to **use the
UI deck as your debugging lens** (watching the council surfaces engine bugs far faster than reading
event logs; a quorum engine is emergent + timing-dependent, so SEE it).

## Where everything lives (READ — it's split across branches/worktrees, not main)
| Piece | Repo | Branch | Path |
|---|---|---|---|
| **Quorum engine (TUNE THIS)** | `~/dev/ocean-os` | `feat/longhouse-engine` (worktree `.claude/worktrees/longhouse-engine`) | `crates/ocean-longhouse/src/{quorum,convene,agent}.rs` |
| Event vocab (the wire contract) | `~/dev/ocean-os` | same | `crates/ocean-agent-sdk/src/lib.rs` (search `LonghouseEvent`) |
| Daemon endpoints | `~/dev/ocean-os` | same | `crates/ocean-daemon/src/main.rs` (`/v1/longhouse/convene` = REAL, `/demo` = scripted) |
| **Game Boy deck (debug lens)** | `~/dev/ocean-surface` | `worktree-agent-a4b837151c678ac0f` | `crates/ocean-surface-proxy/static/longhouse.html` (single file, Phaser, zero-build) |

**Nothing is merged to main yet.** The engine branch (`feat/longhouse-engine`) also carries the
foundation types (they were uncommitted when it branched). Decide merge strategy before shipping.

## How to run it (one daemon, one proxy, no port whack-a-mole)
```
# 1. daemon WITH the real engine (from the engine worktree):
cd ~/dev/ocean-os/.claude/worktrees/longhouse-engine
set -a; source ~/.config/ocean-rs/tools.env; set +a
cargo run -p ocean-daemon            # binds :4780, exposes /v1/longhouse/convene

# 2. surface proxy serving the deck (from ocean-surface):
cd ~/dev/ocean-surface
OCEAN_SURFACE_AUTH=off OCEAN_SURFACE_BIND=127.0.0.1:8790 \
  OCEAN_SURFACE_DIST=<repo>/dist OCEAN_DAEMON_URL=http://127.0.0.1:4780 \
  ./target/debug/ocean-surface-proxy
# (copy longhouse.html into dist/ first, or point DIST at .../static)

# 3. open the deck, fire a REAL council:
open http://127.0.0.1:8790/longhouse.html
curl -X POST http://127.0.0.1:4780/v1/longhouse/convene \
  -H 'content-type: application/json' \
  -d '{"question":"What 3 TikTok hooks to test for an indie pop launch?","federation":"content"}'
```
Cheap models in use: `deepseek-v4-flash` + `kimi-k2.6` (keys in `~/.config/ocean-rs/auth.json`).
NOTE: kimi rejects any non-default `temperature` (HTTP 400) — already handled (temp stripped). Don't re-add it.

## What's REAL today
- `QuorumEngine` (`quorum.rs`): credential-weighted endorse−inhibit tallies, time-decay
  (`weight * 2^(-Δt/ttl)`, clock injected — deterministic), configurable threshold (net-cutoff OR
  sigmoid), margin-gated convergence, seeded tie-break. **11 unit tests pass** (`cargo test -p ocean-longhouse`).
- `convene.rs`: staffs a mixed deepseek+kimi council, 2 rounds (propose → endorse/inhibit),
  re-tallies after every mark, emits the existing events, grants firekeeper, emits Converged/Aborted.
- A model that errors/times out just doesn't contribute — council never crashes.

## THE WORK (tuning + finish), priority order

### P1 — Tune the engine BY WATCHING (use the deck as the debugger)
The engine *terminates correctly* but whether it converges on *good* answers is untuned. Run real
councils, watch the deck, and tune:
- **Threshold + sigmoid sharpness** (`quorum.rs`): too low → converges on noise; too high → deadlocks.
  Watch the quorum bar behavior on the deck. Make it a per-domain/per-topic config, not a const.
- **Decay TTL**: too fast → no stable lead forms; too slow → stale signals dominate. Tune by watching
  whether a lead "sticks" or thrashes.
- **Cross-inhibition weight**: should keep two near-equal rivals from both crossing. Verify visually
  (two proposals, neither should win until one genuinely pulls ahead).
- Add a **debug/inspect mode**: emit the raw tally numbers so the deck can show them (right now the
  deck shows a bar but not the underlying net-weights — wire that for debugging).

### P2 — Open-ended deliberation (currently HARDCODED to 2 rounds)
`convene.rs` runs exactly Round 1 (propose) + Round 2 (vote). The design calls for an open-ended
re-assertion loop that runs until quorum or deadline (with the decay + give-up dynamics doing the
work). Replace the fixed 2 rounds with a loop bounded by `max_rounds` + token budget + deadline.

### P3 — Finish the governance layer (all STUBBED — see design doc §)
Per `docs/LONGHOUSE_ORCHESTRATION.md`, these were scoped out of v1 and need building:
- **Escrow trio**: `TitleRegistry` + `Revoker` as separate daemon principals (firekeeper currently
  bound but NOT a separately-escrowed capability). Grant/exercise/revoke split.
- **`claim_outcome` gate**: firekeeper should only be able to RATIFY what the daemon's quorum state
  already says — the daemon must REJECT a Converged claim unless its own state agrees. (This is the
  "unforgeable" property — currently the firekeeper just emits it.)
- **Graduated recall + `Warned` strikes**, **validator process-veto**, **subsidiarity escalation
  predicate** (most things shouldn't convene a council at all).
- **`GET /v1/longhouse/topics[/{id}]`** read endpoint (stubbed/missing) — needed so the deck can
  REBUILD state on refresh (right now a refresh = blank; the deck holds state only in browser RAM).

### P4 — Sybil hardening (design §6, not built)
Credential-weighted quorum (so a chatty/forked agent can't fake consensus), `spawn_worker` splitting
parent quorum weight across children, self-renewal block. Important if agents ever spawn sub-agents.

## UI-side refinements (separate repo `ocean-surface`, coordinate)
The deck renders but needs polish to be a real debug + watch surface:
- **Legible text** — the custom 3×5 pixel font + the DMG dither shader shred the glyphs. Either
  exempt the text/UI layer from the quantize shader, or use a real GB bitmap font at 2× with backing
  plates + high contrast (brightest palette color on darkest). RIGHT NOW YOU CANNOT READ PROPOSALS.
- **Auto-dive into the active room** on convene (currently stays on the house exterior sometimes).
- **Surface the real content as readable cards** — proposal text, who endorsed/inhibited, the live
  tally, the winner — so WATCHING tells the whole story (this is what makes it a debugger).
- **Survive refresh** — needs the P3 `GET /v1/longhouse/topics` endpoint + a state-rebuild on load.
- Palette is now **Ocean blue** (not GB green): `#06111d → #0a3a5e → #1d9bf0 → #7fe7c8` (matches
  the surface brand). Don't revert to green.

## North-star (where this is going — context, not your task)
The longhouse becomes a **renderable component**: an agent `component_render(kind:"longhouse",
props:{topic_id})` mounts a live council card IN THE CHAT; minimize → chip; pin → desktop sidebar
widget dock; same widget as an Obsidian ItemView. The deck you're refining is widget #1 of a
pinned-widget system. See `docs/LONGHOUSE_ORCHESTRATION.md` + the vault vision doc.

## Honest state / gotchas
- Engine verified live: real deepseek+kimi council answered a real TikTok-hooks question, engine
  tallied + converged. Not faked.
- `/demo` (scripted) and `/convene` (real) both emit the SAME events — if you see "Warner Q3" /
  "Plan A" text, that's the SCRIPTED demo leaking in (kill any `lh-loop.sh` background loop).
- Deck holds council state in browser memory only → refresh wipes it (fix = P3 topics endpoint).
