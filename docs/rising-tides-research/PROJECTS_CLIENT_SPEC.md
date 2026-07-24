# Client spec: Projects (named workspaces)

For the web/PWA, Tauri, and TUI clients. The **daemon side is built, live, and
verified** (commit `bd6aebb`). This spec is the contract each client implements
so sessions stop reverting to `ocean-os` and the operator can steer by project.

## Why this exists

A turn used to fall back to the daemon's launch directory when it had no cwd, so
every project's sessions piled into the ocean-os bucket. The daemon now refuses
to guess: a turn must carry **either** a real `cwd` **or** a `project_id`. Your
client's job: let the operator pick a project, then send `project_id` on every
turn. The daemon resolves it to the project's directory.

## The daemon contract (already works — don't change the daemon)

All against the same base URL the client already uses (`/v1/...`, proxied for web).

### Project CRUD
- `GET /v1/projects` →
  ```json
  {"ok":true,"projects":[
    {"id":"<uuid>","name":"ocean-os",
     "workspace_root":"/Users/.../dev/ocean-os",
     "config":{"default_model":"deepseek-v4-pro"},
     "created_ms":..,"updated_ms":..}]}
  ```
- `POST /v1/projects` with
  `{"name":"ocean-os","workspace_root":"/abs/path","config":{"default_model":"..."}}`
  → `{"ok":true,"project":{...}}` (201). `config` is optional; `default_model`
  and `allowed_tools` inside it are both optional.
- `GET /v1/projects/{id}` → `{"ok":true,"project":{...},"sessions":[...]}` — the
  project **and its sessions** in one call (use this to show a project's history).
- `PATCH /v1/projects/{id}` with `{"name":"..."}` and/or `{"config":{...}}` —
  partial update → `{"ok":true,"project":{...}}`.
- `DELETE /v1/projects/{id}` → `{"ok":true}` (sessions are NOT deleted; they
  just become project-less).

### Sending a turn in a project (the actual fix)
`POST /v1/agent/turns` body gains one optional field:
```json
{ "prompt":"...", "session_id": null,
  "cwd": "", "project_id": "<uuid>" }
```
Rules the daemon enforces (so handle these client-side):
- non-empty `cwd` → used as-is (can target a sub-dir of the project).
- empty `cwd` + `project_id` → daemon binds to the project's `workspace_root`.
- empty `cwd` + no `project_id` → **HTTP 400**, error
  `"no working directory: ... the daemon will not guess"`. Surface this to the
  operator as "pick a project (or set a folder) first" — do not retry blindly.
- unknown `project_id` → **HTTP 400** `"unknown project id <uuid>"`.

## What each client builds

### Wire type
Add `project_id: Option<Uuid/String>` to the turn-request type, serialized as
`project_id`, omitted when `None` (`skip_serializing_if`). Mirror the existing
`session_id`/`client_type` optional-field pattern.

### State
Hold `projects: Vec<Project>` and `current_project: Option<ProjectId>` (persist
the current choice — localStorage for the shared Surface UI and app state for TUI — so it
survives reload, like the model selection).

### Calls (mirror the model-picker you already built)
- `fetch_projects()` → `GET /v1/projects`, populate the list. **Fetch it on the
  same path that resolves the daemon base URL** — NOT eagerly at startup before
  the URL is known. (This is the exact bug we hit with the model picker on
  ocean.agentsworld.org: an eager fetch ran before bootstrap learned the origin
  and silently failed. Fetch projects after the base URL is resolved.)
- `create_project(name, workspace_root, config?)` → `POST /v1/projects`.
- `set_current_project(id)` → just local state; then include `project_id` on
  every subsequent turn.
- On every turn submit: include `current_project` as `project_id`. If the client
  also knows a concrete cwd (TUI knows its own pwd), it may send that too —
  non-empty cwd wins, which is fine.

### UI
- A project picker in the header/sidebar (reuse the model-dropdown widget):
  lists `projects` by name, lets the operator pick the active one, plus a
  "+ New project" affordance (name + folder path → `create_project`).
- Show the active project name somewhere persistent (next to the model readout).
- When a turn errors with the 400 "no working directory" message, prompt the
  operator to pick a project instead of failing silently.

### Per-client notes
- **Web surface** (`ocean-surface-ui`): add to `daemon.rs` like `fetch_models`/
  `set_model`; picker in the header next to the model dropdown; persist
  `current_project` in localStorage. Proxy already forwards `/v1/*` so no proxy
  change needed (it reverse-proxies unknown `/v1/` paths; if `/v1/projects`
  isn't forwarded, add the routes mirroring `proxy_models`).
- **Tauri** (`ocean-tauri`): it hosts the same `ocean-surface-ui` implementation
  as the web/PWA, so project state and turn submission stay in the shared UI
  rather than a separate desktop client implementation.
- **TUI** (`ocean-tui`): it already knows its own working directory, so the
  minimum fix is just to **send its real `cwd`** on every turn (it may already);
  the project picker is optional polish. Sending a correct non-empty cwd alone
  fixes the revert bug for the TUI.

## Done =
From each client: pick (or create) a project, and turns run in that project's
directory — sessions land in the right workspace bucket, not ocean-os. Verified
against `ocean.agentsworld.org`, not just localhost.
