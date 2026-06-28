//! Syntax highlighting for code blocks using syntect (via two-face).
//!
//! This module provides cached syntax highlighting that converts source code lines
//! into colored ratatui Spans. The syntax/theme sets are loaded once and cached
//! in OnceLocks to avoid expensive initialization on every code block.

use ratatui::style::{Color, Style};
use ratatui::text::Span;
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::parsing::SyntaxSet;

/// Cache for the syntax set (loaded once from two-face).
static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();

/// Cache for the lazy theme set (loaded once from two-face).
static LAZY_THEME_SET: OnceLock<two_face::theme::EmbeddedLazyThemeSet> = OnceLock::new();

/// Get or initialize the syntax set (loaded from two-face's bundled assets).
fn get_syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(two_face::syntax::extra_newlines)
}

/// Get or initialize the lazy theme set (loaded from two-face's bundled assets).
fn get_lazy_theme_set() -> &'static two_face::theme::EmbeddedLazyThemeSet {
    LAZY_THEME_SET.get_or_init(two_face::theme::extra)
}

/// Calculate the relative luminance of an RGB color using the standard WCAG formula.
/// Returns a value in [0.0, 1.0] where 0 = black, 1 = white.
fn luminance(r: u8, g: u8, b: u8) -> f64 {
    let to_linear = |c: u8| {
        let c = c as f64 / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let r_lin = to_linear(r);
    let g_lin = to_linear(g);
    let b_lin = to_linear(b);
    0.2126 * r_lin + 0.7152 * g_lin + 0.0722 * b_lin
}

/// Highlight a single line of code using syntect, returning a vector of colored spans.
///
/// # Arguments
/// * `line` - The source code line to highlight
/// * `lang` - Optional language identifier (e.g., "rust", "python"). If None or
///   the language is not found, a monochrome fallback is used.
/// * `theme` - The active theme, used to determine light/dark and the fallback color
///
/// # Returns
/// A vector of ratatui Spans with foreground colors mapped from syntect highlighting.
/// For known languages, returns multiple spans with distinct colors per token.
/// For unknown languages or errors, returns a single monochrome span using the CodeBlock role.
pub fn highlight_code_line(
    line: &str,
    lang: Option<&str>,
    theme: &crate::theme::Theme,
) -> Vec<Span<'static>> {
    let syntax_set = get_syntax_set();
    let lazy_theme_set = get_lazy_theme_set();

    // Determine light or dark theme by luminance of Background color
    let bg_color = theme.get(crate::theme::ThemeRole::Background);
    let is_dark = if let Color::Rgb(r, g, b) = bg_color {
        luminance(r, g, b) < 0.5
    } else {
        true // default to dark
    };

    // Select the syntect theme based on light/dark
    let theme_name = if is_dark {
        two_face::theme::EmbeddedThemeName::Dracula
    } else {
        two_face::theme::EmbeddedThemeName::OneHalfLight
    };

    let syntect_theme = lazy_theme_set.get(theme_name);

    // Try to find the syntax definition by language token (only if lang was provided)
    let syntax = if let Some(l) = lang {
        match syntax_set.find_syntax_by_token(l) {
            Some(s) => s,
            None => {
                // Unknown language: return monochrome fallback
                let fallback_color = theme.get(crate::theme::ThemeRole::CodeBlock);
                return vec![Span::styled(
                    line.to_string(),
                    Style::default().fg(fallback_color),
                )];
            }
        }
    } else {
        // No language specified: return monochrome fallback
        let fallback_color = theme.get(crate::theme::ThemeRole::CodeBlock);
        return vec![Span::styled(
            line.to_string(),
            Style::default().fg(fallback_color),
        )];
    };

    // Highlight the line with the found syntax
    let mut highlighter = HighlightLines::new(syntax, syntect_theme);
    let ranges = match highlighter.highlight_line(line, syntax_set) {
        Ok(r) => r,
        Err(_) => {
            // On highlight error, return monochrome fallback
            let fallback_color = theme.get(crate::theme::ThemeRole::CodeBlock);
            return vec![Span::styled(
                line.to_string(),
                Style::default().fg(fallback_color),
            )];
        }
    };

    // Convert syntect highlighting to ratatui spans
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (syntect_style, text) in ranges {
        let color = Color::Rgb(
            syntect_style.foreground.r,
            syntect_style.foreground.g,
            syntect_style.foreground.b,
        );
        let style = Style::default().fg(color);
        spans.push(Span::styled(text.to_string(), style));
    }

    // If no spans were produced (empty line), return a single empty span
    if spans.is_empty() {
        let fallback_color = theme.get(crate::theme::ThemeRole::CodeBlock);
        spans.push(Span::styled(
            String::new(),
            Style::default().fg(fallback_color),
        ));
    }

    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_highlight_rust_produces_multiple_spans() {
        let theme = crate::theme::ayu_dark();
        let spans = highlight_code_line("let x = 1;", Some("rust"), &theme);

        // Rust code should produce multiple colored spans (keyword, identifier, number, etc.)
        assert!(
            spans.len() > 1,
            "Expected multiple spans for Rust highlighting, got {}",
            spans.len()
        );

        // Verify each span has content and a style with a foreground color
        for span in &spans {
            let has_content = !span.content.is_empty();
            let has_foreground = matches!(span.style.fg, Some(Color::Rgb(_, _, _)));
            assert!(
                has_content || has_foreground,
                "Span should have content or foreground color"
            );
        }
    }

    #[test]
    fn test_unknown_language_returns_monochrome_span() {
        let theme = crate::theme::ayu_dark();
        let spans = highlight_code_line("let x = 1;", Some("unknown_lang_xyz"), &theme);

        // Unknown language should fall back to a single monochrome span
        assert_eq!(
            spans.len(),
            1,
            "Unknown language should produce a single span, got {}",
            spans.len()
        );

        // The span should use the CodeBlock foreground color
        let expected_color = theme.get(crate::theme::ThemeRole::CodeBlock);
        assert_eq!(
            spans[0].style.fg,
            Some(expected_color),
            "Unknown language fallback should use CodeBlock color"
        );
    }

    #[test]
    fn test_none_language_returns_monochrome_span() {
        let theme = crate::theme::ayu_light();
        let spans = highlight_code_line("let x = 1;", None, &theme);

        // None language should fall back to a single monochrome span
        assert_eq!(
            spans.len(),
            1,
            "None language should produce a single span, got {}",
            spans.len()
        );

        // The span should use the CodeBlock foreground color
        let expected_color = theme.get(crate::theme::ThemeRole::CodeBlock);
        assert_eq!(
            spans[0].style.fg,
            Some(expected_color),
            "None language fallback should use CodeBlock color"
        );
    }

    #[test]
    fn test_empty_line_returns_span() {
        let theme = crate::theme::ayu_mirage();
        let spans = highlight_code_line("", Some("rust"), &theme);

        // Even empty line should produce at least one span (possibly empty content)
        assert!(
            !spans.is_empty(),
            "Empty line should produce at least one span"
        );
    }

    #[test]
    fn test_luminance_calculation() {
        // Black should have luminance near 0
        assert!(luminance(0, 0, 0) < 0.1);

        // White should have luminance near 1
        assert!(luminance(255, 255, 255) > 0.9);

        // Mid-gray should be around 0.5
        let gray_lum = luminance(128, 128, 128);
        assert!(gray_lum > 0.2 && gray_lum < 0.8);
    }

    #[test]
    fn test_dark_theme_selection() {
        let theme = crate::theme::ayu_dark(); // Background: #0D1017
        let spans = highlight_code_line("let x = 1;", Some("rust"), &theme);

        // Dark theme background should result in multiple spans for rust
        assert!(spans.len() > 1);
    }

    #[test]
    fn test_light_theme_selection() {
        let theme = crate::theme::ayu_light(); // Background: #F8F9FA
        let spans = highlight_code_line("let x = 1;", Some("rust"), &theme);

        // Light theme background should result in multiple spans for rust
        assert!(spans.len() > 1);
    }

    #[test]
    fn test_syntax_set_cached() {
        // Call get_syntax_set twice and verify we get the same instance (cached)
        let set1 = get_syntax_set();
        let set2 = get_syntax_set();
        assert_eq!(
            set1 as *const _, set2 as *const _,
            "SyntaxSet should be cached"
        );
    }

    #[test]
    fn test_theme_set_cached() {
        // Call get_lazy_theme_set twice and verify we get the same instance (cached)
        let set1 = get_lazy_theme_set();
        let set2 = get_lazy_theme_set();
        assert_eq!(
            set1 as *const _, set2 as *const _,
            "Lazy ThemeSet should be cached"
        );
    }
}
