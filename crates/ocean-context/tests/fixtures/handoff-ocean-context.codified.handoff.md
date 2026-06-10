+++
session_id = "brainstorm-2026-06-09-ocean-context"
repo = "ocean-os"
branch = "fix/ocean-220-livekit-token-auth"
commit_anchor = "d9a9bc9"
scope_ring = "Repo"
written_at = 1780980000

[velocity_at_write]
v_code = 0.0
v_sem = 0.0

[[claims]]
id = "c1"
text = "ocean-context is a NEW crate to be added at crates/ocean-context in the ocean-os workspace; it does not exist yet."
status = "Asserted"
knowledge_tier = "Individual"
confidence = 1.0

[claims.provenance]
commit_sha = "d9a9bc9"

[[claims.provenance.anchors]]
file = "Cargo.toml"
symbol = "workspace.members"
lines = []

[[claims.history]]
at = 1780980000
event = "written"
by_session = "brainstorm-2026-06-09-ocean-context"

[[claims]]
id = "c2"
text = "The full design (Layer A build + Layer B backlog + master equation + theory provenance) is committed and is the source of truth."
status = "Verified"
knowledge_tier = "Common"
confidence = 1.0

[claims.provenance]
commit_sha = "d9a9bc9"

[[claims.provenance.anchors]]
file = "docs/specs/ocean-context-handoff-engine.md"
lines = []

[[claims.history]]
at = 1780980000
event = "written"
by_session = "brainstorm-2026-06-09-ocean-context"

[[claims]]
id = "c4"
text = "Schema validated against reality: regex anchor extraction pulled 51 real anchored claims from 2 root HANDOFF.md docs."
status = "Verified"
knowledge_tier = "Individual"
confidence = 0.85

[claims.provenance]
commit_sha = "d9a9bc9"

[[claims.provenance.anchors]]
file = "HANDOFF.md"
lines = []

[[claims.history]]
at = 1780980000
event = "written"
by_session = "brainstorm-2026-06-09-ocean-context"

[[claims]]
id = "c5"
text = "Tuning method is REPLAY of real ocean-os history (218 commits) + the 51 real claims; a human judges REVERIFY verdicts. NOT a synthetic oracle. Decided by John."
status = "Verified"
knowledge_tier = "Common"
confidence = 1.0

[claims.provenance]
commit_sha = "d9a9bc9"

[[claims.provenance.anchors]]
file = "docs/specs/ocean-context-handoff-engine.md"
symbol = "Proof / tuning method"
lines = []

[[claims.history]]
at = 1780980000
event = "written"
by_session = "brainstorm-2026-06-09-ocean-context"

[[claims]]
id = "c7"
text = "The 80 worktree HANDOFF.md files are byte-identical (one md5) — NOT 80 distinct handoffs. The real distinct corpus is 2 root docs. Do not treat worktree handoffs as data."
status = "Verified"
knowledge_tier = "Individual"
confidence = 0.9

[claims.provenance]
commit_sha = "d9a9bc9"

[[claims.provenance.anchors]]
file = ".claude/worktrees"
lines = []

[[claims.history]]
at = 1780980000
event = "written"
by_session = "brainstorm-2026-06-09-ocean-context"
+++

Codified subset of docs/specs/HANDOFF-ocean-context.md (claims c1, c2, c4, c5, c7),
used as the acceptance-4 replay input: walk ocean-os history forward from d9a9bc9
and let a human judge the verdicts.
