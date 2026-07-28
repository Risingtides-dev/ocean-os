# Ocean OS Public Evidence Policy

Status: active public-site hygiene contract.

Public documentation must distinguish implemented behavior from demonstrations,
point-in-time observations, and source-grounded architecture. A runnable surface
is not automatically safe or truthful public evidence.

## Evidence classes

### Live operational evidence

A capture from an active non-demo workflow may be described as live only when it:

- records the capture date and the exact product surface or route;
- shows real behavior without claiming that the service is still running now;
- contains no secrets, session IDs, hostnames, usernames, absolute local paths,
  private repository names, customer data, or internal-only operational details;
- is visually complete and useful as evidence rather than a loading, blank, or
  failed frame; and
- remains traceable to a reproducible capture command or source-backed workflow.

Redaction must be disclosed. A redacted capture can prove behavior, but not the
literal values that were removed.

### Controlled demonstration

Synthetic prompts, test data, component render tests, seeded fixtures, and demo
endpoints are demonstrations. They may show that a rendering or interaction path
exists, but must not be labeled as production activity, real user work, or live
operational evidence.

### Source-grounded evidence

Source paths, tests, protocol definitions, manifests, and reproducible commands
can support architecture and capability claims without a runtime screenshot.
They must not imply that a process was observed running.

### Rejected evidence

Do not publish captures that expose sensitive local metadata, are blank or
failed, lack a known origin, or are stale while labeled as current. Remove them
from the current tree and queue a safe recapture instead of preserving a
misleading visual.

## Current Surface-site classification

| Asset | Classification | Public decision |
|---|---|---|
| `assets/surfaces/model-dropdown-halt.png` | Controlled demonstration | Keep; synthetic long-form prompt demonstrating model selection and halt UI. |
| `assets/surfaces/map-render-test.png` | Controlled demonstration | Keep; component-protocol map render test using public geographic data. |
| `assets/surfaces/tool-group-collapsed.png` | Rejected | Removed; exposed a local hostname, OS details, and local temporary-file names. |
| `assets/surfaces/longhouse-deck-live.png` | Rejected | Removed; blank/failed frame produced after a demo endpoint, so it did not substantiate the “live deck” claim. |
| `assets/surfaces/cli-capture.txt` | Rejected | Removed; exposed session UUIDs, local absolute paths, branch names, and stale runtime state. |

The removed files remain in public Git history. Their removal narrows the current
release tree; it is not represented as historical recall.

Stale inline CLI and session samples were also removed from `surfaces.html`,
`cli-sdk.html`, and `sessions.html`; source-grounded command and protocol
descriptions remain.

## Capture checklist

1. Use an isolated test workspace and synthetic, non-sensitive content.
2. Prefer a dedicated public demo account or fixture over a developer machine.
3. Inspect the full frame and text output for identifiers and local metadata.
4. Record capture date, command/route, demo status, and any redaction.
5. Label the evidence class in the page caption.
6. Verify the asset renders and the page has no broken local links.
7. Record meaningful public-evidence changes in the root `events.md`.
