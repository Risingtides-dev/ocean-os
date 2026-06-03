//! Typed async handle to a Chrome instance over the DevTools Protocol.

pub mod error;
pub mod launch;
pub mod perception;

pub use error::BrowserError;
pub use launch::{launch, LaunchConfig, LaunchedChrome};
pub use perception::{ElementRef, PageRead};

/// Re-exported result alias used across the crate.
pub type Result<T> = std::result::Result<T, BrowserError>;

use std::sync::Arc;

use chromiumoxide::page::Page;
use tokio::sync::Mutex;

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
        // Reuse our cached page only if it's still alive (the tab wasn't closed).
        if let Some(p) = guard.as_ref() {
            if p.url().await.is_ok() {
                return Ok(p.clone());
            }
        }
        let chrome = self.inner.lock().await;
        // Prefer an EXISTING real tab — the window the user is actually looking
        // at — over spawning a fresh blank one. This is what stops the agent
        // from opening a brand-new window every turn: when you chat from the
        // side panel, you're already in a browser, so drive that browser's tab.
        // We skip the extension's own pages (chrome-extension://) and devtools.
        let existing = match chrome.browser.pages().await {
            Ok(pages) => {
                let mut chosen = None;
                for p in pages {
                    let url = p.url().await.ok().flatten().unwrap_or_default();
                    // Skip the extension's own pages, devtools, and chrome:// UI.
                    // Anything else (a real site, or even about:blank/newtab the
                    // user has open) is a usable tab to drive.
                    if url.starts_with("chrome-extension://")
                        || url.starts_with("devtools://")
                        || url.starts_with("chrome://")
                    {
                        continue;
                    }
                    chosen = Some(p);
                }
                chosen
            }
            Err(_) => None,
        };
        let page = match existing {
            Some(p) => p,
            None => chrome
                .browser
                .new_page("about:blank")
                .await
                .map_err(|e| BrowserError::Cdp(e.to_string()))?,
        };
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
        let params = ScreenshotParams::builder().full_page(full_page).build();
        let bytes = page
            .screenshot(params)
            .await
            .map_err(|e| BrowserError::Cdp(e.to_string()))?;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    }

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
        page.execute(press)
            .await
            .map_err(|e| BrowserError::Cdp(e.to_string()))?;
        page.execute(release)
            .await
            .map_err(|e| BrowserError::Cdp(e.to_string()))?;
        Ok(())
    }

    /// Type text into the currently focused element (real keystrokes). Each
    /// char is dispatched as its own key event, mirroring chromiumoxide's
    /// internal `type_str` — but on the public `Page` via raw CDP.
    pub async fn type_text(&self, text: &str) -> Result<()> {
        let page = self.active_page().await?;
        for ch in text.split("").filter(|s| !s.is_empty()) {
            dispatch_key(&page, ch).await?;
        }
        Ok(())
    }

    /// Press a single key, e.g. "Enter", "Tab", "Backspace".
    pub async fn press_key(&self, key: &str) -> Result<()> {
        let page = self.active_page().await?;
        dispatch_key(&page, key).await
    }

    /// Scroll the page by a delta via window.scrollBy.
    pub async fn scroll_by(&self, dx: f64, dy: f64) -> Result<()> {
        let page = self.active_page().await?;
        page.evaluate(format!("window.scrollBy({dx},{dy})"))
            .await
            .map_err(|e| BrowserError::Cdp(e.to_string()))?;
        Ok(())
    }

    /// Hybrid structured read of the page.
    pub async fn read_page(&self) -> Result<perception::PageRead> {
        let page = self.active_page().await?;
        let val = page
            .evaluate(perception::READ_PAGE_JS)
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
}

/// Dispatch a single key as a keyDown/keyUp pair via raw CDP. Mirrors
/// chromiumoxide's internal `press_key` (which isn't exposed on the public
/// `Page`). Looks up the US-layout key definition; for printable single chars
/// the `text` field drives the inserted character.
async fn dispatch_key(page: &Page, key: &str) -> Result<()> {
    use chromiumoxide::cdp::browser_protocol::input::{
        DispatchKeyEventParams, DispatchKeyEventType,
    };
    use chromiumoxide::keys::get_key_definition;

    let def = get_key_definition(key)
        .ok_or_else(|| BrowserError::Cdp(format!("unknown key: {key}")))?;

    let mut cmd = DispatchKeyEventParams::builder();
    let down_type = if let Some(txt) = def.text {
        cmd = cmd.text(txt);
        DispatchKeyEventType::KeyDown
    } else if def.key.len() == 1 {
        cmd = cmd.text(def.key);
        DispatchKeyEventType::KeyDown
    } else {
        DispatchKeyEventType::RawKeyDown
    };

    cmd = cmd
        .key(def.key)
        .code(def.code)
        .windows_virtual_key_code(def.key_code)
        .native_virtual_key_code(def.key_code);

    let down = cmd
        .clone()
        .r#type(down_type)
        .build()
        .map_err(BrowserError::Cdp)?;
    let up = cmd
        .r#type(DispatchKeyEventType::KeyUp)
        .build()
        .map_err(BrowserError::Cdp)?;
    page.execute(down)
        .await
        .map_err(|e| BrowserError::Cdp(e.to_string()))?;
    page.execute(up)
        .await
        .map_err(|e| BrowserError::Cdp(e.to_string()))?;
    Ok(())
}
