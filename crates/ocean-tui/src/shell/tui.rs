//! Terminal lifecycle: raw mode + alternate screen enter/leave, and a panic
//! hook that restores the terminal so a crash never leaves a wedged shell.

use std::io::{self, Stdout};

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

pub fn init() -> io::Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Bracketed paste: without it the terminal replays a paste as individual
    // key strokes, so any pasted newline lands as a real Enter and SUBMITS the
    // composer mid-paste. With it, a paste arrives as one Event::Paste that
    // components insert verbatim.
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    // Kitty keyboard protocol where supported (iTerm2, Ghostty, kitty, WezTerm):
    // without it, modifier combos like Ctrl+Opt+1 are ambiguous or dropped.
    if matches!(supports_keyboard_enhancement(), Ok(true)) {
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    install_panic_hook();
    Terminal::new(CrosstermBackend::new(stdout))
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
