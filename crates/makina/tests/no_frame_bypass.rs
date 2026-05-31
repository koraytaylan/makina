//! Guard test for `tui-error-pane-no-frame-bypass`.
//!
//! Asserts that no `eprintln!`/`println!` (an stdout/stderr frame-bypass) appears
//! inside the **live-frame region** of the event loop — the source modules that
//! run while a ratatui frame is live during [`makina::event::run`]. Any
//! in-frame system error must instead flow through `tracing::error!`/
//! `tracing::warn!` → the tracing TUI layer → the error pane
//! (`tui-error-pane-channel-wire`), so nothing writes outside the frame and
//! corrupts the alternate screen.
//!
//! `main.rs` is handled separately: it is allowed to contain EXACTLY the four
//! fatal `eprintln!` sites that are intentionally exempt because none runs while
//! a live ratatui frame exists — the config-load failure (pre-`Tui::init()`),
//! the planner-mechanism fallback notice (pre-`Tui::init()`), the terminal-init
//! failure itself, and the post-`tui.restore()` error print.  The test pins the
//! count to four so a NEW in-frame `eprintln!` added to `main.rs` (e.g. between
//! `Tui::init()` and `tui.restore()`) trips the guard.

use std::path::PathBuf;

/// Source files reachable while a ratatui frame is live during `event::run`.
/// These form the live-frame region; a stdout/stderr write here would corrupt
/// the alternate screen, so none may contain `eprintln!`/`println!`.
const LIVE_FRAME_REGION: &[&str] = &["event.rs", "app.rs", "ui.rs", "tui.rs", "browser.rs"];

/// `main.rs` is exempt but pinned: exactly these four FATAL sites may print,
/// and only because each runs with NO live ratatui frame.
const MAIN_RS_EXEMPT_PRINT_COUNT: usize = 4;

fn src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Counts `eprintln!`/`println!` macro invocations in a source string. Skips
/// `//` line comments and `//!`/`///` doc comments so prose mentioning the
/// macros (this very crate documents the exempt sites) does not false-positive.
fn count_print_macros(source: &str) -> usize {
    let mut count = 0;
    for line in source.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        // Strip any trailing `//` line comment to avoid counting commented-out
        // examples or trailing explanations.
        let code = trimmed.split("//").next().unwrap_or(trimmed);
        let eprintln = code.matches("eprintln!").count();
        // `eprintln!` literally contains the substring `println!`, so subtract
        // the `eprintln!` hits from the raw `println!` matches to avoid
        // double-counting every `eprintln!` as both.
        let println = code.matches("println!").count() - eprintln;
        count += eprintln + println;
    }
    count
}

#[test]
fn no_print_macros_in_live_frame_region() {
    for file in LIVE_FRAME_REGION {
        let path = src_dir().join(file);
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let count = count_print_macros(&source);
        assert_eq!(
            count, 0,
            "live-frame region file {file} contains {count} eprintln!/println! \
             frame-bypass call(s); in-frame system errors must use \
             tracing::error!/tracing::warn! so they flow into the error pane \
             instead of corrupting the ratatui alternate screen",
        );
    }
}

#[test]
fn main_rs_has_exactly_the_three_exempt_print_sites() {
    let path = src_dir().join("main.rs");
    let source =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let count = count_print_macros(&source);
    assert_eq!(
        count, MAIN_RS_EXEMPT_PRINT_COUNT,
        "main.rs must contain EXACTLY the {MAIN_RS_EXEMPT_PRINT_COUNT} fatal, \
         frame-exempt eprintln! sites (config-load failure before Tui::init(), \
         planner-mechanism fallback before Tui::init(), terminal-init failure, \
         and the post-tui.restore() error print); found {count}. A new print \
         here likely means an in-frame error is bypassing the error pane — \
         route it through tracing::error! instead.",
    );
}
