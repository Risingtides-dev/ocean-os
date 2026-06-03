# Ocean-Chrome

Ocean drives a dedicated Chrome over the DevTools Protocol and shows a chat
side panel inside it. One conversation, lived in the daemon; the side-panel
cockpit auto-follows while Ocean drives the browser, then releases back to
your origin surface (TUI / PWA).

## Build

1. Build the extension (in the `ocean-surface` repo):
   ```bash
   cd ../ocean-surface && ./scripts/build-extension.sh
   ```
2. Copy it where the daemon expects it (sits next to the daemon's config dir):
   ```bash
   cp -r ../ocean-surface/extension "${XDG_CONFIG_HOME:-$HOME/.config}/ocean/chrome-extension"
   ```
3. Build + run the daemon:
   ```bash
   cargo build --workspace --release && ./target/release/ocean-daemon
   ```

The extension is optional — without it, Ocean still fully drives Chrome; you
just won't get the in-browser side panel. The daemon preloads the extension
only if `<config>/chrome-extension` exists.

## Use

Ask Ocean to do anything web ("open X, click Y, read me the result"). On the
first browser tool call the daemon launches Chrome with the extension
preloaded; the side panel auto-focuses while it works and releases when done.

Chrome launches **lazily** — only the first turn that needs a browser pays the
launch cost; after that the same window is reused for the daemon's lifetime.

## Tools

All exposed to the agent, permission-gated except the read-only ones:

| Tool | Permission | What it does |
|---|---|---|
| `browser_navigate` | ✅ prompts | Go to a URL |
| `browser_read_page` | read-only | Structured read: title, URL, interactive elements (each with a `ref`), visible text, `visual_hint` |
| `browser_screenshot` | read-only | PNG of the page (for visual pages / canvas / video) |
| `browser_click` | ✅ prompts | Click by `ref` (from read_page) or `x`/`y` pixel |
| `browser_type` | ✅ prompts | Type into the focused element |
| `browser_key` | ✅ prompts | Press a key (Enter, Tab, …) |
| `browser_scroll` | read-only | Scroll by a pixel delta |
| `browser_eval_js` | ✅ prompts | Run JS in the page |
| `browser_console` | read-only | Recent console output |
| `browser_network` | read-only | Recent network requests (resource timings) |

Perception is **hybrid**: prefer `browser_read_page` (cheap, precise); when it
reports `visual_hint: true` (canvas/video) or isn't enough, use
`browser_screenshot` + `browser_click` x/y.

## Profile

Logins persist in `<config>/chrome-profile`. Log into your accounts once in
Ocean's Chrome; the profile remembers them across restarts.

## Handoff

When a browser tool fires, the daemon emits `browser_activity { active: true }`
on `/v1/agent/events`. The side panel takes focus; the TUI shows a passive
note. When browser work ends it emits `active: false` and focus releases.

## Known limits (v1)

- `browser_console` only sees logs emitted **after** its first call this
  session (it installs the capture hook on first use). A CDP
  `Runtime.consoleAPICalled` listener is the richer follow-up.
- `browser_read_page` is blind to canvas/video content — use
  `browser_screenshot` + x/y clicks there.
- One tab at a time (the active page). Multi-tab orchestration is a follow-up.
