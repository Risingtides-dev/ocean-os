//! Browser **shell** control — tabs as first-class, addressable objects.
//!
//! The original [`BrowserHandle`](crate::BrowserHandle) caches a single "active
//! page" chosen by a heuristic (first non-extension tab). That is Layer 1–2 of
//! the agent control surface (perception + actuation *inside one page*). This
//! module is the Layer 3 jump: the **browser shell** — the agent can enumerate
//! every tab, address a specific one, open/close/switch, and read a typed
//! [`BrowserContext`] snapshot of the whole window at once.
//!
//! This is the first piece of "Ocean browser proper": the agent stops being a
//! guest driving one mystery tab and starts *owning the tab model*. It still
//! drives the same Chromium over CDP — owning the engine is explicitly a
//! non-goal (see `docs/OCEAN_BROWSER_CONTROL_SURFACE.md`).

use serde::{Deserialize, Serialize};

use crate::{BrowserError, BrowserHandle, Result};

/// A stable, agent-facing identifier for a tab. We use Chrome's CDP target id
/// (the GUID behind every page) so a `TabId` survives across reads within a
/// session and addresses exactly one tab unambiguously.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TabId(pub String);

impl std::fmt::Display for TabId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A single tab's shell-level metadata — what the agent sees *about* a tab
/// without reading its full DOM. The cheap, always-available view.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TabInfo {
    pub id: TabId,
    pub url: String,
    pub title: String,
    /// True for the tab the shell considers active/foregrounded.
    pub active: bool,
    /// Ocean-internal pages (the cockpit extension, devtools, chrome:// UI)
    /// that aren't real browsing targets. The agent usually wants to skip these.
    pub is_chrome_internal: bool,
}

/// The always-on, structured snapshot of the whole browser shell — "what is
/// open across all tabs right now." This is the Layer-6 *always-on browsing
/// context*: the agent reads this up front instead of probing tab-by-tab, and
/// the daemon can ship it with each turn so "this page" / "the other tab"
/// resolve without a round-trip.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrowserContext {
    pub tabs: Vec<TabInfo>,
    /// The active tab's id, if one is foregrounded.
    pub active_tab: Option<TabId>,
}

impl BrowserContext {
    /// The real (non-internal) browsing tabs — what the user is actually looking
    /// at, with Ocean's own UI pages filtered out.
    pub fn browsing_tabs(&self) -> impl Iterator<Item = &TabInfo> {
        self.tabs.iter().filter(|t| !t.is_chrome_internal)
    }
}

fn is_chrome_internal(url: &str) -> bool {
    url.starts_with("chrome-extension://")
        || url.starts_with("devtools://")
        || url.starts_with("chrome://")
        || url.starts_with("chrome-untrusted://")
}

impl BrowserHandle {
    /// Enumerate every open tab as addressable [`TabInfo`]. This is the shell's
    /// ground truth — the agent's "list my tabs."
    pub async fn list_tabs(&self) -> Result<BrowserContext> {
        let pages = self.snapshot_pages().await?;
        let active_target = self.active_target_id().await;
        let mut tabs = Vec::with_capacity(pages.len());
        for p in &pages {
            let url = p.url().await.ok().flatten().unwrap_or_default();
            let title = p.get_title().await.ok().flatten().unwrap_or_default();
            let id = TabId(p.target_id().inner().clone());
            let active = active_target.as_ref() == Some(&id);
            tabs.push(TabInfo {
                is_chrome_internal: is_chrome_internal(&url),
                id,
                url,
                title,
                active,
            });
        }
        let active_tab = tabs.iter().find(|t| t.active).map(|t| t.id.clone());
        Ok(BrowserContext { tabs, active_tab })
    }

    /// Open a new tab at `url` and make it the active page. Returns its id.
    pub async fn open_tab(&self, url: &str) -> Result<TabId> {
        let page = self.new_page(url).await?;
        let id = TabId(page.target_id().inner().clone());
        self.set_active_page(page).await;
        Ok(id)
    }

    /// Switch the agent's focus to an existing tab by id. Subsequent page-level
    /// tools (read/click/type) act on this tab until switched again.
    pub async fn switch_tab(&self, id: &TabId) -> Result<()> {
        let pages = self.snapshot_pages().await?;
        let page = pages
            .into_iter()
            .find(|p| p.target_id().as_ref() == id.0.as_str())
            .ok_or_else(|| BrowserError::ElementNotFound(format!("tab {id}")))?;
        // Bring it to the foreground so it's the visible, active tab — not just
        // the one the agent drives. This is real shell control, not a silent
        // background switch.
        let _ = page.bring_to_front().await;
        self.set_active_page(page).await;
        Ok(())
    }

    /// Close a tab by id. If it was the active tab, the active-page cache is
    /// cleared so the next page-level call re-resolves a live tab.
    pub async fn close_tab(&self, id: &TabId) -> Result<()> {
        let pages = self.snapshot_pages().await?;
        let target = pages
            .into_iter()
            .find(|p| p.target_id().as_ref() == id.0.as_str())
            .ok_or_else(|| BrowserError::ElementNotFound(format!("tab {id}")))?;
        target
            .close()
            .await
            .map_err(|e| BrowserError::Cdp(e.to_string()))?;
        self.clear_active_if(id).await;
        Ok(())
    }
}
