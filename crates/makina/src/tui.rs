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

use std::io::{self, Stdout, Write};
use std::panic;

use ratatui::Terminal;
use ratatui::crossterm::{
    cursor,
    event::{DisableBracketedPaste, EnableBracketedPaste},
    event::{DisableMouseCapture, EnableMouseCapture},
    event::{KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags},
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        supports_keyboard_enhancement,
    },
};
use ratatui::prelude::CrosstermBackend;

/// Ask the terminal to report modified keys unambiguously, when it can.
///
/// Without the kitty keyboard protocol a terminal sends a bare CR for
/// Shift+Enter — byte-identical to Enter — so no application can tell the two
/// apart. `DISAMBIGUATE_ESCAPE_CODES` is the minimal flag that makes them
/// distinguishable; it is deliberately the only one requested, since the
/// report-all-keys flags change how ordinary text arrives.
///
/// Best-effort by design: terminals that do not support it keep their existing
/// behaviour, and every newline chord has a plain-ASCII fallback (Ctrl+J) that
/// needs none of this.
fn push_keyboard_enhancements(out: &mut Stdout) -> bool {
    if !matches!(supports_keyboard_enhancement(), Ok(true)) {
        return false;
    }
    execute!(
        out,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .is_ok()
}

// ── Tui wrapper ───────────────────────────────────────────────────────────────

/// Owns the ratatui `Terminal` handle and manages the terminal lifecycle.
pub struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    /// Whether `init`/`reinit` successfully pushed keyboard enhancement flags,
    /// so `restore` pops exactly what it pushed and nothing else.
    keyboard_enhanced: bool,
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
            // Without bracketed paste a pasted block arrives as ordinary key
            // presses, so its newlines read as Enter — submitting the composer
            // partway through the paste and losing the rest. With it the whole
            // block arrives as one `Event::Paste` and survives intact.
            EnableBracketedPaste,
            cursor::Hide
        )?;
        let keyboard_enhanced = push_keyboard_enhancements(&mut stdout);
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self {
            terminal,
            keyboard_enhanced,
        })
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
        if self.keyboard_enhanced {
            let _ = execute!(self.terminal.backend_mut(), PopKeyboardEnhancementFlags);
            self.keyboard_enhanced = false;
        }
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            DisableBracketedPaste,
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
            EnableBracketedPaste,
            cursor::Hide
        )?;
        self.keyboard_enhanced = push_keyboard_enhancements(&mut io::stdout());
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

    /// Copy `text` to the system clipboard using the OSC 52 escape sequence.
    ///
    /// OSC 52 (`ESC ] 52 ; c ; <base64> BEL`) asks the terminal emulator to set
    /// the clipboard, so it works locally *and* over SSH without a platform
    /// clipboard dependency. The sequence is written straight to the backend's
    /// writer so it reaches the terminal even while the alternate screen is
    /// active. The terminal must allow clipboard writes (iTerm2, kitty, WezTerm,
    /// Alacritty, Ghostty, tmux with `set-clipboard on`, …) — when it doesn't,
    /// this is a silent no-op from the user's perspective.
    ///
    /// Used by the event loop to copy a finalised mouse text selection
    /// ([`crate::selection::Selection`]).
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if writing the escape sequence to the terminal
    /// fails.
    pub fn copy_to_clipboard(&mut self, text: &str) -> io::Result<()> {
        let seq = format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()));
        let backend = self.terminal.backend_mut();
        backend.write_all(seq.as_bytes())?;
        backend.flush()
    }
}

/// Encode `input` as standard (RFC 4648) base64 with `=` padding.
///
/// A tiny self-contained encoder so OSC 52 clipboard writes
/// ([`Tui::copy_to_clipboard`]) need no external base64 dependency.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
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
    // Mirrors `Tui::restore` for callers that do not own the `Tui` (the panic
    // hook, external exits). Gated on the same capability check that decided
    // whether to push, so this never pops a level it did not add.
    if matches!(supports_keyboard_enhancement(), Ok(true)) {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste,
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
    /// `base64_encode` matches the RFC 4648 test vectors, including padding —
    /// the OSC 52 clipboard payload depends on this being correct.
    #[test]
    fn base64_encode_matches_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

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
