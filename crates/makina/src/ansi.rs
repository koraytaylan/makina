//! ANSI escape-sequence parsing for the TUI.
//!
//! Agent backends and gate commands frequently emit ANSI-colored output
//! (`\x1b[32m…\x1b[0m`).  ratatui renders [`ratatui::text::Span`]s with an
//! explicit [`ratatui::style::Style`] rather than raw escape codes, so before
//! such text can be displayed it must be parsed into styled runs.
//!
//! [`parse_ansi`] performs that conversion: it walks the input, interpreting
//! **SGR** (Select Graphic Rendition, `CSI … m`) sequences as style changes and
//! stripping every other control sequence (e.g. cursor moves `\x1b[H`, screen
//! clears `\x1b[2J`) so no literal `\x1b` byte ever reaches the terminal.
//!
//! The SGR subset is intentionally small — only the parameters the orchestrated
//! tools actually emit — which keeps the parser hand-rolled and dependency-free.
//! It mirrors the [`Color`]/[`Modifier`] usage already present in `ui.rs`.

use ratatui::style::{Color, Modifier, Style};

/// A contiguous run of text sharing a single [`Style`], produced by
/// [`parse_ansi`].
///
/// `text` never contains a literal escape (`\x1b`) byte: SGR sequences are
/// folded into `style` and all other control sequences are stripped.
#[derive(Debug, Clone, PartialEq)]
pub struct AnsiSpan {
    pub text: String,
    pub style: ratatui::style::Style,
}

/// Parse `input` into a sequence of styled [`AnsiSpan`]s.
///
/// SGR sequences (`\x1b[…m`) update the running style; supported parameters are
/// `0` (reset), `1` (bold), `31` (red foreground) and `32` (green foreground).
/// All other escape sequences (cursor moves, screen clears, …) are stripped.
/// Empty runs are not emitted, so the result contains no zero-length spans.
pub fn parse_ansi(input: &str) -> Vec<AnsiSpan> {
    let mut spans: Vec<AnsiSpan> = Vec::new();
    let mut current = String::new();
    let mut style = Style::default();

    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            current.push(c);
            continue;
        }

        // We hit an escape.  A CSI sequence is `\x1b[` followed by parameter
        // bytes and a final byte in the `0x40..=0x7e` range.  Anything else
        // (a lone ESC, or `\x1b` + non-`[`) is treated as a one-off escape and
        // simply dropped along with the next byte.
        if chars.peek() != Some(&'[') {
            // Drop the (at most one) following byte of a non-CSI escape.
            chars.next();
            continue;
        }
        chars.next(); // consume '['

        // Collect the CSI body up to and including its final byte.
        let mut params = String::new();
        let mut final_byte = None;
        for b in chars.by_ref() {
            if ('\u{40}'..='\u{7e}').contains(&b) {
                final_byte = Some(b);
                break;
            }
            params.push(b);
        }

        // Only SGR (`m`) sequences affect style; everything else is stripped.
        if final_byte == Some('m') {
            // A style change starts a new span: flush whatever we have so far.
            if !current.is_empty() {
                spans.push(AnsiSpan {
                    text: std::mem::take(&mut current),
                    style,
                });
            }
            style = apply_sgr(style, &params);
        }
    }

    if !current.is_empty() {
        spans.push(AnsiSpan {
            text: current,
            style,
        });
    }

    spans
}

/// Fold the semicolon-separated SGR parameters in `params` into `style`.
///
/// An empty parameter string (bare `\x1b[m`) is treated as a reset, matching
/// terminal behaviour.
fn apply_sgr(mut style: Style, params: &str) -> Style {
    if params.is_empty() {
        return Style::default();
    }
    for part in params.split(';') {
        match part {
            "0" | "" => style = Style::default(),
            "1" => style = style.add_modifier(Modifier::BOLD),
            "31" => style = style.fg(Color::Red),
            "32" => style = style.fg(Color::Green),
            // Unsupported parameters are ignored, not surfaced as text.
            _ => {}
        }
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn green_sgr_sets_green_foreground() {
        let spans = parse_ansi("\x1b[32mhello");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "hello");
        assert_eq!(spans[0].style.fg, Some(Color::Green));
    }

    #[test]
    fn red_sgr_sets_red_foreground() {
        let spans = parse_ansi("\x1b[31merror");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "error");
        assert_eq!(spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn bold_sgr_sets_bold_modifier() {
        let spans = parse_ansi("\x1b[1mloud");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "loud");
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn reset_sgr_returns_to_default_style() {
        let spans = parse_ansi("\x1b[32mgreen\x1b[0mplain");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, "green");
        assert_eq!(spans[0].style.fg, Some(Color::Green));
        assert_eq!(spans[1].text, "plain");
        assert_eq!(spans[1].style, Style::default());
    }

    #[test]
    fn non_sgr_sequences_are_stripped() {
        // `\x1b[2J` (clear screen) and `\x1b[H` (cursor home) carry no style and
        // must be removed entirely — no literal escape byte may survive.
        let spans = parse_ansi("\x1b[2Jbefore\x1b[Hafter");
        let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, "beforeafter");
        assert!(
            spans.iter().all(|s| !s.text.contains('\x1b')),
            "no AnsiSpan.text may contain a literal escape byte"
        );
    }
}
