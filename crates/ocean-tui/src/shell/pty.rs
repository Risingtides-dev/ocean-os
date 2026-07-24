//! A real embedded terminal: a shell in a PTY, parsed by vt100, rendered by
//! tui-term. Harvested near-verbatim from CTRL's `term.rs` — it's already
//! provider-agnostic. The reader stays on a std thread (PTY reads are blocking
//! and infrequent); the app pumps it each tick.

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

// vt100 underflows on 1-column grids (seen on 0.15; 0.16 doesn't claim a fix
// either), so keep the PTY at a safe floor.
const MIN_ROWS: u16 = 2;
const MIN_COLS: u16 = 8;

pub struct TermPane {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    _child: Box<dyn Child + Send + Sync>,
    pub parser: vt100::Parser,
    rx: Receiver<Vec<u8>>,
    rows: u16,
    cols: u16,
    scrollback: usize,
}

impl TermPane {
    pub fn new(cwd: &std::path::Path, rows: u16, cols: u16) -> Result<Self> {
        let rows = rows.max(MIN_ROWS);
        let cols = cols.max(MIN_COLS);
        let pty = native_pty_system();
        let pair = pty.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let mut cmd = CommandBuilder::new(shell);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Self {
            master: pair.master,
            writer,
            _child: child,
            parser: vt100::Parser::new(rows, cols, 2000),
            rx,
            rows,
            cols,
            scrollback: 0,
        })
    }

    /// Drain pending shell output into the vt100 parser. Returns true if any
    /// bytes were processed (so the caller can request a redraw).
    pub fn pump(&mut self) -> bool {
        let mut any = false;
        while let Ok(bytes) = self.rx.try_recv() {
            self.parser.process(&bytes);
            any = true;
        }
        any
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(MIN_ROWS);
        let cols = cols.max(MIN_COLS);
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.parser.screen_mut().set_size(rows, cols);
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    pub fn send(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    pub fn send_key(&mut self, k: KeyEvent) {
        if self.scrollback != 0 {
            self.scrollback = 0;
            self.parser.screen_mut().set_scrollback(0);
        }
        let bytes = key_to_bytes(k);
        if !bytes.is_empty() {
            self.send(&bytes);
        }
    }

    /// Paste into the child. When the inner app enabled bracketed paste
    /// (DEC mode 2004 — vt100 tracks it from the child's own escape output),
    /// wrap the text in the paste markers so the app receives one atomic
    /// paste; otherwise fall back to classic terminal behavior where pasted
    /// newlines arrive as carriage returns.
    pub fn paste(&mut self, text: &str) {
        if self.scrollback != 0 {
            self.scrollback = 0;
            self.parser.screen_mut().set_scrollback(0);
        }
        let bytes = paste_bytes(text, self.parser.screen().bracketed_paste());
        if !bytes.is_empty() {
            self.send(&bytes);
        }
    }
}

/// Translate a crossterm key event into the byte sequence a shell expects.
pub fn key_to_bytes(k: KeyEvent) -> Vec<u8> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let mut out: Vec<u8> = Vec::new();
    match k.code {
        KeyCode::Char(c) => {
            if ctrl {
                let b = (c.to_ascii_uppercase() as u8).wrapping_sub(b'@') & 0x7f;
                out.push(b);
            } else {
                if alt {
                    out.push(0x1b);
                }
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Up => out.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => out.extend_from_slice(b"\x1b[B"),
        KeyCode::Right => out.extend_from_slice(b"\x1b[C"),
        KeyCode::Left => out.extend_from_slice(b"\x1b[D"),
        KeyCode::Home => out.extend_from_slice(b"\x1b[H"),
        KeyCode::End => out.extend_from_slice(b"\x1b[F"),
        KeyCode::PageUp => out.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => out.extend_from_slice(b"\x1b[6~"),
        KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
        _ => {}
    }
    out
}

/// The byte stream a paste produces for the child PTY. Bracketed mode wraps
/// the text VERBATIM in ESC[200~ / ESC[201~ (the app un-brackets it itself);
/// raw mode converts newlines to carriage returns — what a real terminal
/// sends on paste — so line-oriented prompts see Enter, not a bare LF.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut out = Vec::with_capacity(text.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

#[cfg(test)]
mod paste_tests {
    use super::paste_bytes;

    #[test]
    fn raw_paste_converts_newlines_to_carriage_returns() {
        assert_eq!(paste_bytes("a\nb\r\nc", false), b"a\rb\rc".to_vec());
    }

    #[test]
    fn bracketed_paste_wraps_text_verbatim() {
        let got = paste_bytes("x\ny", true);
        assert_eq!(got, b"\x1b[200~x\ny\x1b[201~".to_vec());
    }
}
