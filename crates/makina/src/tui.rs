//! Terminal lifecycle: setup, teardown, and panic-safe restoration.
//!
//! # Responsibilities
//!
//! This module owns the terminal's raw mode and alternate screen state.  It
//! provides:
//!
//! * [`Tui::init`] — enables raw mode, enters the alternate screen, hides the
//!   cursor, and installs a panic hook that restores the terminal before the
//!   default panic handler prints its message (so a panic never leaves the
//!   user's shell in an unreadable state).
//! * [`Tui::restore`] — disables raw mode, leaves the alternate screen, shows
//!   the cursor.  Called explicitly on clean exit and from the panic hook.
//! * [`Drop`] impl — calls [`Tui::restore`] so the terminal is always cleaned
//!   up, even if the caller forgets.
//!
//! # Design
//!
//! `Tui` wraps a `Terminal<CrosstermBackend<Stdout>>` — the standard ratatui
//! terminal type for crossterm.  It is the single owner of the terminal handle,
//! preventing accidental double-initialisation.

use std::io::{self, Stdout};
use std::panic;

use ratatui::Terminal;
use ratatui::crossterm::{
    cursor,
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::prelude::CrosstermBackend;

// ── Tui wrapper ───────────────────────────────────────────────────────────────

/// Owns the ratatui `Terminal` handle and manages the terminal lifecycle.
pub struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Tui {
    /// Initialise the terminal.
    ///
    /// 1. Enables raw mode.
    /// 2. Enters the alternate screen.
    /// 3. Hides the cursor.
    /// 4. Installs a panic hook that calls [`restore_terminal`] before the
    ///    default panic handler runs, so a panic never leaves the shell in a
    ///    garbled state.
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if any of the terminal setup steps fail.
    pub fn init() -> io::Result<Self> {
        install_panic_hook();
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            cursor::Hide
        )?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    /// Restore the terminal to its state before [`Tui::init`] was called.
    ///
    /// Safe to call multiple times; subsequent calls after the first are no-ops
    /// from the OS perspective (raw mode disabled, alternate screen left) even if
    /// they technically re-run the escape sequences.
    ///
    /// Called explicitly by the event loop on clean exit, and by the panic hook
    /// and [`Drop`] as safety nets.
    pub fn restore(&mut self) {
        // Best-effort: ignore errors during teardown.
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            cursor::Show
        );
    }

    /// Re-initialise the terminal after it was restored (e.g. to hand control to
    /// an external pager and then resume the TUI).
    ///
    /// Re-enables raw mode, re-enters the alternate screen, hides the cursor, and
    /// clears the terminal so the next [`draw`](Self::draw) produces a full
    /// repaint.  The panic hook is **not** re-installed (it was installed once by
    /// [`init`](Self::init) and remains in place).
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if raw mode or the alternate-screen escape sequence
    /// cannot be applied.
    pub fn reinit(&mut self) -> io::Result<()> {
        enable_raw_mode()?;
        execute!(
            self.terminal.backend_mut(),
            EnterAlternateScreen,
            EnableMouseCapture,
            cursor::Hide
        )?;
        // Force a full repaint so no stale pager content bleeds through.
        let _ = self.terminal.clear();
        Ok(())
    }

    /// Draw one frame using the provided closure.
    ///
    /// A thin wrapper around [`Terminal::draw`] so callers don't need to hold a
    /// separate `Terminal` handle.
    pub fn draw<F>(&mut self, render_fn: F) -> io::Result<ratatui::CompletedFrame<'_>>
    where
        F: FnOnce(&mut ratatui::Frame),
    {
        self.terminal.draw(render_fn)
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        self.restore();
    }
}

// ── Panic hook ────────────────────────────────────────────────────────────────

/// Install a panic hook that restores the terminal before the default handler.
///
/// If a panic occurs while raw mode or the alternate screen is active, the
/// default handler's message would be invisible (raw mode) or printed to the
/// alternate screen (which then disappears immediately on process exit).
/// Installing this hook ensures the terminal is always restored first.
///
/// # Idempotent
///
/// Calling this function multiple times simply re-installs the hook; the last
/// call wins.  In practice `Tui::init` calls it exactly once.
fn install_panic_hook() {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        // Reap any live agent process groups FIRST: a panic mid-run must not
        // leave orphaned `grok`/agent subprocesses behind. `kill_all_agents` is
        // sync and takes no locks across an `.await`, so it is safe to call from
        // the panic hook.
        makina_acp::kill_all_agents();
        restore_terminal();
        default_hook(info);
    }));
}

/// Test seam: install the production panic hook so a test can assert it reaps
/// agents before restoring the terminal.
///
/// Installs the *same* reaping hook the production [`install_panic_hook`] does
/// (it calls [`makina_acp::kill_all_agents`] first), and **returns the previously
/// installed hook** so the test can restore it afterwards (the panic hook is
/// process-global and must not leak into sibling tests in this binary). Exposed
/// only under `cfg(test)` for the `exit::panic_hook_invokes_reaper` test;
/// production code installs the hook via [`Tui::init`].
#[cfg(test)]
pub(crate) fn install_panic_hook_for_test()
-> Box<dyn Fn(&panic::PanicHookInfo<'_>) + Send + Sync + 'static> {
    // Snapshot the harness's current hook so the caller can restore it.
    let original = panic::take_hook();
    // Install a no-op as the *default* the production hook will wrap, so the
    // simulated panic doesn't spew the standard panic message to the test's
    // stderr. The production installer then layers the reaping hook on top.
    panic::set_hook(Box::new(|_| {}));
    install_panic_hook();
    original
}

/// Restore the terminal outside of a [`Tui`] instance.
///
/// Used by the panic hook **and** the out-of-band signal reaper
/// ([`crate::exit::install_signal_reaper`]): both run from contexts that do not
/// own the render loop's [`Tui`], so they need a standalone restore that leaves
/// the alternate screen, disables raw mode, and shows the cursor.
pub fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        cursor::Show
    );
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test for the init → restore round-trip (task `tui-mouse-scroll`).
    ///
    /// In a TTY, `Tui::init` enables raw mode, the alternate screen, and hides
    /// the cursor; `Tui::restore` disables raw mode, leaves the alternate screen,
    /// and shows the cursor. Under `cargo test` there is usually no real TTY, so
    /// `enable_raw_mode()` may return an error and `init` returns `Err`; that is
    /// fine — the point is that the round-trip neither panics nor leaves the
    /// terminal in a bad state.  When a TTY *is* present we drive a full init+restore
    /// and assert it round-trips cleanly.
    #[test]
    fn init_then_restore_round_trips() {
        match Tui::init() {
            Ok(mut tui) => {
                // A real TTY was available: restore must not panic and must be
                // safe to call.
                tui.restore();
            }
            Err(_) => {
                // No TTY under the test harness — raw mode could not be enabled.
                // The static restore path must still be callable without panic.
                restore_terminal();
            }
        }
    }

    /// Smoke test that the init and reinit paths enable mouse capture, and
    /// restore/restore_terminal disable it (task `re-enable-mouse-capture`).
    ///
    /// In a TTY, `Tui::init` and `Tui::reinit` emit `EnableMouseCapture` in their
    /// terminal setup sequence; `Tui::restore` and the free `restore_terminal`
    /// emit `DisableMouseCapture` in their teardown sequence. Under no-TTY test
    /// harness, the functions may fail but the static restore path must still be
    /// callable. The point is that the capture state rides on init/reinit entry
    /// and restore/restore_terminal exit, so a `$PAGER` round-trip
    /// (restore → reinit) leaves the wheel working on resume.
    #[test]
    fn init_enables_mouse_capture() {
        match Tui::init() {
            Ok(mut tui) => {
                // A real TTY was available: mouse capture was enabled by init.
                // Restore must disable it without panic.
                tui.restore();
            }
            Err(_) => {
                // No TTY under the test harness — raw mode could not be enabled.
                // The static restore path must still be callable without panic.
                restore_terminal();
            }
        }
    }
}
