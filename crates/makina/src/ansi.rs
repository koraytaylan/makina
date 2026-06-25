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
//! The SGR subset covers reset/bold plus the 16 standard ANSI colors, which the
//! orchestrated tools actually emit; each color resolves through the active
//! theme's palette rather than a hardcoded value. It mirrors the [`Style`]/
//! [`Modifier`] usage already present in `ui.rs`.

use ratatui::style::{Modifier, Style};

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
/// `0` (reset), `1` (bold), `39`/`49` (reset foreground/background), the eight
/// normal colors (`30`–`37` foreground, `40`–`47` background) and their bright
/// variants (`90`–`97`, `100`–`107`) — each resolved through the active theme's
/// 16-entry ANSI palette. All other escape sequences (cursor moves, screen
/// clears, …) are stripped. Empty runs are not emitted, so the result contains
/// no zero-length spans.
pub fn parse_ansi(input: &str, theme: &crate::theme::Theme) -> Vec<AnsiSpan> {
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
            style = apply_sgr(style, &params, theme);
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
fn apply_sgr(mut style: Style, params: &str, theme: &crate::theme::Theme) -> Style {
    if params.is_empty() {
        return Style::default();
    }
    for part in params.split(';') {
        if let Ok(n) = part.parse::<usize>() {
            match n {
                0 | 39 | 49 => {
                    if n == 0 {
                        style = Style::default();
                    } else if n == 39 {
                        // Reset foreground to default
                        style = style.fg(theme.get(crate::theme::ThemeRole::Foreground));
                    } else if n == 49 {
                        // Reset background to default
                        style = style.bg(theme.get(crate::theme::ThemeRole::Background));
                    }
                }
                1 => style = style.add_modifier(Modifier::BOLD),
                // Foreground colors: 30-37 (normal) and 90-97 (bright)
                30..=37 => style = style.fg(theme.ansi(n - 30)),
                90..=97 => style = style.fg(theme.ansi(8 + (n - 90))),
                // Background colors: 40-47 (normal) and 100-107 (bright)
                40..=47 => style = style.bg(theme.ansi(n - 40)),
                100..=107 => style = style.bg(theme.ansi(8 + (n - 100))),
                // Unsupported parameters (256-color, truecolor, etc) are ignored
                _ => {}
            }
        }
    }
    style
}

/// Return the base [`Style`] for a unified-diff `line`, based on its prefix.
///
/// The line text is *never* modified — only its leading character(s) are
/// inspected — so indentation and column alignment are preserved by
/// construction.  The mapping is:
///
/// * a line starting with `+` (but not the `+++` file header) → Success;
/// * a line starting with `-` (but not the `---` file header) → Error;
/// * a line starting with `@@` (a hunk header) → Info;
/// * anything else (context lines, `+++`/`---` file headers) → [`None`].
///
/// The returned style carries only a foreground colour, acting as a *base*
/// layer: consumers overlay [`parse_ansi`] SGR spans on top, so embedded ANSI
/// styling wins where present and this diff colour applies otherwise.
pub fn diff_line_style(line: &str, theme: &crate::theme::Theme) -> Option<Style> {
    if line.starts_with("@@") {
        Some(Style::default().fg(theme.get(crate::theme::ThemeRole::Info)))
    } else if line.starts_with("+++") || line.starts_with("---") {
        // File headers are not added/removed lines.
        None
    } else if line.starts_with('+') {
        Some(Style::default().fg(theme.get(crate::theme::ThemeRole::Success)))
    } else if line.starts_with('-') {
        Some(Style::default().fg(theme.get(crate::theme::ThemeRole::Error)))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn green_sgr_sets_green_foreground() {
        let th = crate::theme::ayu_dark();
        let spans = parse_ansi("\x1b[32mhello", &th);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "hello");
        assert_eq!(spans[0].style.fg, Some(th.ansi(2)));
    }

    #[test]
    fn red_sgr_sets_red_foreground() {
        let th = crate::theme::ayu_dark();
        let spans = parse_ansi("\x1b[31merror", &th);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "error");
        assert_eq!(spans[0].style.fg, Some(th.ansi(1)));
    }

    #[test]
    fn bold_sgr_sets_bold_modifier() {
        let th = crate::theme::ayu_dark();
        let spans = parse_ansi("\x1b[1mloud", &th);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "loud");
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn reset_sgr_returns_to_default_style() {
        let th = crate::theme::ayu_dark();
        let spans = parse_ansi("\x1b[32mgreen\x1b[0mplain", &th);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, "green");
        assert_eq!(spans[0].style.fg, Some(th.ansi(2)));
        assert_eq!(spans[1].text, "plain");
        assert_eq!(spans[1].style, Style::default());
    }

    #[test]
    fn non_sgr_sequences_are_stripped() {
        let th = crate::theme::ayu_dark();
        // `\x1b[2J` (clear screen) and `\x1b[H` (cursor home) carry no style and
        // must be removed entirely — no literal escape byte may survive.
        let spans = parse_ansi("\x1b[2Jbefore\x1b[Hafter", &th);
        let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, "beforeafter");
        assert!(
            spans.iter().all(|s| !s.text.contains('\x1b')),
            "no AnsiSpan.text may contain a literal escape byte"
        );
    }

    #[test]
    fn diff_line_style_colors_prefixes() {
        let th = crate::theme::ayu_dark();
        assert_eq!(
            diff_line_style("+added", &th).unwrap().fg,
            Some(th.get(crate::theme::ThemeRole::Success))
        );
        assert_eq!(
            diff_line_style("-removed", &th).unwrap().fg,
            Some(th.get(crate::theme::ThemeRole::Error))
        );
        assert_eq!(
            diff_line_style("@@ -1 +1 @@", &th).unwrap().fg,
            Some(th.get(crate::theme::ThemeRole::Info))
        );
        assert_eq!(diff_line_style("  context", &th), None);
        // `+++`/`---` are file headers, not added/removed lines.
        assert_eq!(diff_line_style("+++ b/file", &th), None);
    }
}
