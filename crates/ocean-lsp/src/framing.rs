//! LSP wire framing: `Content-Length: N\r\n\r\n<N bytes of JSON>`.
//!
//! Unlike MCP's line-delimited JSON, the Language Server Protocol frames every
//! message with HTTP-style headers. Only `Content-Length` is meaningful; a
//! `Content-Type` header may appear and is ignored.

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// Upper bound on a single framed message. A language server legitimately sends
/// large payloads (workspace edits, big diagnostic batches), but an absurd
/// header is a protocol error, not a payload — refuse it rather than allocating
/// unbounded memory off a corrupt stream.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Write one framed JSON message.
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, json: &str) -> Result<()> {
    let header = format!("Content-Length: {}\r\n\r\n", json.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(json.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one framed JSON message. Returns `Ok(None)` on clean EOF at a frame
/// boundary (the server exited).
pub async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Option<String>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .context("reading LSP frame header")?;
        if n == 0 {
            // EOF. Clean only if it happened before any header of this frame.
            return if content_length.is_none() {
                Ok(None)
            } else {
                Err(anyhow!("EOF mid-frame (headers read, no body)"))
            };
        }
        let line = line.trim_end();
        if line.is_empty() {
            // Blank line terminates headers.
            break;
        }
        if let Some(v) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
            let len: usize = v.trim().parse().context("bad Content-Length")?;
            if len > MAX_FRAME_BYTES {
                bail!("LSP frame of {len} bytes exceeds the {MAX_FRAME_BYTES}-byte cap");
            }
            content_length = Some(len);
        }
        // Other headers (Content-Type) are ignored.
    }
    let len = content_length.ok_or_else(|| anyhow!("LSP frame missing Content-Length"))?;
    let mut buf = vec![0u8; len];
    reader
        .read_exact(&mut buf)
        .await
        .context("reading LSP frame body")?;
    Ok(Some(String::from_utf8(buf).context("LSP frame not UTF-8")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_a_frame() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, r#"{"jsonrpc":"2.0"}"#).await.unwrap();
        let mut reader = BufReader::new(buf.as_slice());
        let got = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(got, r#"{"jsonrpc":"2.0"}"#);
        // Next read: clean EOF.
        assert!(read_frame(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_absurd_content_length() {
        let raw = format!("Content-Length: {}\r\n\r\n", MAX_FRAME_BYTES + 1);
        let mut reader = BufReader::new(raw.as_bytes());
        let err = read_frame(&mut reader).await.unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");
    }

    #[tokio::test]
    async fn ignores_content_type_header() {
        let body = r#"{"x":1}"#;
        let raw = format!(
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let mut reader = BufReader::new(raw.as_bytes());
        assert_eq!(read_frame(&mut reader).await.unwrap().unwrap(), body);
    }
}
