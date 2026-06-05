//! Network **capture** — real request/response observation over the CDP Network
//! domain, far richer than the `performance.getEntriesByType` timing read.
//!
//! The existing `read_network` only sees resource *timings* (name/type/duration)
//! after the fact. This module taps the live CDP `Network.responseReceived`
//! event stream: the agent sees every request's URL, method, status, mime type,
//! and (on demand) the response **body**. That is the high-value Layer-1/2
//! unlock for scraping — the agent can read the JSON an API returned, not just
//! that a request happened.
//!
//! Capture is opt-in per handle: call [`BrowserHandle::start_netcap`] to begin
//! buffering, then [`BrowserHandle::captured_requests`] to read what landed.

use std::collections::VecDeque;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{BrowserError, BrowserHandle, Result};

/// One observed network response. The cheap metadata view — body is fetched
/// separately and on demand (bodies can be large; we don't buffer them all).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapturedResponse {
    /// CDP request id — used to fetch the body later.
    pub request_id: String,
    pub url: String,
    pub status: i64,
    pub mime_type: String,
}

/// A bounded ring buffer of captured responses. Bounded so a long-running
/// session capturing a chatty page can't grow without limit.
#[derive(Clone)]
pub struct NetCapture {
    buf: Arc<Mutex<VecDeque<CapturedResponse>>>,
    cap: usize,
}

impl NetCapture {
    fn new(cap: usize) -> Self {
        Self {
            buf: Arc::new(Mutex::new(VecDeque::with_capacity(cap.min(256)))),
            cap,
        }
    }

    async fn push(&self, r: CapturedResponse) {
        let mut buf = self.buf.lock().await;
        if buf.len() >= self.cap {
            buf.pop_front();
        }
        buf.push_back(r);
    }

    async fn snapshot(&self) -> Vec<CapturedResponse> {
        self.buf.lock().await.iter().cloned().collect()
    }
}

impl BrowserHandle {
    /// Begin capturing network responses on the active page. Enables the CDP
    /// Network domain and spawns a listener that buffers every
    /// `responseReceived` event. Idempotent-ish: calling again starts a fresh
    /// capture on the (current) active page. `cap` bounds the ring buffer.
    pub async fn start_netcap(&self, cap: usize) -> Result<()> {
        use chromiumoxide::cdp::browser_protocol::network::{
            EnableParams, EventResponseReceived,
        };

        let page = self.active_page().await?;
        // Turn on the Network domain so responseReceived events flow.
        page.execute(EnableParams::default())
            .await
            .map_err(|e| BrowserError::Cdp(e.to_string()))?;

        let capture = NetCapture::new(cap.max(1));
        let mut events = page
            .event_listener::<EventResponseReceived>()
            .await
            .map_err(|e| BrowserError::Cdp(e.to_string()))?;

        let sink = capture.clone();
        // The listener task lives as long as the EventStream; it ends when the
        // page/handle is dropped. We don't join it — it's fire-and-forget
        // background buffering.
        tokio::spawn(async move {
            use futures::StreamExt;
            while let Some(ev) = events.next().await {
                sink.push(CapturedResponse {
                    request_id: ev.request_id.inner().clone(),
                    url: ev.response.url.clone(),
                    status: ev.response.status,
                    mime_type: ev.response.mime_type.clone(),
                })
                .await;
            }
        });

        *self.netcap.lock().await = Some(capture);
        Ok(())
    }

    /// Read the captured responses (metadata only). Empty if capture wasn't
    /// started. Returns most-recent-last.
    pub async fn captured_requests(&self) -> Vec<CapturedResponse> {
        match self.netcap.lock().await.as_ref() {
            Some(c) => c.snapshot().await,
            None => Vec::new(),
        }
    }

    /// Fetch the response **body** for a captured request id. This is the
    /// scraping payoff: read the actual JSON/HTML a request returned. Bodies
    /// aren't buffered automatically (they can be huge) — fetch the ones you
    /// need by id from [`captured_requests`].
    pub async fn response_body(&self, request_id: &str) -> Result<String> {
        use chromiumoxide::cdp::browser_protocol::network::{
            GetResponseBodyParams, RequestId,
        };
        let page = self.active_page().await?;
        let params = GetResponseBodyParams::new(RequestId::new(request_id));
        let body = page
            .execute(params)
            .await
            .map_err(|e| BrowserError::Cdp(e.to_string()))?;
        Ok(body.result.body.clone())
    }
}
