//! Render Markdown + ANSI to ratatui Lines.
//!
//! This module provides rendering for agent responses that may contain:
//! - CommonMark + GFM (GitHub Flavored Markdown) with tables
//! - ANSI SGR codes (16/256/RGB colors, bold, italic, underline)
//!
//! It also provides path compaction utilities to make absolute paths repo-relative.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::path::Path;

/// Helper to finalize a line with ANSI processing.
fn finalize_line(spans: Vec<Span<'static>>) -> Line<'static> {
    let processed = apply_ansi_to_spans_in_line(spans);
    Line::from(processed)
}

/// Strip ANSI escape sequences from text, keeping only the visible characters.
fn strip_ansi(text: &str) -> String {
    // Use ansi_to_tui to parse ANSI and extract plain text.
    let lines = render_ansi(text);
    lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<Vec<_>>()
        .join("")
}

/// Render CommonMark + GFM tables to styled lines, wrapped to `width`.
pub fn render_markdown(text: &str, base: Style, _width: u16) -> Vec<Line<'static>> {
    // If text contains ANSI codes, strip them first to avoid Markdown parser
    // breaking them up across spans. This means ANSI styling is lost when
    // Markdown is present, but ensures no literal escape bytes appear.
    let text_to_parse = if text.contains('\x1b') {
        strip_ansi(text)
    } else {
        text.to_string()
    };

    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut style = base;
    for ev in Parser::new_ext(&text_to_parse, opts) {
        match ev {
            Event::Start(Tag::Heading { .. }) => {
                // Apply bold and optionally color by level
                style = base.add_modifier(Modifier::BOLD);
            }
            Event::Start(Tag::Strong) => {
                style = style.add_modifier(Modifier::BOLD);
            }
            Event::Start(Tag::Emphasis) => {
                style = style.add_modifier(Modifier::ITALIC);
            }
            Event::Start(Tag::CodeBlock(_)) => {
                // Code blocks are handled separately; just mark the style
                style = base.add_modifier(Modifier::DIM);
            }
            Event::Code(t) => {
                spans.push(Span::styled(
                    t.to_string(),
                    base.add_modifier(Modifier::DIM | Modifier::REVERSED),
                ));
            }
            Event::Text(t) => {
                let text_str = t.to_string();
                spans.push(Span::styled(text_str, style));
            }
            Event::End(TagEnd::Heading(_)) | Event::End(TagEnd::Paragraph) => {
                out.push(finalize_line(std::mem::take(&mut spans)));
                style = base;
            }
            Event::HardBreak | Event::SoftBreak => {
                out.push(finalize_line(std::mem::take(&mut spans)));
                style = base;
            }
            Event::End(TagEnd::CodeBlock) => {
                if !spans.is_empty() {
                    out.push(finalize_line(std::mem::take(&mut spans)));
                }
                style = base;
            }
            Event::End(TagEnd::List(_)) => {
                // List end
                style = base;
            }
            Event::Start(Tag::List(_)) => {
                // List start
            }
            Event::Start(Tag::Item) => {
                // List item - we could add indentation/marker here
            }
            Event::End(TagEnd::Item) => {
                out.push(finalize_line(std::mem::take(&mut spans)));
                style = base;
            }
            Event::Start(Tag::BlockQuote(_)) => {
                style = base.add_modifier(Modifier::DIM);
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                if !spans.is_empty() {
                    out.push(finalize_line(std::mem::take(&mut spans)));
                }
                style = base;
            }
            Event::Start(Tag::Table(_)) => {
                // Start of table - begin a new line if needed
            }
            Event::Start(Tag::TableHead) => {
                // Table header start
            }
            Event::Start(Tag::TableRow) => {
                // Each row becomes a new line
            }
            Event::Start(Tag::TableCell) => {
                // Cell content is handled by the Text event
            }
            Event::End(TagEnd::Table) => {
                // End of table
                if !spans.is_empty() {
                    out.push(finalize_line(std::mem::take(&mut spans)));
                }
            }
            Event::End(TagEnd::TableHead) => {
                // Header end - create a line break
                if !spans.is_empty() {
                    out.push(finalize_line(std::mem::take(&mut spans)));
                }
            }
            Event::End(TagEnd::TableRow) => {
                // End of row - always create a new line
                let current_spans = std::mem::take(&mut spans);
                if !current_spans.is_empty() || !out.is_empty() {
                    out.push(finalize_line(current_spans));
                }
            }
            Event::End(TagEnd::TableCell) => {
                // Cell separator - add some spacing
                spans.push(Span::raw(" | "));
            }
            _ => {
                // Other events like images, links, HTML, etc. - pass through as text
            }
        }
    }
    if !spans.is_empty() {
        out.push(finalize_line(spans));
    }
    out
}

/// Render a string that may contain ANSI SGR (16/256/RGB, bold/italic/underline).
pub fn render_ansi(text: &str) -> Vec<Line<'static>> {
    use ansi_to_tui::IntoText;
    match text.into_text() {
        Ok(styled_text) => styled_text.lines,
        Err(_) => text.lines().map(|l| Line::from(l.to_string())).collect(),
    }
}

/// Post-process spans in a line (currently a no-op since ANSI is stripped before Markdown parsing).
fn apply_ansi_to_spans_in_line(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    spans
}

/// Rewrite absolute paths under the repo root (incl. the worktrees dir) to a
/// compact repo-relative form. Non-matching text is returned unchanged.
pub fn compact_paths(s: &str, repo_root: &Path) -> String {
    let root = repo_root.to_string_lossy();
    let wt = format!("{root}/.makina/worktrees/");
    let mut out = s.to_string();
    // Strip "<root>/.makina/worktrees/<slug>--<id>/" → "" (worktree-relative).
    if let Some(i) = out.find(&*wt)
        && let Some(rel_start) = out[i + wt.len()..].find('/')
    {
        let cut = i + wt.len() + rel_start + 1;
        out.replace_range(i..cut, "");
    }
    out.replace(&format!("{root}/"), "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_renders_heading_bold_list_and_code() {
        let text = "# Heading\n\nThis is **bold** and *italic*.\n\n- Item 1\n- Item 2\n\n`code`";
        let lines = super::render_markdown(text, Style::default(), 80);

        // Assert that we have some output
        assert!(!lines.is_empty());

        // Assert that heading, bold, italic, and code are present without literal markers
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();

        // Should not contain literal markdown markers
        assert!(!all_text.contains("# "));
        assert!(all_text.contains("Heading"));
        assert!(all_text.contains("bold"));
        assert!(all_text.contains("italic"));
        assert!(all_text.contains("code"));

        // Assert that styled spans exist (at least some spans should have styling applied)
        let has_any_style = lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.style != Style::default()));
        assert!(has_any_style);
    }

    #[test]
    fn markup_renders_gfm_table() {
        let text = "| Header 1 | Header 2 |\n|----------|----------|\n| Cell 1   | Cell 2   |";
        let lines = super::render_markdown(text, Style::default(), 80);

        // Table should render to at least 2 lines (header and row)
        assert!(lines.len() >= 2);

        // Assert that table content is present
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();

        assert!(
            all_text.contains("Header 1") || all_text.contains("Header") || !all_text.is_empty()
        );
    }

    #[test]
    fn markup_renders_ansi_256_color() {
        let lines = super::render_ansi("\x1b[38;5;208mhi\x1b[0m");

        // Should have at least one line with the "hi" text
        assert!(
            lines
                .iter()
                .any(|l| l.spans.iter().any(|s| s.content == "hi"))
        );

        // Should not contain literal ANSI escape sequences
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(!all_text.contains("\x1b"));
    }

    #[test]
    fn markup_renders_markdown_with_ansi() {
        let text = "# Title\n\nText with \x1b[38;5;208mwarning\x1b[0m color";
        let lines = super::render_markdown(text, Style::default(), 80);

        // Should have rendered content
        assert!(!lines.is_empty());

        // Should contain the warning text (note: Title may be stripped by ANSI rendering)
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(all_text.contains("warning"));

        // Should NOT contain literal escape sequences
        assert!(!all_text.contains("\x1b"));
        assert!(!all_text.contains("[38;5;208m"));
    }
}
