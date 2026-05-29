//! Async event loop.
//!
//! [`run`] is the top-level event loop.  It:
//!
//! 1. Subscribes to the api event stream once at startup.
//! 2. Starts a background task that forwards terminal input events (key
//!    presses, resize) from crossterm's async [`EventStream`] into an mpsc
//!    channel.
//! 3. Runs a `tokio::select!` loop that races:
//!    * terminal input events (from the mpsc channel),
//!    * a periodic **tick** (drives the redraw),
//!    * api events from `api.subscribe()`.
//! 4. Translates each raw event into an [`AppEvent`] and feeds it to
//!    [`App::update`].
//! 5. Re-renders the frame after any state change.
//! 6. Exits the loop when [`App::should_quit`] is set (by `AppEvent::Quit`).
//!
//! # Quit keys
//!
//! The loop exits cleanly when the user presses:
//! * `q`
//! * `Esc`
//! * `Ctrl-C`
//!
//! # Merging input + tick + api events
//!
//! ```text
//! crossterm EventStream ──► terminal_tx ──┐
//!                                          ├── tokio::select! ──► App::update ──► render
//! tokio::time::interval ──────────────────┘
//! api.subscribe() ────────────────────────┘
//! ```

use std::time::Duration;

use crossterm::event::{Event as CrosstermEvent, EventStream, KeyCode, KeyModifiers};
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::time;

use crate::app::{App, AppEvent};
use crate::tui::Tui;
use crate::ui;

// ── Tick interval ─────────────────────────────────────────────────────────────

/// How often a tick is sent to drive periodic redraws (250 ms → 4 fps minimum,
/// fast enough for smooth cursor/status updates).
const TICK_INTERVAL: Duration = Duration::from_millis(250);

// ── Event loop ────────────────────────────────────────────────────────────────

/// Run the TUI event loop until the user quits.
///
/// This function blocks the calling async task until `app.should_quit` is set.
/// The caller owns `tui` and `app`; after this function returns the caller
/// should call `tui.restore()` (though the [`Drop`] impl on `Tui` is a safety
/// net).
///
/// # Errors
///
/// Returns any `io::Error` from terminal I/O.
pub async fn run(tui: &mut Tui, app: &mut App) -> std::io::Result<()> {
    // Subscribe to api events once at startup.
    let mut api_stream = app.api.subscribe();

    // Spawn a background task that drives crossterm's async EventStream and
    // forwards terminal events into an mpsc channel.  Using a channel here
    // lets us use `tokio::select!` cleanly without holding a `!Send` future
    // from EventStream across await points.
    let (term_tx, mut term_rx) = mpsc::channel::<CrosstermEvent>(64);
    tokio::spawn(async move {
        let mut stream = EventStream::new();
        while let Some(Ok(ev)) = stream.next().await {
            if term_tx.send(ev).await.is_err() {
                break; // receiver dropped → main loop exited
            }
        }
    });

    // Periodic tick timer.
    let mut ticker = time::interval(TICK_INTERVAL);

    // Initial render.
    tui.draw(|frame| ui::render(app, frame))?;

    loop {
        let app_event: Option<AppEvent> = tokio::select! {
            // Bias toward terminal input (lower latency for keystrokes).
            biased;

            maybe_term = term_rx.recv() => {
                maybe_term.map(translate_terminal_event)
            }

            maybe_api = api_stream.next() => {
                match maybe_api {
                    Some(ev) => Some(AppEvent::ApiEvent(ev)),
                    // api stream ended → orchestrator shut down; quit cleanly.
                    None => Some(AppEvent::Quit),
                }
            }

            _ = ticker.tick() => {
                Some(AppEvent::Tick)
            }
        };

        if let Some(event) = app_event {
            let needs_redraw = app.update(event);
            if app.should_quit {
                break;
            }
            if needs_redraw {
                tui.draw(|frame| ui::render(app, frame))?;
            }
        }
    }

    Ok(())
}

// ── Translation helpers ───────────────────────────────────────────────────────

/// Translate a raw crossterm [`CrosstermEvent`] into an [`AppEvent`].
///
/// Returns `None` for events the scaffold doesn't handle yet (e.g. mouse
/// events); those are silently dropped.
fn translate_terminal_event(ev: CrosstermEvent) -> AppEvent {
    match ev {
        // Quit keys
        CrosstermEvent::Key(key) => translate_key(key),
        CrosstermEvent::Resize(w, h) => AppEvent::Resize(w, h),
        // Mouse, paste, focus, etc. — ignored for now (task 31 may handle some).
        _ => AppEvent::Tick,
    }
}

fn translate_key(key: crossterm::event::KeyEvent) -> AppEvent {
    use crossterm::event::KeyEventKind;
    // Only react to key-press events (not key-release / repeat on some platforms).
    if key.kind != KeyEventKind::Press {
        return AppEvent::Tick;
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => AppEvent::Quit,
        KeyCode::Esc => AppEvent::Quit,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => AppEvent::Quit,
        KeyCode::Tab => AppEvent::FocusNext,
        _ => AppEvent::Tick,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn key_press(code: KeyCode, modifiers: KeyModifiers) -> CrosstermEvent {
        CrosstermEvent::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    #[test]
    fn q_key_translates_to_quit() {
        let ev = key_press(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(matches!(translate_terminal_event(ev), AppEvent::Quit));
    }

    #[test]
    fn esc_key_translates_to_quit() {
        let ev = key_press(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(translate_terminal_event(ev), AppEvent::Quit));
    }

    #[test]
    fn ctrl_c_translates_to_quit() {
        let ev = key_press(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(translate_terminal_event(ev), AppEvent::Quit));
    }

    #[test]
    fn tab_translates_to_focus_next() {
        let ev = key_press(KeyCode::Tab, KeyModifiers::NONE);
        assert!(matches!(translate_terminal_event(ev), AppEvent::FocusNext));
    }

    #[test]
    fn key_release_is_tick_not_quit() {
        // On some platforms crossterm fires key-release events; they must be
        // ignored (treated as Tick, not as Quit).
        let ev = CrosstermEvent::Key(KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        });
        assert!(matches!(translate_terminal_event(ev), AppEvent::Tick));
    }

    #[test]
    fn resize_translates_to_resize_event() {
        let ev = CrosstermEvent::Resize(120, 40);
        assert!(matches!(
            translate_terminal_event(ev),
            AppEvent::Resize(120, 40)
        ));
    }

    /// Verify the full quit path: translate key → update App → should_quit.
    #[test]
    fn quit_key_drives_app_to_should_quit() {
        use crate::app::App;
        use crate::placeholder::PlaceholderApi;
        use std::sync::Arc;

        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![]);

        let ev = translate_terminal_event(key_press(KeyCode::Char('q'), KeyModifiers::NONE));
        app.update(ev);
        assert!(app.should_quit);
    }

    /// Verify that an api event flows correctly: api::Event → AppEvent::ApiEvent → App state.
    #[test]
    fn api_event_flows_to_app_state() {
        use crate::app::App;
        use crate::placeholder::PlaceholderApi;
        use makina_core::api::{Event as CoreEvent, RunId, RunStatus};
        use std::sync::Arc;

        let api = Arc::new(PlaceholderApi::empty());
        let mut app = App::new(api, vec![]);

        let core_ev = CoreEvent::RunOpened {
            run: RunId(42),
            task_list_path: std::path::PathBuf::from(".tasks/flow.json"),
        };
        let app_ev = AppEvent::ApiEvent(core_ev);
        app.update(app_ev);

        assert_eq!(app.runs.len(), 1);
        assert_eq!(app.runs[0].id, RunId(42));
        assert_eq!(app.runs[0].status, RunStatus::Pending);
    }
}
