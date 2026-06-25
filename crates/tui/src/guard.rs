//! [`TerminalGuard`] — a RAII wrapper that restores the terminal even on panic.
//!
//! Call [`TerminalGuard::install`] immediately after entering the alternate
//! screen.  Its `Drop` impl runs the full teardown sequence regardless of how
//! the process exits (normal return, `?` propagation, or panic via the hook).

use std::io;

use crossterm::{
    event::DisableMouseCapture,
    execute,
    terminal::{disable_raw_mode, LeaveAlternateScreen},
};

/// RAII guard: restores the terminal in `drop`.
///
/// Also installs a `std::panic::set_hook` that calls the teardown BEFORE
/// printing the panic message, so the panic output is visible on the normal
/// screen rather than garbled into the alternate screen buffer.
pub struct TerminalGuard;

impl TerminalGuard {
    /// Install the guard AND set the panic hook.  Call once, right after
    /// `enable_raw_mode` + `EnterAlternateScreen` + `EnableMouseCapture`.
    pub fn install() -> Self {
        // Override the panic hook so the teardown always runs on panic too.
        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Best-effort teardown; ignore errors (we're already panicking).
            let _ = restore_terminal();
            original_hook(info);
        }));
        TerminalGuard
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort: the terminal might already be restored (e.g. the panic
        // hook ran first), but a second restore is harmless.
        if let Err(e) = restore_terminal() {
            // We're in a destructor — just log; don't panic.
            eprintln!("TerminalGuard: teardown failed: {e}");
        }
    }
}

/// Unconditionally restore the terminal to a usable state.
pub fn restore_terminal() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    Ok(())
}
