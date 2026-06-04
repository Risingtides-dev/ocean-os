# Handoff — Browser Control Plane spec doc

> **For the local coding agent picking this up.** This is a handoff artifact, not a deliverable —
> delete it before merging. It carries the full approved plan, the current state, and exactly what
> (if anything) is left to do.

## How to pick this up

```bash
git fetch origin claude/refine-local-plan-vOe28
git checkout claude/refine-local-plan-vOe28
```

- **PR:** https://github.com/Risingtides-dev/ocean-os/pull/31 (draft)
- **Branch:** `claude/refine-local-plan-vOe28`
- **Deliverable already on the branch:** `docs/OCEAN_BROWSER_CONTROL_PLANE.md`

## Current state (what's already done)

The task was *plan it first, don't build* — produce a single spec/roadmap markdown doc, **no source
code changes**. That is **done**:

- `docs/OCEAN_BROWSER_CONTROL_PLANE.md` is written and committed.
- It was verified against the codebase (see the plan's "ground truth" section below).
- `git status` was clean apart from the new doc — no Rust/source files were touched.
- Draft PR #31 is open.

**So there is no remaining implementation work for the doc itself.** What's left is John's
decision-making (the three open decisions at the end of the doc) and, if/when approved, the *actual
phased build* the doc describes — which is a separate, future effort, not part of this branch.

If you (local agent) are meant to continue, the natural next steps are:
1. Get John's answers to the three open decisions (§6 of the doc).
2. Start Phase 1's one remaining gap: add a `surface-extension` arm to `append_client_type`
   (`crates/ocean-agent/src/lib.rs:1981`), mirroring the existing `surface-web`/`surface-gpui` arms.
   This is the smallest concrete code step the doc identifies.

## Verified ground truth (don't re-verify from scratch)

- 10 CDP browser tools exist, split across `crates/ocean-runtime/src/tools/browser/`:
  `nav.rs` (`browser_navigate`), `input.rs` (`browser_click`/`type`/`key`/`scroll`),
  `inspect.rs` (`browser_console`/`network`), `perceive.rs` (`browser_read_page`/`screenshot`),
  registered via `BrowserProvider` (`mod.rs:91,120`).
- Read/mutate permission split is real: reads ungated, mutators implement `requires_permission()`
  (`input.rs:29,67,97,130`, `nav.rs:26`, `inspect.rs:22`); contract at `mod.rs:2`.
- Single browser + single active page: `crates/ocean-browser/src/lib.rs:19–20`; `active_page()`
  (`lib.rs:37–82`) skips `chrome-extension://` and devtools to prefer the real tab; lazy launch in
  `launch.rs`.
- `CapabilityProvider` registry: `crates/ocean-runtime/src/capability.rs:65`; MCP rides it too
  (`crates/ocean-mcp`).
- `client_type` plumbing end-to-end: `AgentTurnRequest.client_type`
  (`crates/ocean-agent-sdk/src/lib.rs:200`) → daemon (`crates/ocean-daemon/src/main.rs:1757–1853`,
  `guidance` dropped at 1763) → `build_system_prompt` (`crates/ocean-agent/src/lib.rs:452`) →
  `append_client_type` (`lib.rs:1981`).

**Two things the original draft got wrong (the doc corrects them):**
1. **Phase 1 is NOT done.** There is no `surface-extension` arm and no `extension_surface_prompt`
   anywhere in the repo (checked all branches). A `surface-extension` client_type currently falls
   through to the generic `Some(other)` "unknown client" message. Plumbing is real; the dedicated
   extension prompt is not built.
2. **`ocean-acp` ≠ "ACP-level".** `crates/ocean-acp/` is the *Agent Client Protocol* bridge for Zed.
   John's "ACP-level control plane" means breadth of browser control, not that crate. Don't conflate.

**Not verifiable in the remote session:** `ocean-surface` was not cloned in the environment where
this was produced. The doc's ocean-surface citations (`surface_client_type()` in
`crates/ocean-surface-ui/src/daemon.rs`, MV3 `manifest.json` permissions) are flagged
*verify-before-trust*. Confirm them against the local `../ocean-surface` checkout.

---

# Full approved plan

## Plan: Write `docs/OCEAN_BROWSER_CONTROL_PLANE.md` (spec doc only)

### Context

John wants the Ocean Chrome extension to grow into a "full-blown ACP-level control plane" — the
agent steering the browser through every path (UI, API, CLI, dev tools). He chose **"Plan it first,
don't build"**: the deliverable is a single planning/spec markdown doc, not code. The doc lives in
**ocean-os** (`docs/OCEAN_BROWSER_CONTROL_PLANE.md`) because the runtime/tools/types are the brain's
concern even though the extension UI ships in ocean-surface.

On approval, the **only** action is writing that one markdown file. No source changes, no build, no
daemon restart.

### The doc's sections

1. **Goal & framing** — agent as first-class browser operator across four surfaces (UI / API /
   CLI-style / dev tools); current single-browser daemon-driven Chrome-for-Testing model; one-line
   ACP disambiguation.
2. **The floor** — table of what exists today, each row citing a real path, plus the honest caveat
   that the `surface-extension` prompt arm does not exist yet.
3. **Phased roadmap** — P1 identity (plumbed, 1 arm left) → P2 active-tab context (highest leverage,
   next; daemon-side (a) vs extension `tabs` (b)) → P3 typed `ClientContext` (schema + serde
   dual-compat) → P4 per-tab/multi-tab steering → P5 DOM bridge / content-script reach (gated) →
   P6 tool-registry expansion (ACP/MCP shape).
4. **Type changes** — `ClientContext`/`BrowserContext` field list, attach point
   (`AgentTurnRequest`, `crates/ocean-agent-sdk/src/lib.rs:178`), daemon unpack
   (`crates/ocean-daemon/src/main.rs:1841–1853`), agent read (`crates/ocean-agent/src/lib.rs:452`),
   note `guidance` is dead.
5. **Permission & security model** — read/mutate gating contract; manifest-escalation map
   (`tabs`→P2b, `scripting`→P5, `debugger`→user-Chrome); audit via `BrowserActivity` side-effect
   (currently a handoff flag, not an audit trail); daemon-Chrome vs user-Chrome boundary as a
   deliberate security stance.
6. **Open decisions for John** — (1) active-tab context daemon-side (a) or extension `tabs` (b);
   (2) stay on isolated Chrome-for-Testing or graduate to the user's real Chrome; (3) build per-tab
   steering now or defer.

### Verification

1. `docs/OCEAN_BROWSER_CONTROL_PLANE.md` exists and renders cleanly.
2. Every "floor" claim cites a path matching code in this repo; ocean-surface claims flagged
   verify-before-trust.
3. Doc states Phase 1's remaining gap rather than claiming it done.
4. ACP terminology disambiguated (`ocean-acp` ≠ John's "ACP-level").
5. Sequencing reads done → next-most-leverage → heaviest-escalation-last; ends with the three
   decisions.
6. `git status` shows only the new doc added — no source changes.

### Guardrails

- **No code.** Only `docs/OCEAN_BROWSER_CONTROL_PLANE.md` is written (this HANDOFF.md is a separate
  handoff artifact, to be deleted before merge).
- **No deploy / no daemon touch.**
