//! Terminal lifecycle (TODO 1.6).
//!
//! Raw mode and the alternate screen are process-global side effects on the
//! user's terminal. Leaving them set is the classic way a TUI ruins a shell
//! session, so restoration has to survive every exit path — clean return,
//! error return, and panic.
//!
//! Drop alone is not enough for the panic case: the panic hook runs *before*
//! unwinding, so without a hook the panic message is printed into the alternate
//! screen and then vanishes with it. Hence a hook that restores first and then
//! delegates to the default one, plus an idempotent `restore` so the guard's
//! Drop during unwinding is a no-op.

use std::io::Write;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use crossterm::{cursor, execute, terminal};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::tty::Screen;

pub type Tui = Terminal<CrosstermBackend<Screen>>;

/// True whenever the terminal is in its normal state, so `restore` can be
/// called from anywhere as often as it likes.
static RESTORED: AtomicBool = AtomicBool::new(true);
static PANIC_HOOK: Once = Once::new();

/// Owns the terminal's modified state. Dropping it puts the terminal back.
pub struct TerminalGuard {
    tui: Tui,
}

impl TerminalGuard {
    pub fn tui(&mut self) -> &mut Tui {
        &mut self.tui
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Enters raw mode and the alternate screen.
///
/// Mouse capture is deliberately *not* enabled. It would take over the
/// terminal's own selection and break copy-paste, and PLAN §7.1 commits to a
/// keyboard-driven UI where the mouse is never required.
pub fn enter() -> Result<TerminalGuard> {
    crate::tty::verify_keyboard()?;

    let mut screen = Screen::open()?;
    terminal::enable_raw_mode().context("cannot enable raw mode")?;
    RESTORED.store(false, Ordering::SeqCst);

    // From here on any early return must restore, which the guard below cannot
    // yet do — so failures go through `restore_on_error`.
    if let Err(e) = execute!(screen, terminal::EnterAlternateScreen, cursor::Hide) {
        restore();
        return Err(e).context("cannot enter the alternate screen");
    }

    install_panic_hook();

    // Deliberately no `Terminal::clear()` here. It snapshots the cursor
    // position first, and crossterm implements that as a DSR round-trip
    // (`ESC[6n` to stdout, answer back through the event source) with a retry
    // loop that has neither backoff nor an attempt limit. Two ways that hangs
    // at 100% CPU: an event source that failed to initialise, and a redirected
    // stdout, which sends the query into a file where no terminal will ever
    // answer it. Entering the alternate screen already gives a blank screen,
    // and the first `draw` paints all of it, so the round-trip buys nothing.
    let tui = match Terminal::new(CrosstermBackend::new(screen)) {
        Ok(t) => t,
        Err(e) => {
            restore();
            return Err(e).context("cannot initialise the terminal backend");
        }
    };

    Ok(TerminalGuard { tui })
}

/// Puts the terminal back. Idempotent, best-effort, safe to call from a panic
/// hook or a signal-ish context.
pub fn restore() {
    if RESTORED.swap(true, Ordering::SeqCst) {
        return;
    }

    let _ = terminal::disable_raw_mode();

    // A fresh handle rather than the terminal's own: the panic hook does not
    // have access to it, and this way there is exactly one restore path.
    if let Ok(mut screen) = Screen::open() {
        let _ = execute!(screen, terminal::LeaveAlternateScreen, cursor::Show);
        let _ = screen.flush();
    }
}

fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Order matters: restore first so the panic message lands on the
            // user's real screen and stays there.
            restore();
            crate::logging::record_panic(info);
            previous(info);
        }));
    });
}
