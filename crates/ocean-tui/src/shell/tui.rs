//! Terminal lifecycle: raw mode + alternate screen enter/leave, and a panic
//! hook that restores the terminal so a crash never leaves a wedged shell.

use std::{
    io::{self, Stdout},
    ops::{Deref, DerefMut},
};

use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use ratatui::{backend::CrosstermBackend, Terminal};

pub type Backend = CrosstermBackend<Stdout>;
pub type Tui = Terminal<Backend>;

/// RAII owner for terminal mode. Every return path after `init`—including a
/// splash/render error—restores raw mode, paste/mouse flags, keyboard protocol,
/// and the alternate screen.
pub struct Guard(Tui);

impl Deref for Guard {
    type Target = Tui;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Guard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = restore();
    }
}

pub fn init() -> io::Result<Guard> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Bracketed paste: without it the terminal replays a paste as individual
    // key strokes, so any pasted newline lands as a real Enter and SUBMITS the
    // composer mid-paste. With it, a paste arrives as one Event::Paste that
    // components insert verbatim.
    if let Err(error) = execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    ) {
        let _ = restore();
        return Err(error);
    }
    // Kitty keyboard protocol where supported (iTerm2, Ghostty, kitty, WezTerm):
    // without it, modifier combos like Ctrl+Opt+1 are ambiguous or dropped.
    if matches!(supports_keyboard_enhancement(), Ok(true)) {
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    install_panic_hook();
    match Terminal::new(CrosstermBackend::new(stdout)) {
        Ok(terminal) => Ok(Guard(terminal)),
        Err(error) => {
            let _ = restore();
            Err(error)
        }
    }
}

pub fn restore() -> io::Result<()> {
    // Pop is a no-op where enhancement was never pushed.
    let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    disable_raw_mode()?;
    execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    Ok(())
}

/// Restore the terminal before the default panic handler prints, so a panic
/// message isn't swallowed by the alternate screen.
fn install_panic_hook() {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore();
        hook(info);
    }));
}
