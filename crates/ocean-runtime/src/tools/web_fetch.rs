use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::types::{AgentTool, AgentToolResult};

/// Total wall-clock deadline for one fetch — connect + headers + body. The turn
/// timeout only bounds the provider stream, NOT tool execution, so without this
/// a hung server (or a tar-pit) froze the whole turn until a manual cancel.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Connect-phase deadline (DNS + TCP + TLS), tighter than the total.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Max response-body bytes buffered in memory. The body is streamed chunk by
/// chunk and reading STOPS at the cap — a multi-GB file or an endless streaming
/// endpoint can no longer grow daemon RAM without bound. 2 MiB is orders of
/// magnitude above the default 8 KB excerpt actually returned to the model.
const DEFAULT_MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Fetch a URL and return a coarse text representation of the response body.
/// Very intentionally simple: strips HTML tags by removing `<...>` sequences,
/// collapses whitespace, and truncates to a sensible size. For richer parsing,
/// upstream callers can install a custom tool.
pub struct WebFetchTool {
    timeout: Duration,
    connect_timeout: Duration,
    max_body_bytes: usize,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }
    }
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test seam: shrink the deadlines/cap so hang- and flood-scenarios run in
    /// milliseconds instead of the production 30s/2MiB.
    #[cfg(test)]
    fn with_limits(timeout: Duration, connect_timeout: Duration, max_body_bytes: usize) -> Self {
        Self {
            timeout,
            connect_timeout,
            max_body_bytes,
        }
    }
}

#[async_trait]
impl AgentTool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn concurrency(&self) -> crate::types::Concurrency {
        crate::types::Concurrency::Shared
    }
    fn description(&self) -> &str {
        "Fetch a URL via HTTPS and return a text-only excerpt of the response body. Use for documentation pages, GitHub READMEs, status checks, etc."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "Absolute URL to fetch"},
                "max_chars": {"type": "integer", "default": 8000}
            },
            "required": ["url"]
        })
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or("missing 'url'")?
            .to_string();
        let max_chars = args
            .get("max_chars")
            .and_then(|v| v.as_u64())
            .unwrap_or(8000) as usize;

        // The client-level timeout is a TOTAL deadline: it keeps counting while
        // the body streams, so a slow-drip body can't extend the fetch forever.
        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .connect_timeout(self.connect_timeout)
            .build()
            .map_err(|e| format!("fetch {url}: building client: {e}"))?;
        let mut resp = client
            .get(&url)
            .header("user-agent", "pi-coding-agent/1.0")
            .send()
            .await
            .map_err(|e| format!("fetch {url}: {e}"))?;
        let status = resp.status();

        // Stream the body with a hard byte cap. Reading STOPS at the cap (the
        // connection is dropped with the response), so neither a huge file nor
        // an endless stream endpoint buffers unbounded memory.
        let mut body: Vec<u8> = Vec::new();
        let mut capped = false;
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    let room = self.max_body_bytes - body.len();
                    if chunk.len() >= room {
                        body.extend_from_slice(&chunk[..room]);
                        capped = true;
                        break;
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => {
                    // Body errors after some bytes arrived (timeout mid-body,
                    // reset): surface what we have rather than discarding it —
                    // the model usually only needs the head anyway.
                    if body.is_empty() {
                        return Err(format!("fetch {url}: reading body: {e}"));
                    }
                    capped = true;
                    break;
                }
            }
        }
        let text = String::from_utf8_lossy(&body);
        let stripped = strip_html(&text);
        let truncated = truncate_to_budget(stripped, max_chars);
        let cap_note = if capped {
            format!(
                "\n...(body capped at {} bytes; page continues)",
                self.max_body_bytes
            )
        } else {
            String::new()
        };
        Ok(AgentToolResult::text(format!(
            "GET {url} [{status}]\n{truncated}{cap_note}"
        )))
    }
}

fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    let mut last_ws = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                if !last_ws {
                    out.push(' ');
                    last_ws = true;
                }
            }
            c if in_tag => {
                let _ = c;
            }
            c if c.is_whitespace() => {
                if !last_ws {
                    out.push(' ');
                    last_ws = true;
                }
            }
            c => {
                out.push(c);
                last_ws = false;
            }
        }
    }
    out.trim().to_string()
}

/// Truncate `s` to roughly `max_chars` BYTES, appending a "(truncated, N chars
/// total)" note. `max_chars` is a byte budget but `&str[..n]` panics if `n`
/// splits a multibyte UTF-8 char — common on any non-ASCII page — so we walk
/// back to the nearest char boundary at or below the budget.
fn truncate_to_budget(s: String, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s;
    }
    let mut cut = max_chars;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n...(truncated, {} chars total)", &s[..cut], s.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    #[test]
    fn truncate_never_splits_a_utf8_char() {
        // 20 bytes of 'é' (2 bytes each); a byte budget of 5 lands mid-char.
        // Pre-fix this panicked; now it walks back to byte 4.
        let s = "é".repeat(10);
        let out = truncate_to_budget(s.clone(), 5);
        assert!(out.starts_with("éé"), "kept whole chars: {out}");
        assert!(out.contains("truncated"));
        // under budget → returned untouched
        assert_eq!(truncate_to_budget("hi".to_string(), 100), "hi");
        // ASCII at an exact boundary still works
        assert!(truncate_to_budget("abcdef".to_string(), 3).starts_with("abc"));
    }

    /// A server that accepts and then never responds must NOT hang the turn:
    /// the fetch errors out within the tool's own deadline. Pre-fix, the
    /// default reqwest client had NO timeout and this hung forever (the turn
    /// timeout only bounds the provider stream, not tools).
    #[tokio::test]
    async fn hung_server_times_out_instead_of_hanging_the_turn() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept connections but never write a byte.
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                // Hold the socket open, silent, far longer than the test.
                tokio::spawn(async move {
                    let _sock = sock;
                    tokio::time::sleep(Duration::from_secs(60)).await;
                });
            }
        });

        let tool = WebFetchTool::with_limits(
            Duration::from_millis(300),
            Duration::from_millis(300),
            DEFAULT_MAX_BODY_BYTES,
        );
        let start = Instant::now();
        let err = tool
            .execute("t", json!({ "url": format!("http://{addr}/") }))
            .await
            .expect_err("a silent server must produce a fetch error, not hang");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "fetch must fail within the tool deadline, took {:?}",
            start.elapsed()
        );
        assert!(err.contains("fetch"), "error names the fetch: {err}");
    }

    /// A response body larger than the cap is read only up to the cap — memory
    /// stays bounded and the result carries an explicit cap note. Pre-fix,
    /// `resp.text().await` buffered the ENTIRE body (a multi-GB download or an
    /// endless stream grew daemon RAM without bound).
    #[tokio::test]
    async fn oversized_body_is_capped_not_buffered_whole() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Serve a valid HTTP response whose body is far larger than the cap.
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let body_len = 256 * 1024; // 256 KiB body vs a 16 KiB cap below
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {body_len}\r\ncontent-type: text/plain\r\n\r\n"
            );
            sock.write_all(head.as_bytes()).await.unwrap();
            let chunk = vec![b'x'; 8 * 1024];
            let mut sent = 0;
            while sent < body_len {
                if sock.write_all(&chunk).await.is_err() {
                    break; // client hung up after hitting its cap — expected
                }
                sent += chunk.len();
            }
        });

        let cap = 16 * 1024;
        let tool = WebFetchTool::with_limits(Duration::from_secs(5), Duration::from_secs(5), cap);
        let res = tool
            .execute(
                "t",
                json!({ "url": format!("http://{addr}/big"), "max_chars": 100 }),
            )
            .await
            .expect("a capped download must still succeed");
        let text = res.content[0].as_text().unwrap();
        assert!(
            text.contains("body capped"),
            "result must carry the cap note: {text}"
        );
        assert!(text.contains("200"), "status still reported: {text}");
    }
}
