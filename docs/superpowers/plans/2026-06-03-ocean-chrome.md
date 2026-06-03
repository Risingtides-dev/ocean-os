# Ocean-Chrome Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the Ocean agent full control of a dedicated Chrome instance over the DevTools Protocol (navigate, click, type, scroll, screenshot, run JS, read console/network, hybrid DOM/screenshot perception), and repackage the existing `ocean-surface` Leptos/WASM UI as a Chrome extension side panel that auto-follows the live conversation while browser work happens.

**Architecture:** A new `ocean-browser` crate wraps `chromiumoxide` (CDP driver) and launches a persistent-profile Chrome with the Ocean extension preloaded. A set of permission-gated `AgentTool` implementations in `ocean-runtime` expose the browser to the agent loop; each browser tool emits a `BrowserActivity` side-effect so the daemon can stream `browser-active`/`browser-idle` on the existing SSE bus. The `ocean-surface` Trunk build gains an MV3 extension packaging step; the side panel loads the same compiled WASM bundle and listens for the activity events to auto-focus/release.

**Tech Stack:** Rust, `chromiumoxide` 0.9.1 (CDP), `tokio`, `async-trait`, `serde_json`, `base64`; existing `AgentTool` trait + `AgentToolResult.side_effects`; `ocean_protocol::Content::Image` for screenshots; Leptos/WASM + Trunk for the surface; Chrome MV3 (`side_panel`, `manifest.json`).

**Spec:** `docs/superpowers/specs/2026-06-02-ocean-chrome-design.md`

---

## STATUS: ✅ COMPLETE (2026-06-03)

All 14 tasks implemented and committed across `ocean-os` and `ocean-surface`.
Workspace builds release-clean; real-Chrome smoke tests pass headless.

**Deviations from the original plan (all improvements found during build):**

1. **chromiumoxide features:** `tokio-runtime` doesn't exist in 0.9; any TLS
   feature (`rustls`/`native-tls`) drags in the broken `chromiumoxide_fetcher`
   (zip-gate compile error). Fixed by using **default features only** — we
   drive the system Chrome, never the fetcher.
2. **type/key dispatch:** `Page::type_str`/`press_key` aren't on the public
   `Page` in 0.9 (only on `Element` / the internal handler). Replicated the
   handler's exact CDP `DispatchKeyEvent` dispatch via a `dispatch_key` free fn
   using the public `keys::get_key_definition`.
3. **Daemon integration:** instead of stashing a lazy handle in `AppState` and
   appending tools per-turn, implemented a **`BrowserProvider`
   (CapabilityProvider)** registered in `build_capability_registry`. This fits
   the existing tool seam exactly — lazy launch lives in the provider's cached
   `tools()`, and the agent runtime needed no per-turn changes.
4. **Event path:** `browser-active/idle` flows as a first-class
   `AgentEvent::BrowserActivity` → `AgentTurnEvent::BrowserActivity` wire event
   (not a hand-rolled JSON publish), so it rides the existing SSE serializer.
   Required adding exhaustive match arms in the TUI (2 sites) — done.
5. **build-extension.sh:** `cp dist/*.js` also matched `sw.js`; fixed to copy
   exact filenames. `--filehash false` confirmed working in Trunk 0.21.

**Not verifiable in this environment:** a full agent turn driving the browser
needs an LLM provider/API key configured (none present). The CDP layer itself
is proven by the passing real-Chrome smoke tests; the agent wiring compiles and
registers the tools.

---

## File Structure

**New crate `crates/ocean-browser/`** — owns the Chrome process + CDP session. One responsibility: be a typed, async handle to a running Chrome.
- `Cargo.toml`
- `src/lib.rs` — public `BrowserHandle` API (launch, navigate, click, type, key, scroll, screenshot, eval_js, read_console, read_network, read_page).
- `src/launch.rs` — Chrome discovery + launch flags (profile dir, remote-debugging, `--load-extension`).
- `src/perception.rs` — hybrid `read_page`: DOM/a11y snapshot + element-ref table, screenshot fallback decision.
- `src/error.rs` — `BrowserError` enum (retryable vs fatal).

**New tools in `crates/ocean-runtime/src/tools/browser/`** — thin `AgentTool` wrappers over `ocean-browser`. Split by responsibility, one file per concern:
- `mod.rs` — shared `BrowserToolCtx` (holds `Arc<BrowserHandle>`), `browser_tools()` constructor, the `BrowserActivity` side-effect helper.
- `nav.rs` — `BrowserNavigateTool`.
- `perceive.rs` — `BrowserReadPageTool`, `BrowserScreenshotTool`.
- `input.rs` — `BrowserClickTool`, `BrowserTypeTool`, `BrowserKeyTool`, `BrowserScrollTool`.
- `inspect.rs` — `BrowserEvalJsTool`, `BrowserConsoleTool`, `BrowserNetworkTool`.

**Modified:**
- `crates/ocean-runtime/src/types.rs` — add `BrowserActivity { active: bool }` variant to `ToolSideEffect`.
- `crates/ocean-runtime/src/tools/mod.rs` — re-export `browser` module (tools wired in by the daemon, not `default_tools()`, since they need a live handle).
- `crates/ocean-runtime/Cargo.toml` — depend on `ocean-browser`.
- `crates/ocean-daemon/src/main.rs` — lazily launch the browser handle, build `browser_tools(handle)`, append to the agent's tool set; map `BrowserActivity` side-effects onto the event bus.
- `Cargo.toml` (workspace root) — add `ocean-browser` member + `chromiumoxide`, `base64` to `[workspace.dependencies]`.

**Surface (`../ocean-surface`):**
- `extension/manifest.json` — MV3 manifest (created).
- `extension/sidepanel.html` — loads the WASM bundle (created).
- `extension/background.js` — opens the side panel on action click (created).
- `scripts/build-extension.sh` — Trunk build + assemble `extension/dist/` (created).
- `crates/ocean-surface-ui/src/daemon.rs` — add `browser-active`/`browser-idle` to the event enum (modified).
- `crates/ocean-surface-ui/src/app.rs` — handoff hook: on `browser-active` set a focus signal, on `browser-idle` clear it (modified).

---

## Task 1: Scaffold the `ocean-browser` crate

**Files:**
- Create: `crates/ocean-browser/Cargo.toml`
- Create: `crates/ocean-browser/src/lib.rs`
- Create: `crates/ocean-browser/src/error.rs`
- Modify: `Cargo.toml` (workspace root)

- [ ] **Step 1: Add workspace deps + member**

In root `Cargo.toml`, under `[workspace.dependencies]` add:

```toml
chromiumoxide = { version = "0.9", features = ["tokio-runtime"], default-features = false }
base64 = "0.22"
```

Under `[workspace]` `members`, add `"crates/ocean-browser"`.

- [ ] **Step 2: Create the crate manifest**

`crates/ocean-browser/Cargo.toml`:

```toml
[package]
name = "ocean-browser"
version = "0.1.0"
edition = { workspace = true }
license = { workspace = true }
authors = { workspace = true }
rust-version = { workspace = true }
description = "Typed async handle to a Chrome instance driven over the DevTools Protocol."

[dependencies]
chromiumoxide = { workspace = true }
tokio = { workspace = true }
tokio-stream = { workspace = true }
futures = "0.3"
serde = { workspace = true }
serde_json = { workspace = true }
base64 = { workspace = true }
thiserror = "1"
tracing = { workspace = true }

[dev-dependencies]
tokio = { workspace = true }
```

- [ ] **Step 3: Define the error type**

`crates/ocean-browser/src/error.rs`:

```rust
//! Browser errors split into retryable (transient CDP/page faults the agent can
//! retry) and fatal (Chrome missing / failed to launch) so callers map them
//! onto tool errors vs hard failures.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum BrowserError {
    #[error("chrome could not be launched: {0}")]
    Launch(String),
    #[error("no active page/tab")]
    NoPage,
    #[error("navigation failed: {0}")]
    Navigation(String),
    #[error("cdp call failed: {0}")]
    Cdp(String),
    #[error("element not found: {0}")]
    ElementNotFound(String),
    #[error("timeout: {0}")]
    Timeout(String),
}

impl BrowserError {
    /// Whether the agent should be told it may retry. Launch failures are fatal;
    /// everything else is a transient page-state issue.
    pub fn retryable(&self) -> bool {
        !matches!(self, BrowserError::Launch(_))
    }
}
```

- [ ] **Step 4: Stub lib.rs so the crate compiles**

`crates/ocean-browser/src/lib.rs`:

```rust
//! Typed async handle to a Chrome instance over the DevTools Protocol.

pub mod error;

pub use error::BrowserError;

/// Re-exported result alias used across the crate.
pub type Result<T> = std::result::Result<T, BrowserError>;
```

- [ ] **Step 5: Verify it compiles**

Run: `cargo build -p ocean-browser`
Expected: compiles clean (only the error + stub lib).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/ocean-browser
git commit -m "feat(browser): scaffold ocean-browser crate (chromiumoxide + error type)"
```

---

## Task 2: Launch Chrome with the right flags

**Files:**
- Create: `crates/ocean-browser/src/launch.rs`
- Modify: `crates/ocean-browser/src/lib.rs`
- Test: inline `#[cfg(test)]` in `launch.rs`

- [ ] **Step 1: Write the failing test (flag assembly is pure, test it directly)**

In `crates/ocean-browser/src/launch.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn flags_include_profile_and_extension() {
        let cfg = LaunchConfig {
            profile_dir: Path::new("/tmp/ocean-profile").to_path_buf(),
            extension_dir: Some(Path::new("/tmp/ocean-ext").to_path_buf()),
            headless: false,
            port: 0,
        };
        let args = cfg.to_args();
        assert!(args.iter().any(|a| a == "--user-data-dir=/tmp/ocean-profile"));
        assert!(args.iter().any(|a| a == "--load-extension=/tmp/ocean-ext"));
        assert!(args.iter().any(|a| a.starts_with("--remote-debugging-port=")));
        assert!(!args.iter().any(|a| a == "--headless=new"));
    }

    #[test]
    fn headless_adds_flag() {
        let cfg = LaunchConfig {
            profile_dir: Path::new("/tmp/p").to_path_buf(),
            extension_dir: None,
            headless: true,
            port: 9333,
        };
        let args = cfg.to_args();
        assert!(args.iter().any(|a| a == "--headless=new"));
        assert!(args.iter().any(|a| a == "--remote-debugging-port=9333"));
        assert!(!args.iter().any(|a| a.starts_with("--load-extension")));
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p ocean-browser launch`
Expected: FAIL — `LaunchConfig` / `to_args` not defined.

- [ ] **Step 3: Implement `LaunchConfig` + launch**

Prepend to `crates/ocean-browser/src/launch.rs`:

```rust
//! Chrome discovery + launch flag assembly. The flag list is pure and unit
//! tested; the actual spawn is delegated to chromiumoxide.

use std::path::PathBuf;

use chromiumoxide::browser::{Browser, BrowserConfig};
use futures::StreamExt;

use crate::error::BrowserError;

/// Inputs that determine how Chrome is launched.
#[derive(Debug, Clone)]
pub struct LaunchConfig {
    /// Persistent profile dir so logins survive restarts.
    pub profile_dir: PathBuf,
    /// Unpacked extension to preload (the Ocean cockpit). None in tests/headless.
    pub extension_dir: Option<PathBuf>,
    pub headless: bool,
    /// 0 lets the OS pick a free port.
    pub port: u16,
}

impl LaunchConfig {
    /// Assemble the raw chrome CLI args. Pure — unit tested.
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec![
            format!("--user-data-dir={}", self.profile_dir.display()),
            format!("--remote-debugging-port={}", self.port),
            "--no-first-run".to_string(),
            "--no-default-browser-check".to_string(),
        ];
        if self.headless {
            args.push("--headless=new".to_string());
        }
        if let Some(ext) = &self.extension_dir {
            args.push(format!("--load-extension={}", ext.display()));
            // Extensions are disabled in headless; only meaningful headful.
            args.push(format!("--disable-extensions-except={}", ext.display()));
        }
        args
    }
}

/// A launched Chrome plus its CDP handler task. Dropping this kills Chrome.
pub struct LaunchedChrome {
    pub browser: Browser,
}

/// Launch Chrome via chromiumoxide using our flag set. Spawns the required
/// CDP event-handler task internally.
pub async fn launch(cfg: &LaunchConfig) -> Result<LaunchedChrome, BrowserError> {
    let mut builder = BrowserConfig::builder()
        .user_data_dir(&cfg.profile_dir)
        .arg("--no-first-run")
        .arg("--no-default-browser-check");
    if cfg.headless {
        builder = builder.arg("--headless=new");
    } else {
        builder = builder.with_head();
    }
    if let Some(ext) = &cfg.extension_dir {
        builder = builder
            .arg(format!("--load-extension={}", ext.display()))
            .arg(format!("--disable-extensions-except={}", ext.display()));
    }
    let config = builder
        .build()
        .map_err(|e| BrowserError::Launch(e.to_string()))?;

    let (browser, mut handler) = Browser::launch(config)
        .await
        .map_err(|e| BrowserError::Launch(e.to_string()))?;

    // The handler future must be polled for CDP to make progress.
    tokio::spawn(async move {
        while let Some(ev) = handler.next().await {
            if ev.is_err() {
                break;
            }
        }
    });

    Ok(LaunchedChrome { browser })
}
```

> Note: `to_args` mirrors the builder flags for unit-testability; the builder is the real launch path. Keep them in sync.

- [ ] **Step 4: Export from lib.rs**

Add to `crates/ocean-browser/src/lib.rs`:

```rust
pub mod launch;
pub use launch::{launch, LaunchConfig, LaunchedChrome};
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p ocean-browser launch`
Expected: PASS (both flag tests).

- [ ] **Step 6: Commit**

```bash
git add crates/ocean-browser/src/launch.rs crates/ocean-browser/src/lib.rs
git commit -m "feat(browser): chrome launch config + flag assembly (tested)"
```

---

## Task 3: `BrowserHandle` — navigate + screenshot

**Files:**
- Modify: `crates/ocean-browser/src/lib.rs`
- Test: `crates/ocean-browser/tests/smoke.rs` (gated, ignored by default)

- [ ] **Step 1: Implement `BrowserHandle`**

Add to `crates/ocean-browser/src/lib.rs`:

```rust
use std::sync::Arc;

use chromiumoxide::page::Page;
use tokio::sync::Mutex;

use crate::launch::{launch, LaunchConfig, LaunchedChrome};

/// Shared, cloneable handle to one Chrome + its active page. Cloning shares the
/// same underlying browser (Arc). The active page is mutexed because tools may
/// be called concurrently within a turn.
#[derive(Clone)]
pub struct BrowserHandle {
    inner: Arc<Mutex<LaunchedChrome>>,
    page: Arc<Mutex<Option<Page>>>,
}

impl BrowserHandle {
    pub async fn launch(cfg: LaunchConfig) -> Result<Self> {
        let chrome = launch(&cfg).await?;
        Ok(Self {
            inner: Arc::new(Mutex::new(chrome)),
            page: Arc::new(Mutex::new(None)),
        })
    }

    /// Get (or create) the active page.
    async fn active_page(&self) -> Result<Page> {
        let mut guard = self.page.lock().await;
        if let Some(p) = guard.as_ref() {
            return Ok(p.clone());
        }
        let chrome = self.inner.lock().await;
        let page = chrome
            .browser
            .new_page("about:blank")
            .await
            .map_err(|e| BrowserError::Cdp(e.to_string()))?;
        *guard = Some(page.clone());
        Ok(page)
    }

    pub async fn navigate(&self, url: &str) -> Result<String> {
        let page = self.active_page().await?;
        page.goto(url)
            .await
            .map_err(|e| BrowserError::Navigation(e.to_string()))?;
        page.wait_for_navigation()
            .await
            .map_err(|e| BrowserError::Navigation(e.to_string()))?;
        let title = page.get_title().await.ok().flatten().unwrap_or_default();
        Ok(title)
    }

    /// PNG screenshot, base64-encoded (ready for Content::Image).
    pub async fn screenshot(&self, full_page: bool) -> Result<String> {
        use base64::Engine;
        use chromiumoxide::page::ScreenshotParams;
        let page = self.active_page().await?;
        let params = ScreenshotParams::builder()
            .full_page(full_page)
            .build();
        let bytes = page
            .screenshot(params)
            .await
            .map_err(|e| BrowserError::Cdp(e.to_string()))?;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    }
}
```

- [ ] **Step 2: Write a gated smoke test**

`crates/ocean-browser/tests/smoke.rs`:

```rust
//! Real-Chrome smoke test. Ignored by default (needs a Chrome binary + display
//! or headless). Run with: `cargo test -p ocean-browser --test smoke -- --ignored`.

use ocean_browser::{BrowserHandle, LaunchConfig};

#[tokio::test]
#[ignore]
async fn navigate_and_screenshot() {
    let cfg = LaunchConfig {
        profile_dir: std::env::temp_dir().join("ocean-test-profile"),
        extension_dir: None,
        headless: true,
        port: 0,
    };
    let h = BrowserHandle::launch(cfg).await.expect("launch");
    let title = h.navigate("https://example.com").await.expect("nav");
    assert!(title.to_lowercase().contains("example"));
    let png_b64 = h.screenshot(false).await.expect("shot");
    assert!(png_b64.len() > 100);
}
```

- [ ] **Step 3: Verify the crate compiles + unit tests still pass**

Run: `cargo test -p ocean-browser`
Expected: PASS (launch tests; smoke is `#[ignore]`d so it's skipped).

- [ ] **Step 4: Run the smoke test if Chrome is available**

Run: `cargo test -p ocean-browser --test smoke -- --ignored`
Expected: PASS if a Chrome/Chromium binary is installed; otherwise a clear `Launch` error (acceptable — note it and move on).

- [ ] **Step 5: Commit**

```bash
git add crates/ocean-browser/src/lib.rs crates/ocean-browser/tests/smoke.rs
git commit -m "feat(browser): BrowserHandle navigate + screenshot"
```

---

## Task 4: Input — click (ref + x/y), type, key, scroll

**Files:**
- Modify: `crates/ocean-browser/src/lib.rs`

- [ ] **Step 1: Add input methods to `BrowserHandle`**

Add inside `impl BrowserHandle`:

```rust
    /// Click an element by CSS selector (precise path).
    pub async fn click_selector(&self, selector: &str) -> Result<()> {
        let page = self.active_page().await?;
        let el = page
            .find_element(selector)
            .await
            .map_err(|_| BrowserError::ElementNotFound(selector.to_string()))?;
        el.click()
            .await
            .map_err(|e| BrowserError::Cdp(e.to_string()))?;
        Ok(())
    }

    /// Click a raw viewport coordinate (works on canvas/video/anything).
    pub async fn click_xy(&self, x: f64, y: f64) -> Result<()> {
        use chromiumoxide::cdp::browser_protocol::input::{
            DispatchMouseEventParams, DispatchMouseEventType, MouseButton,
        };
        let page = self.active_page().await?;
        let press = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MousePressed)
            .x(x)
            .y(y)
            .button(MouseButton::Left)
            .click_count(1)
            .build()
            .map_err(BrowserError::Cdp)?;
        let release = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseReleased)
            .x(x)
            .y(y)
            .button(MouseButton::Left)
            .click_count(1)
            .build()
            .map_err(BrowserError::Cdp)?;
        page.execute(press).await.map_err(|e| BrowserError::Cdp(e.to_string()))?;
        page.execute(release).await.map_err(|e| BrowserError::Cdp(e.to_string()))?;
        Ok(())
    }

    /// Type text into the currently focused element (real keystrokes).
    pub async fn type_text(&self, text: &str) -> Result<()> {
        let page = self.active_page().await?;
        page.type_str(text)
            .await
            .map_err(|e| BrowserError::Cdp(e.to_string()))?;
        Ok(())
    }

    /// Press a single key or combo, e.g. "Enter", "Tab", "Control+a".
    pub async fn press_key(&self, key: &str) -> Result<()> {
        let page = self.active_page().await?;
        page.press_key(key)
            .await
            .map_err(|e| BrowserError::Cdp(e.to_string()))?;
        Ok(())
    }

    /// Scroll the page by a delta via window.scrollBy.
    pub async fn scroll_by(&self, dx: f64, dy: f64) -> Result<()> {
        let page = self.active_page().await?;
        page.evaluate(format!("window.scrollBy({dx},{dy})"))
            .await
            .map_err(|e| BrowserError::Cdp(e.to_string()))?;
        Ok(())
    }
```

> If `DispatchMouseEventParams::builder().build()` returns `Result<_, String>` in this chromiumoxide version, the `.map_err(BrowserError::Cdp)` matches. If it returns the struct directly, drop the `?`/`map_err`. Confirm by checking the builder signature at implementation time.

- [ ] **Step 2: Verify compile**

Run: `cargo build -p ocean-browser`
Expected: compiles. Fix the builder `map_err` per the note if the type differs.

- [ ] **Step 3: Commit**

```bash
git add crates/ocean-browser/src/lib.rs
git commit -m "feat(browser): input — click (selector + xy), type, key, scroll"
```

---

## Task 5: Hybrid perception (`read_page`) + inspect (eval/console/network)

**Files:**
- Create: `crates/ocean-browser/src/perception.rs`
- Modify: `crates/ocean-browser/src/lib.rs`

- [ ] **Step 1: Implement the perception module**

`crates/ocean-browser/src/perception.rs`:

```rust
//! Hybrid page perception. Default: a cheap structured read — visible
//! interactive elements with a stable `ref` (selector) + role + text, plus the
//! page's visible text. The caller decides whether to additionally grab a
//! screenshot (visual pages). This module returns structured data only.

use serde::Serialize;

/// One interactive element the agent can target by `ref`.
#[derive(Debug, Serialize)]
pub struct ElementRef {
    /// CSS selector usable with click_selector.
    pub r#ref: String,
    pub role: String,
    pub text: String,
}

/// Structured page read.
#[derive(Debug, Serialize)]
pub struct PageRead {
    pub url: String,
    pub title: String,
    /// Visible interactive elements (links, buttons, inputs).
    pub elements: Vec<ElementRef>,
    /// Trimmed visible text.
    pub text: String,
    /// True when the page looks visual (canvas/video heavy) and a screenshot
    /// is recommended over the structured read.
    pub visual_hint: bool,
}

/// JS evaluated in the page to build a PageRead as JSON. Kept as a single
/// expression so it can be passed to page.evaluate.
pub const READ_PAGE_JS: &str = r#"
(() => {
  const sel = (el) => {
    if (el.id) return '#' + CSS.escape(el.id);
    const parts = [];
    let n = el;
    while (n && n.nodeType === 1 && parts.length < 4) {
      let p = n.tagName.toLowerCase();
      if (n.classList.length) p += '.' + [...n.classList].slice(0,2).map(c=>CSS.escape(c)).join('.');
      const sibs = n.parentNode ? [...n.parentNode.children].filter(c=>c.tagName===n.tagName) : [];
      if (sibs.length > 1) p += `:nth-of-type(${sibs.indexOf(n)+1})`;
      parts.unshift(p);
      n = n.parentElement;
    }
    return parts.join(' > ');
  };
  const vis = (el) => {
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0 && r.bottom > 0 && r.top < innerHeight;
  };
  const out = [];
  document.querySelectorAll('a,button,input,textarea,select,[role=button],[role=link]').forEach(el => {
    if (!vis(el)) return;
    const text = (el.innerText || el.value || el.placeholder || el.getAttribute('aria-label') || '').trim().slice(0,120);
    out.push({ ref: sel(el), role: el.getAttribute('role') || el.tagName.toLowerCase(), text });
  });
  const canvases = document.querySelectorAll('canvas,video').length;
  const bodyText = (document.body ? document.body.innerText : '').trim().slice(0, 6000);
  return JSON.stringify({
    url: location.href,
    title: document.title,
    elements: out.slice(0, 120),
    text: bodyText,
    visual_hint: canvases > 0 && bodyText.length < 200
  });
})()
"#;
```

- [ ] **Step 2: Wire perception + inspect into `BrowserHandle`**

Add to `crates/ocean-browser/src/lib.rs` (and `pub mod perception;` + `pub use perception::{PageRead, ElementRef};`):

```rust
    /// Hybrid structured read of the page.
    pub async fn read_page(&self) -> Result<crate::perception::PageRead> {
        let page = self.active_page().await?;
        let val = page
            .evaluate(crate::perception::READ_PAGE_JS)
            .await
            .map_err(|e| BrowserError::Cdp(e.to_string()))?;
        let json: String = val
            .into_value()
            .map_err(|e| BrowserError::Cdp(e.to_string()))?;
        serde_json::from_str(&json).map_err(|e| BrowserError::Cdp(e.to_string()))
    }

    /// Evaluate arbitrary JS, return its result as a JSON string.
    pub async fn eval_js(&self, source: &str) -> Result<String> {
        let page = self.active_page().await?;
        let val = page
            .evaluate(source)
            .await
            .map_err(|e| BrowserError::Cdp(e.to_string()))?;
        Ok(val.value().map(|v| v.to_string()).unwrap_or_default())
    }
```

> Console + network: chromiumoxide exposes these via event listeners. For v1, implement `read_console` and `read_network` as JS-side reads to avoid a long-lived listener: `read_console` evaluates a small buffer the page populates, and `read_network` uses `performance.getEntriesByType('resource')`. Implement as:

```rust
    /// Recent network requests via the Performance API (resource timings).
    pub async fn read_network(&self) -> Result<String> {
        self.eval_js(
            "JSON.stringify(performance.getEntriesByType('resource').slice(-50).map(e=>({name:e.name,type:e.initiatorType,dur:Math.round(e.duration)})))",
        )
        .await
    }

    /// Console capture. Installs a tiny buffer on first call, returns it after.
    pub async fn read_console(&self) -> Result<String> {
        self.eval_js(
            "(()=>{if(!window.__oceanLogs){window.__oceanLogs=[];['log','warn','error'].forEach(k=>{const o=console[k];console[k]=(...a)=>{window.__oceanLogs.push(k+': '+a.join(' '));o.apply(console,a)}})}return JSON.stringify(window.__oceanLogs.slice(-100))})()",
        )
        .await
    }
```

> Trade-off documented: console capture only sees logs emitted *after* the first `read_console` call installs the hook. For v1 this is acceptable; a CDP `Runtime.consoleAPICalled` listener is the richer follow-up (logged as a known limitation, not a gap).

- [ ] **Step 3: Verify compile**

Run: `cargo build -p ocean-browser`
Expected: compiles.

- [ ] **Step 4: Extend the smoke test**

Append to `crates/ocean-browser/tests/smoke.rs`:

```rust
#[tokio::test]
#[ignore]
async fn read_page_finds_link() {
    let cfg = LaunchConfig {
        profile_dir: std::env::temp_dir().join("ocean-test-profile2"),
        extension_dir: None,
        headless: true,
        port: 0,
    };
    let h = BrowserHandle::launch(cfg).await.expect("launch");
    h.navigate("https://example.com").await.expect("nav");
    let read = h.read_page().await.expect("read");
    assert!(read.text.to_lowercase().contains("example"));
    assert!(read.elements.iter().any(|e| e.role == "a" || e.role == "link"));
}
```

- [ ] **Step 5: Run unit tests (smoke ignored)**

Run: `cargo test -p ocean-browser`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/ocean-browser/src/perception.rs crates/ocean-browser/src/lib.rs crates/ocean-browser/tests/smoke.rs
git commit -m "feat(browser): hybrid read_page + eval/console/network inspect"
```

---

## Task 6: Add the `BrowserActivity` side-effect to the runtime

**Files:**
- Modify: `crates/ocean-runtime/src/types.rs`
- Modify: `crates/ocean-runtime/Cargo.toml`

- [ ] **Step 1: Add the side-effect variant**

In `crates/ocean-runtime/src/types.rs`, extend `ToolSideEffect`:

```rust
pub enum ToolSideEffect {
    Render {
        id: String,
        kind: String,
        props: Value,
        replace: bool,
    },
    Unmount {
        id: String,
    },
    /// Signals the daemon that a browser tool started (`active: true`) or the
    /// turn's browser work is winding down (`active: false`). The daemon relays
    /// this onto the SSE bus as `browser-active` / `browser-idle` so the
    /// extension side panel can auto-focus / release.
    BrowserActivity {
        active: bool,
    },
}
```

- [ ] **Step 2: Add the crate dependency**

In `crates/ocean-runtime/Cargo.toml` `[dependencies]`:

```toml
ocean-browser = { workspace = true }
```

And in root `Cargo.toml` `[workspace.dependencies]`:

```toml
ocean-browser = { path = "crates/ocean-browser" }
```

- [ ] **Step 3: Verify compile (exhaustive matches may break — fix them)**

Run: `cargo build -p ocean-runtime 2>&1 | head -40`
Expected: either clean, or non-exhaustive-match errors on `ToolSideEffect`. If the agent loop matches `ToolSideEffect` exhaustively, add a `BrowserActivity` arm that is a no-op there (the daemon handles it, not the loop):

In `crates/ocean-runtime/src/agent_loop.rs`, wherever `ToolSideEffect` is matched, add:

```rust
            ToolSideEffect::BrowserActivity { .. } => {
                // Relayed by the daemon onto the SSE bus; no loop-side action.
            }
```

- [ ] **Step 4: Re-verify**

Run: `cargo build -p ocean-runtime`
Expected: compiles clean.

- [ ] **Step 5: Commit**

```bash
git add crates/ocean-runtime/src/types.rs crates/ocean-runtime/src/agent_loop.rs crates/ocean-runtime/Cargo.toml Cargo.toml Cargo.lock
git commit -m "feat(runtime): BrowserActivity tool side-effect for browser-active/idle"
```

---

## Task 7: Browser tools — shared context + navigate

**Files:**
- Create: `crates/ocean-runtime/src/tools/browser/mod.rs`
- Create: `crates/ocean-runtime/src/tools/browser/nav.rs`
- Modify: `crates/ocean-runtime/src/tools/mod.rs`

- [ ] **Step 1: Shared tool context + activity helper**

`crates/ocean-runtime/src/tools/browser/mod.rs`:

```rust
//! Agent-facing browser tools. Thin wrappers over `ocean_browser::BrowserHandle`.
//! Every tool is permission-gated and emits a `BrowserActivity { active: true }`
//! side-effect so the daemon can drive the side-panel handoff.

pub mod inspect;
pub mod input;
pub mod nav;
pub mod perceive;

use std::sync::Arc;

use ocean_browser::BrowserHandle;

use crate::types::{AgentToolResult, ToolSideEffect};

/// Shared dependency injected into every browser tool.
#[derive(Clone)]
pub struct BrowserToolCtx {
    pub handle: Arc<BrowserHandle>,
}

/// Build a text result that also flags browser activity for the handoff.
fn active_result(text: impl Into<String>) -> AgentToolResult {
    let mut r = AgentToolResult::text(text);
    r.side_effects.push(ToolSideEffect::BrowserActivity { active: true });
    r
}

/// Construct the full browser tool suite bound to a live handle.
pub fn browser_tools(ctx: BrowserToolCtx) -> Vec<Arc<dyn crate::types::AgentTool>> {
    vec![
        Arc::new(nav::BrowserNavigateTool { ctx: ctx.clone() }),
        Arc::new(perceive::BrowserReadPageTool { ctx: ctx.clone() }),
        Arc::new(perceive::BrowserScreenshotTool { ctx: ctx.clone() }),
        Arc::new(input::BrowserClickTool { ctx: ctx.clone() }),
        Arc::new(input::BrowserTypeTool { ctx: ctx.clone() }),
        Arc::new(input::BrowserKeyTool { ctx: ctx.clone() }),
        Arc::new(input::BrowserScrollTool { ctx: ctx.clone() }),
        Arc::new(inspect::BrowserEvalJsTool { ctx: ctx.clone() }),
        Arc::new(inspect::BrowserConsoleTool { ctx: ctx.clone() }),
        Arc::new(inspect::BrowserNetworkTool { ctx }),
    ]
}
```

- [ ] **Step 2: Navigate tool**

`crates/ocean-runtime/src/tools/browser/nav.rs`:

```rust
use async_trait::async_trait;
use serde_json::{json, Value};

use super::{active_result, BrowserToolCtx};
use crate::types::{AgentTool, AgentToolResult};

pub struct BrowserNavigateTool {
    pub ctx: BrowserToolCtx,
}

#[async_trait]
impl AgentTool for BrowserNavigateTool {
    fn name(&self) -> &str {
        "browser_navigate"
    }
    fn description(&self) -> &str {
        "Navigate the Ocean-controlled Chrome to a URL. Returns the page title."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "url": { "type": "string", "description": "Absolute URL" } },
            "required": ["url"]
        })
    }
    fn requires_permission(&self) -> bool {
        true
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let url = args.get("url").and_then(|v| v.as_str()).ok_or("missing 'url'")?;
        let title = self
            .ctx
            .handle
            .navigate(url)
            .await
            .map_err(|e| e.to_string())?;
        Ok(active_result(format!("navigated to {url} — title: {title}")))
    }
}
```

- [ ] **Step 3: Register the module**

In `crates/ocean-runtime/src/tools/mod.rs`, add `pub mod browser;` next to the other `pub mod` lines.

- [ ] **Step 4: Verify compile (perceive/input/inspect not yet created → expected fail)**

Run: `cargo build -p ocean-runtime 2>&1 | head`
Expected: FAIL — unresolved `perceive`/`input`/`inspect` modules. That's fine; next tasks create them. To compile this task in isolation, temporarily comment the not-yet-created tool entries in `browser_tools` and the `pub mod` lines, OR proceed straight to Task 8/9 before building. (Recommended: do Tasks 7–9 then build once.)

- [ ] **Step 5: Commit**

```bash
git add crates/ocean-runtime/src/tools/browser/mod.rs crates/ocean-runtime/src/tools/browser/nav.rs crates/ocean-runtime/src/tools/mod.rs
git commit -m "feat(runtime): browser tool ctx + navigate tool"
```

---

## Task 8: Browser tools — perceive (read_page + screenshot)

**Files:**
- Create: `crates/ocean-runtime/src/tools/browser/perceive.rs`

- [ ] **Step 1: Implement perceive tools**

`crates/ocean-runtime/src/tools/browser/perceive.rs`:

```rust
use async_trait::async_trait;
use ocean_protocol::Content;
use serde_json::{json, Value};

use super::{active_result, BrowserToolCtx};
use crate::types::{AgentTool, AgentToolResult, ToolSideEffect};

pub struct BrowserReadPageTool {
    pub ctx: BrowserToolCtx,
}

#[async_trait]
impl AgentTool for BrowserReadPageTool {
    fn name(&self) -> &str {
        "browser_read_page"
    }
    fn description(&self) -> &str {
        "Read the current page: title, URL, visible interactive elements (each with a `ref` selector usable by browser_click), and visible text. Cheap and precise — prefer this before screenshotting. If `visual_hint` is true, follow up with browser_screenshot."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _id: &str, _args: Value) -> Result<AgentToolResult, String> {
        let read = self.ctx.handle.read_page().await.map_err(|e| e.to_string())?;
        let body = serde_json::to_string_pretty(&read).map_err(|e| e.to_string())?;
        Ok(active_result(body))
    }
}

pub struct BrowserScreenshotTool {
    pub ctx: BrowserToolCtx,
}

#[async_trait]
impl AgentTool for BrowserScreenshotTool {
    fn name(&self) -> &str {
        "browser_screenshot"
    }
    fn description(&self) -> &str {
        "Capture a PNG screenshot of the current page. Use for visual pages (canvas/video/maps) or when browser_read_page is insufficient. Pair with browser_click x/y to act on what you see."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "full_page": { "type": "boolean", "default": false } }
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let full = args.get("full_page").and_then(|v| v.as_bool()).unwrap_or(false);
        let b64 = self.ctx.handle.screenshot(full).await.map_err(|e| e.to_string())?;
        Ok(AgentToolResult {
            content: vec![
                Content::text("screenshot:"),
                Content::Image { data: b64, mime_type: "image/png".to_string() },
            ],
            details: Value::Null,
            terminate: false,
            side_effects: vec![ToolSideEffect::BrowserActivity { active: true }],
        })
    }
}
```

- [ ] **Step 2: Commit (build happens in Task 10)**

```bash
git add crates/ocean-runtime/src/tools/browser/perceive.rs
git commit -m "feat(runtime): browser perceive tools (read_page + screenshot)"
```

---

## Task 9: Browser tools — input + inspect

**Files:**
- Create: `crates/ocean-runtime/src/tools/browser/input.rs`
- Create: `crates/ocean-runtime/src/tools/browser/inspect.rs`

- [ ] **Step 1: Input tools**

`crates/ocean-runtime/src/tools/browser/input.rs`:

```rust
use async_trait::async_trait;
use serde_json::{json, Value};

use super::{active_result, BrowserToolCtx};
use crate::types::{AgentTool, AgentToolResult};

pub struct BrowserClickTool { pub ctx: BrowserToolCtx }

#[async_trait]
impl AgentTool for BrowserClickTool {
    fn name(&self) -> &str { "browser_click" }
    fn description(&self) -> &str {
        "Click an element. Provide `ref` (a selector from browser_read_page) for precise clicks, OR `x`/`y` viewport coordinates for visual pages."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ref": { "type": "string", "description": "CSS selector from browser_read_page" },
                "x": { "type": "number" },
                "y": { "type": "number" }
            }
        })
    }
    fn requires_permission(&self) -> bool { true }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        if let Some(r) = args.get("ref").and_then(|v| v.as_str()) {
            self.ctx.handle.click_selector(r).await.map_err(|e| e.to_string())?;
            Ok(active_result(format!("clicked {r}")))
        } else if let (Some(x), Some(y)) = (
            args.get("x").and_then(|v| v.as_f64()),
            args.get("y").and_then(|v| v.as_f64()),
        ) {
            self.ctx.handle.click_xy(x, y).await.map_err(|e| e.to_string())?;
            Ok(active_result(format!("clicked ({x},{y})")))
        } else {
            Err("provide either 'ref' or both 'x' and 'y'".to_string())
        }
    }
}

pub struct BrowserTypeTool { pub ctx: BrowserToolCtx }

#[async_trait]
impl AgentTool for BrowserTypeTool {
    fn name(&self) -> &str { "browser_type" }
    fn description(&self) -> &str { "Type text into the currently focused element (real keystrokes). Click an input first." }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] })
    }
    fn requires_permission(&self) -> bool { true }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let text = args.get("text").and_then(|v| v.as_str()).ok_or("missing 'text'")?;
        self.ctx.handle.type_text(text).await.map_err(|e| e.to_string())?;
        Ok(active_result(format!("typed {} chars", text.len())))
    }
}

pub struct BrowserKeyTool { pub ctx: BrowserToolCtx }

#[async_trait]
impl AgentTool for BrowserKeyTool {
    fn name(&self) -> &str { "browser_key" }
    fn description(&self) -> &str { "Press a key or combo, e.g. 'Enter', 'Tab', 'Control+a'." }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": { "key": { "type": "string" } }, "required": ["key"] })
    }
    fn requires_permission(&self) -> bool { true }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let key = args.get("key").and_then(|v| v.as_str()).ok_or("missing 'key'")?;
        self.ctx.handle.press_key(key).await.map_err(|e| e.to_string())?;
        Ok(active_result(format!("pressed {key}")))
    }
}

pub struct BrowserScrollTool { pub ctx: BrowserToolCtx }

#[async_trait]
impl AgentTool for BrowserScrollTool {
    fn name(&self) -> &str { "browser_scroll" }
    fn description(&self) -> &str { "Scroll the page by a pixel delta (dx, dy). Positive dy scrolls down." }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "dx": { "type": "number", "default": 0 }, "dy": { "type": "number", "default": 600 } }
        })
    }
    fn requires_permission(&self) -> bool { false }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let dx = args.get("dx").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let dy = args.get("dy").and_then(|v| v.as_f64()).unwrap_or(600.0);
        self.ctx.handle.scroll_by(dx, dy).await.map_err(|e| e.to_string())?;
        Ok(active_result(format!("scrolled ({dx},{dy})")))
    }
}
```

- [ ] **Step 2: Inspect tools**

`crates/ocean-runtime/src/tools/browser/inspect.rs`:

```rust
use async_trait::async_trait;
use serde_json::{json, Value};

use super::{active_result, BrowserToolCtx};
use crate::types::{AgentTool, AgentToolResult};

pub struct BrowserEvalJsTool { pub ctx: BrowserToolCtx }

#[async_trait]
impl AgentTool for BrowserEvalJsTool {
    fn name(&self) -> &str { "browser_eval_js" }
    fn description(&self) -> &str { "Evaluate JavaScript in the page and return its result as JSON." }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": { "source": { "type": "string" } }, "required": ["source"] })
    }
    fn requires_permission(&self) -> bool { true }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let src = args.get("source").and_then(|v| v.as_str()).ok_or("missing 'source'")?;
        let out = self.ctx.handle.eval_js(src).await.map_err(|e| e.to_string())?;
        Ok(active_result(out))
    }
}

pub struct BrowserConsoleTool { pub ctx: BrowserToolCtx }

#[async_trait]
impl AgentTool for BrowserConsoleTool {
    fn name(&self) -> &str { "browser_console" }
    fn description(&self) -> &str { "Read recent console output (log/warn/error). Note: captures logs emitted after the first read this session." }
    fn parameters(&self) -> Value { json!({ "type": "object", "properties": {} }) }
    async fn execute(&self, _id: &str, _args: Value) -> Result<AgentToolResult, String> {
        let out = self.ctx.handle.read_console().await.map_err(|e| e.to_string())?;
        Ok(active_result(out))
    }
}

pub struct BrowserNetworkTool { pub ctx: BrowserToolCtx }

#[async_trait]
impl AgentTool for BrowserNetworkTool {
    fn name(&self) -> &str { "browser_network" }
    fn description(&self) -> &str { "Read recent network requests (resource timings: name, type, duration)." }
    fn parameters(&self) -> Value { json!({ "type": "object", "properties": {} }) }
    async fn execute(&self, _id: &str, _args: Value) -> Result<AgentToolResult, String> {
        let out = self.ctx.handle.read_network().await.map_err(|e| e.to_string())?;
        Ok(active_result(out))
    }
}
```

- [ ] **Step 3: Commit**

```bash
git add crates/ocean-runtime/src/tools/browser/input.rs crates/ocean-runtime/src/tools/browser/inspect.rs
git commit -m "feat(runtime): browser input + inspect tools"
```

---

## Task 10: Compile the runtime with all browser tools

**Files:** none (verification + fixes)

- [ ] **Step 1: Build the runtime**

Run: `cargo build -p ocean-runtime 2>&1 | head -40`
Expected: compiles. Likely fixes:
- `Content::Image` field names — confirm against `crates/ocean-protocol/src/types.rs:28` (`Image { data, mime_type }`). Adjust if different.
- `chromiumoxide` builder `map_err` mismatches (see Task 4 note).

- [ ] **Step 2: Run the whole workspace build**

Run: `cargo build --workspace 2>&1 | tail -20`
Expected: clean.

- [ ] **Step 3: Commit any fixups**

```bash
git add -A
git commit -m "fix(runtime): align browser tools with protocol + chromiumoxide APIs"
```

---

## Task 11: Wire the browser into the daemon (lazy launch + event relay)

**Files:**
- Modify: `crates/ocean-daemon/src/main.rs`
- Modify: `crates/ocean-daemon/Cargo.toml`

- [ ] **Step 1: Add deps**

In `crates/ocean-daemon/Cargo.toml` `[dependencies]`: `ocean-browser = { workspace = true }` (and `dirs = "5"` if not present, for the profile path).

- [ ] **Step 2: Lazy browser handle in daemon state**

Find the shared daemon state struct in `crates/ocean-daemon/src/main.rs` (the one passed to handlers, likely `AppState`/`Daemon`). Add:

```rust
    /// Lazily-launched browser. None until the first browser tool needs it.
    browser: Arc<tokio::sync::Mutex<Option<Arc<ocean_browser::BrowserHandle>>>>,
```

Initialize it `Arc::new(tokio::sync::Mutex::new(None))` where the state is constructed.

- [ ] **Step 3: Helper to get-or-launch the browser**

Add a free function / method near the agent-turn handler:

```rust
async fn ensure_browser(
    slot: &Arc<tokio::sync::Mutex<Option<Arc<ocean_browser::BrowserHandle>>>>,
) -> Result<Arc<ocean_browser::BrowserHandle>, String> {
    let mut guard = slot.lock().await;
    if let Some(h) = guard.as_ref() {
        return Ok(h.clone());
    }
    let profile_dir = dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("ocean/chrome-profile");
    let extension_dir = dirs::data_dir()
        .map(|d| d.join("ocean/chrome-extension"))
        .filter(|p| p.exists());
    let cfg = ocean_browser::LaunchConfig {
        profile_dir,
        extension_dir,
        headless: false,
        port: 0,
    };
    let handle = Arc::new(
        ocean_browser::BrowserHandle::launch(cfg)
            .await
            .map_err(|e| e.to_string())?,
    );
    *guard = Some(handle.clone());
    Ok(handle)
}
```

- [ ] **Step 4: Register browser tools on the agent + relay activity**

In the agent-turn handler, where the tool set is assembled and passed to the runtime, after building `default_tools()` append the browser suite:

```rust
    let handle = ensure_browser(&state.browser).await.map_err(|e| /* turn error */ e)?;
    let mut tools = ocean_runtime::tools::default_tools();
    tools.extend(ocean_runtime::tools::browser::browser_tools(
        ocean_runtime::tools::browser::BrowserToolCtx { handle },
    ));
```

> If launching the browser eagerly per turn is too heavy, gate `ensure_browser` behind a check of whether the prompt/turn is likely to need it — but simplest correct v1 is: launch on first turn that registers tools, reuse after. The Mutex+Option makes it launch-once.

Then, where the runtime emits `ToolSideEffect` onto the daemon's SSE bus (search for where `Render`/`Unmount` side-effects are handled — likely in the turn event loop), add:

```rust
        ToolSideEffect::BrowserActivity { active } => {
            // Emit a lightweight SSE event the surface listens for.
            // Reuse the existing event-publish path; event name carries the flag.
            publish_browser_activity(&state, &session_id, active).await;
        }
```

Implement `publish_browser_activity` to push a JSON event `{ "type": "browser-activity", "session_id": ..., "active": bool }` onto the same broadcast channel `/v1/agent/events` reads from. (Match the existing publish pattern used for agent turn events in this file.)

- [ ] **Step 5: Build the daemon**

Run: `cargo build -p ocean-daemon 2>&1 | head -40`
Expected: compiles. Fix field/method names to match the actual state struct and publish path discovered in this file.

- [ ] **Step 6: Manual smoke — daemon drives Chrome**

Run:
```bash
cargo build --workspace --release
./target/release/ocean-daemon &
curl -s -X POST localhost:4780/v1/agent/turns -H 'content-type: application/json' \
  -d '{"prompt":"open example.com and tell me the page title","cwd":"/tmp"}'
```
Expected: a Chrome window opens, navigates to example.com, the turn reports the title. (Permission prompts may appear depending on policy — allow them.)

- [ ] **Step 7: Commit**

```bash
git add crates/ocean-daemon/src/main.rs crates/ocean-daemon/Cargo.toml Cargo.lock
git commit -m "feat(daemon): lazy-launch browser, register tools, relay browser-activity SSE"
```

---

## Task 12: Package ocean-surface as a Chrome extension

**Files (in `../ocean-surface`):**
- Create: `extension/manifest.json`
- Create: `extension/sidepanel.html`
- Create: `extension/background.js`
- Create: `scripts/build-extension.sh`

- [ ] **Step 1: MV3 manifest**

`../ocean-surface/extension/manifest.json`:

```json
{
  "manifest_version": 3,
  "name": "Ocean",
  "version": "0.1.0",
  "description": "Ocean cockpit — chat that rides along while Ocean drives this browser.",
  "permissions": ["sidePanel", "storage"],
  "host_permissions": ["http://127.0.0.1:4780/*"],
  "background": { "service_worker": "background.js" },
  "action": { "default_title": "Open Ocean" },
  "side_panel": { "default_path": "sidepanel.html" },
  "content_security_policy": {
    "extension_pages": "script-src 'self' 'wasm-unsafe-eval'; object-src 'self'; connect-src http://127.0.0.1:4780 ws://127.0.0.1:4780"
  }
}
```

- [ ] **Step 2: Side panel host page**

`../ocean-surface/extension/sidepanel.html`:

```html
<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Ocean</title>
    <link rel="stylesheet" href="dist/style.css" />
  </head>
  <body>
    <script type="module">
      import init from './dist/ocean-surface-ui.js';
      init('./dist/ocean-surface-ui_bg.wasm');
    </script>
  </body>
</html>
```

> The exact JS/wasm filenames are content-hashed by Trunk. The build script (Step 4) renames them to stable names so this HTML stays valid.

- [ ] **Step 3: Background service worker (open panel on click)**

`../ocean-surface/extension/background.js`:

```js
chrome.action.onClicked.addListener(async (tab) => {
  await chrome.sidePanel.open({ tabId: tab.id });
});
chrome.runtime.onInstalled.addListener(() => {
  chrome.sidePanel.setPanelBehavior({ openPanelOnActionClick: true });
});
```

- [ ] **Step 4: Build script — Trunk → stable-named extension bundle**

`../ocean-surface/scripts/build-extension.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

# 1. Build the WASM app with Trunk (no content hashes so filenames are stable).
trunk build --release --filehash false

# 2. Assemble the extension bundle.
EXT=extension
DIST="$EXT/dist"
rm -rf "$DIST"
mkdir -p "$DIST"
cp dist/*.js  "$DIST/ocean-surface-ui.js"
cp dist/*.wasm "$DIST/ocean-surface-ui_bg.wasm"
cp dist/*.css "$DIST/style.css" 2>/dev/null || true

echo "Extension built at $EXT/ — load it unpacked in chrome://extensions, or"
echo "copy to the daemon's extension dir so it auto-loads."
```

Make executable: `chmod +x ../ocean-surface/scripts/build-extension.sh`

> `--filehash false` makes Trunk emit `ocean-surface-ui.js` / `_bg.wasm` without hashes. If the flag name differs in the installed Trunk version, the `cp dist/*.js` glob still copies+renames the hashed file to the stable name — the glob is the robust path.

- [ ] **Step 5: Build it**

Run:
```bash
cd ../ocean-surface && ./scripts/build-extension.sh && ls extension/dist
```
Expected: `ocean-surface-ui.js`, `ocean-surface-ui_bg.wasm`, `style.css` present.

- [ ] **Step 6: Load unpacked + verify it renders**

Manual: `chrome://extensions` → Developer mode → Load unpacked → select `../ocean-surface/extension`. Click the Ocean toolbar icon → side panel opens → the WASM UI loads and connects to the daemon at `:4780`.
Expected: the familiar Ocean chat renders in the side panel.

- [ ] **Step 7: Commit (in ocean-surface repo)**

```bash
cd ../ocean-surface
git add extension scripts/build-extension.sh
git commit -m "feat(extension): package ocean-surface as MV3 Chrome side-panel extension"
```

---

## Task 13: Handoff hook in the surface UI

**Files (in `../ocean-surface`):**
- Modify: `crates/ocean-surface-ui/src/daemon.rs`
- Modify: `crates/ocean-surface-ui/src/app.rs`

- [ ] **Step 1: Recognize the browser-activity event**

In `crates/ocean-surface-ui/src/daemon.rs`, the daemon event enum (the one deserialized from `/v1/agent/events`) needs a new variant. Add to that enum:

```rust
    /// Daemon relayed a browser tool starting/stopping; drives the side-panel
    /// auto-focus handoff. `session_id` lets a global SSE client filter.
    #[serde(rename = "browser-activity")]
    BrowserActivity {
        session_id: String,
        active: bool,
    },
```

> Match the existing `#[serde(tag = "type", ...)]` style already used in this enum (the file shows `session_id`-tagged variants — mirror that exactly).

- [ ] **Step 2: Add a focus signal + react to the event**

In `crates/ocean-surface-ui/src/app.rs`, where the app holds reactive state, add a signal:

```rust
    // True while Ocean is actively driving the browser — the side panel uses
    // this to take focus; cleared when browser work ends.
    let (browser_focus, set_browser_focus) = create_signal(false);
```

In the event-handling match (where incoming daemon events update signals), add:

```rust
        DaemonEvent::BrowserActivity { active, .. } => {
            set_browser_focus.set(active);
            // In the extension context, take focus when active. window.focus()
            // is a no-op in a tab but brings the side panel forward when run
            // inside the extension page.
            if active {
                let _ = web_sys::window().map(|w| w.focus());
            }
        }
```

> If `create_signal` is already namespaced (Leptos version), match the existing signal-creation idiom in this file. The point is: one boolean signal toggled by the event.

- [ ] **Step 3: Optional visual cue (small, additive)**

Where the top-level view is rendered, gate a thin banner on `browser_focus`:

```rust
        {move || browser_focus.get().then(|| view! {
            <div class="ocean-browser-banner">"Ocean is driving the browser…"</div>
        })}
```

(Add a minimal `.ocean-browser-banner` rule to `style.css` — a one-line accent bar. Skip if styling is out of scope for v1.)

- [ ] **Step 4: Rebuild the extension bundle**

Run: `cd ../ocean-surface && ./scripts/build-extension.sh`
Expected: rebuilds clean.

- [ ] **Step 5: End-to-end manual verification**

1. Start the daemon: `cd ../ocean-os && ./target/release/ocean-daemon &`
2. Trigger a browser turn from the TUI or curl (as in Task 11 Step 6).
3. Watch: the daemon's Chrome opens, the Ocean side panel shows the banner / takes focus when browser tools fire, and clears when the turn ends.

Expected: the conversation visibly "follows" into the side panel during browser work and releases after.

- [ ] **Step 6: Commit**

```bash
cd ../ocean-surface
git add crates/ocean-surface-ui/src/daemon.rs crates/ocean-surface-ui/src/app.rs style.css
git commit -m "feat(surface): browser-activity handoff — side panel auto-focus on browser work"
```

---

## Task 14: Auto-stage the extension for the daemon + docs

**Files:**
- Modify: `crates/ocean-daemon/src/main.rs` (or a small build step)
- Create: `docs/OCEAN_CHROME.md`

- [ ] **Step 1: Document the wiring**

`docs/OCEAN_CHROME.md` — short operator doc:

```markdown
# Ocean-Chrome

Ocean drives a dedicated Chrome over the DevTools Protocol and shows a chat
side panel inside it.

## Build
1. Build the extension: `cd ../ocean-surface && ./scripts/build-extension.sh`
2. Copy it where the daemon expects it:
   `cp -r ../ocean-surface/extension "$XDG_DATA_HOME/ocean/chrome-extension"`
   (macOS: `~/Library/Application Support/ocean/chrome-extension`)
3. Build + run the daemon: `cargo build --workspace --release && ./target/release/ocean-daemon`

## Use
Ask Ocean to do anything web ("open X, click Y"). On the first browser tool
call the daemon launches Chrome with the extension preloaded; the side panel
auto-focuses while it works and releases when done.

## Tools
browser_navigate, browser_read_page, browser_screenshot, browser_click
(ref or x/y), browser_type, browser_key, browser_scroll, browser_eval_js,
browser_console, browser_network. All but read/scroll are permission-gated.

## Profile
Logins persist in `<data>/ocean/chrome-profile`. Log into your accounts once.

## Known limits (v1)
- console capture sees logs emitted after the first browser_console call.
- read_page is blind to canvas/video — use browser_screenshot + x/y clicks.
```

- [ ] **Step 2: Commit**

```bash
cd ../ocean-os
git add docs/OCEAN_CHROME.md
git commit -m "docs: Ocean-Chrome operator guide"
```

- [ ] **Step 3: Final full build + test**

Run:
```bash
cargo build --workspace --release 2>&1 | tail -5
cargo test -p ocean-browser 2>&1 | tail -10
```
Expected: workspace builds; ocean-browser unit tests pass (smoke `#[ignore]`d).

---

## Self-Review Notes

- **Spec coverage:** launch model (T2, T11), full tool set incl. screenshot/mouse/JS/console/network (T3–T9), hybrid perception (T5/T8), extension packaging of ocean-surface (T12), auto handoff via browser-active/idle (T6→T11→T13), own profile with persistent logins (T11), permission gating (T7–T9 `requires_permission`), error retryable/fatal split (T1). All spec sections map to tasks.
- **Open decisions resolved:** CDP driver = `chromiumoxide` 0.9 (T1); browser-active/idle = `BrowserActivity` side-effect → SSE relay (T6/T11); read_page fallback heuristic = `visual_hint` (T5); Trunk→MV3 = `--filehash false` + assemble script (T12); launched-Chrome lifecycle = lazy launch-once in daemon (T11).
- **Type consistency:** `BrowserToolCtx`/`BrowserHandle`/`active_result`/`BrowserActivity { active }`/`Content::Image { data, mime_type }` used consistently across tasks. Method names (`click_selector`, `click_xy`, `type_text`, `press_key`, `scroll_by`, `read_page`, `eval_js`, `read_console`, `read_network`, `screenshot`, `navigate`) defined in T3–T5 and consumed unchanged in T7–T9.
- **Known v1 limitations are documented, not hidden:** console-capture timing and canvas-blind read_page (T5 notes + T14 doc).
