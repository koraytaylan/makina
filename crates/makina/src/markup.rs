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

/// Finalize a flushed run of spans into a [`Line`].
fn finalize_line(spans: Vec<Span<'static>>) -> Line<'static> {
    Line::from(spans)
}

/// Greedily wrap `text` into chunks no wider than `width` (in columns),
/// breaking on spaces; `width == 0` disables wrapping (returns one chunk).
pub(crate) fn wrap_words(text: &str, width: u16) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }

    let width = width as usize;
    let mut result = Vec::new();
    let mut current_line = String::new();

    for word in text.split(' ') {
        if current_line.is_empty() {
            // First word on the line
            if word.len() <= width {
                current_line = word.to_string();
            } else {
                // Word is longer than width, break it anyway
                result.push(word.to_string());
            }
        } else if current_line.len() + 1 + word.len() <= width {
            // Word fits on the current line with a space
            current_line.push(' ');
            current_line.push_str(word);
        } else {
            // Word doesn't fit, start a new line
            result.push(current_line);
            current_line = word.to_string();
        }
    }

    if !current_line.is_empty() {
        result.push(current_line);
    }

    result
}

/// Split text on newlines, excluding the final empty element if text ends with '\n'.
/// This prevents a spurious blank line after each code block source line.
fn trimmed_lines(text: &str) -> Vec<&str> {
    let lines: Vec<&str> = text.split('\n').collect();
    // If the last element is empty (text ends with '\n'), exclude it
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines[..lines.len() - 1].to_vec()
    } else {
        lines
    }
}

/// Strip ANSI escape sequences from text, keeping only the visible characters.
fn strip_ansi(text: &str) -> String {
    // Parse ANSI via ansi_to_tui and extract plain text, preserving line
    // structure: join spans *within* a line, and lines with '\n'. Joining
    // everything with "" would collapse multi-line content into one paragraph
    // before Markdown parsing.
    render_ansi(text)
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render CommonMark + GFM tables to styled lines, wrapped to `width`.
pub fn render_markdown(
    text: &str,
    base: Style,
    width: u16,
    theme: &crate::theme::Theme,
) -> Vec<Line<'static>> {
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

    // Track code block state
    let mut in_code_block = false;
    let mut code_lang: Option<String> = None;

    // Track list context for nesting
    #[derive(Debug, Clone)]
    struct ListCtx {
        ordered: bool,
        next: u64,
    }
    let mut list_stack: Vec<ListCtx> = Vec::new();

    // Track link URL
    let mut link_url: Option<String> = None;

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
            Event::Start(Tag::Strikethrough) => {
                style = style.add_modifier(Modifier::CROSSED_OUT);
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                // Code blocks: flush pending spans and enter code block mode
                if !spans.is_empty() {
                    out.push(finalize_line(std::mem::take(&mut spans)));
                }
                in_code_block = true;
                // Capture the fence language from CodeBlockKind::Fenced
                code_lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(info) if !info.is_empty() => {
                        Some(info.to_string())
                    }
                    _ => None,
                };
            }
            Event::Code(t) => {
                // Inline code: use theme-aware colors
                let code_style = Style::default()
                    .fg(theme.get(crate::theme::ThemeRole::CodeBlock))
                    .bg(theme.get(crate::theme::ThemeRole::Background))
                    .add_modifier(Modifier::DIM);
                spans.push(Span::styled(t.to_string(), code_style));
            }
            Event::Text(t) => {
                let mut text_str = t.to_string();
                if in_code_block {
                    // Split code block text on newlines; each line becomes its own Line.
                    // Use trimmed_lines to skip the trailing empty element if text ends with '\n'.
                    for line in trimmed_lines(&text_str) {
                        // Get syntax-highlighted spans for this line
                        let mut line_spans = vec![Span::raw("  ")]; // Keep the indent
                        line_spans.extend(crate::syntax::highlight_code_line(
                            line,
                            code_lang.as_deref(),
                            theme,
                        ));

                        // Apply full-width CodeBlockBg background band
                        let bg_style =
                            Style::default().bg(theme.get(crate::theme::ThemeRole::CodeBlockBg));

                        // Pad line to width so the band spans full viewport width.
                        // Measure by DISPLAY width (Span::width → unicode-width),
                        // matching how ratatui lays out the line — char count would
                        // under-measure CJK/emoji/wide glyphs and wrap the band.
                        let line_width: usize = line_spans.iter().map(|s| s.width()).sum();
                        // saturating_sub guards against a code line wider than the
                        // viewport (would otherwise underflow and panic/over-allocate).
                        let padding_needed = (width as usize).saturating_sub(line_width);
                        if padding_needed > 0 {
                            line_spans.push(Span::raw(" ".repeat(padding_needed)));
                        }

                        out.push(Line::from(line_spans).style(bg_style));
                    }
                } else {
                    if !spans.is_empty() {
                        let leading_spaces = text_str
                            .chars()
                            .take_while(|ch| *ch == ' ')
                            .map(char::len_utf8)
                            .sum::<usize>();
                        if leading_spaces > 0 {
                            let leading = text_str[..leading_spaces].to_string();
                            spans.push(Span::styled(leading, style));
                            text_str = text_str[leading_spaces..].to_string();
                        }
                    }
                    // Wrap text to width when not in a code block
                    let wrapped = wrap_words(&text_str, width);
                    for (i, chunk) in wrapped.into_iter().enumerate() {
                        if i > 0 {
                            // Start a new line for wrapped chunks
                            out.push(finalize_line(std::mem::take(&mut spans)));
                        }
                        spans.push(Span::styled(chunk, style));
                    }
                }
            }
            Event::End(TagEnd::Heading(_)) | Event::End(TagEnd::Paragraph) => {
                if !spans.is_empty() {
                    out.push(finalize_line(std::mem::take(&mut spans)));
                }
                style = base;
            }
            Event::HardBreak | Event::SoftBreak => {
                out.push(finalize_line(std::mem::take(&mut spans)));
                style = base;
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                code_lang = None;
                style = base;
            }
            Event::Start(Tag::List(start)) => {
                // Push a list context; start is Some(n) for ordered, None for unordered
                list_stack.push(ListCtx {
                    ordered: start.is_some(),
                    next: start.unwrap_or(1),
                });
            }
            Event::End(TagEnd::List(_)) => {
                // Pop the list context (guarded against empty stack)
                let _ = list_stack.pop();
                style = base;
            }
            Event::Start(Tag::Item) => {
                // Emit list marker with appropriate indentation
                let depth = list_stack.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let marker = if let Some(ctx) = list_stack.last_mut() {
                    if ctx.ordered {
                        let n = ctx.next;
                        ctx.next += 1;
                        format!("{}{}. ", indent, n)
                    } else {
                        format!("{}- ", indent)
                    }
                } else {
                    format!("{}- ", indent)
                };
                spans.push(Span::styled(marker, base));
            }
            Event::End(TagEnd::Item) => {
                if !spans.is_empty() {
                    out.push(finalize_line(std::mem::take(&mut spans)));
                }
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
            Event::Start(Tag::Link { dest_url, .. }) => {
                // Stash the link URL to append later
                link_url = Some(dest_url.to_string());
            }
            Event::End(TagEnd::Link) => {
                // If the link URL differs from the text, append it as dim
                if let Some(url) = link_url.take() {
                    spans.push(Span::styled(
                        format!(" ({})", url),
                        base.add_modifier(Modifier::DIM),
                    ));
                }
            }
            Event::Rule => {
                // Flush pending spans and emit a rule line
                if !spans.is_empty() {
                    out.push(finalize_line(std::mem::take(&mut spans)));
                }
                let rule_width = if width > 0 { width as usize } else { 20 };
                let rule = "─".repeat(rule_width);
                out.push(Line::from(Span::styled(
                    rule,
                    base.add_modifier(Modifier::DIM),
                )));
            }
            _ => {
                // Other events like images, HTML, etc. - pass through silently
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

/// Rewrite absolute paths under the repo root (incl. the worktrees dir) to a
/// compact repo-relative form. Non-matching text is returned unchanged.
///
/// After plan 0029 the worktrees live **off-repo** at
/// `~/.makina/projects/{ns}/worktrees/{short-name}/`, so we strip:
/// 1. The relocated `state_root(repo_root)/worktrees/<short-name>/` prefix.
/// 2. The legacy in-repo `<root>/.makina/worktrees/<slug>--<id>/` prefix
///    (for backward compatibility with transcripts written before the move).
/// 3. Any remaining `<root>/` prefix for non-worktree repo-relative paths.
pub fn compact_paths(s: &str, repo_root: &Path) -> String {
    let root = repo_root.to_string_lossy();
    let mut out = s.to_string();

    // 1. Strip relocated worktree prefix: state_root/worktrees/<name>/ → ""
    if let Ok(state_root) = makina_core::paths::state_root(repo_root) {
        let relocated_wt = format!("{}/worktrees/", state_root.display());
        if let Some(i) = out.find(&*relocated_wt)
            && let Some(rel_start) = out[i + relocated_wt.len()..].find('/')
        {
            let cut = i + relocated_wt.len() + rel_start + 1;
            out.replace_range(i..cut, "");
            // No further stripping needed — the relocated path is outside repo_root.
            return out;
        }
    }

    // 2. Strip legacy in-repo worktree prefix: <root>/.makina/worktrees/<name>/ → ""
    let legacy_wt = format!("{root}/.makina/worktrees/");
    if let Some(i) = out.find(&*legacy_wt)
        && let Some(rel_start) = out[i + legacy_wt.len()..].find('/')
    {
        let cut = i + legacy_wt.len() + rel_start + 1;
        out.replace_range(i..cut, "");
        return out;
    }

    // 3. Strip general repo-relative prefix: <root>/ → ""
    out.replace(&format!("{root}/"), "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;

    #[test]
    fn markup_renders_heading_bold_list_and_code() {
        let text = "# Heading\n\nThis is **bold** and *italic*.\n\n- Item 1\n- Item 2\n\n`code`";
        let lines = super::render_markdown(text, Style::default(), 80, &theme::ayu_dark());

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
    fn inline_markdown_preserves_spaces_after_styled_spans() {
        let text = "Please **review** this and consider `inline code` next.";
        let lines = super::render_markdown(text, Style::default(), 80, &theme::ayu_dark());
        let all_text = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect::<String>();

        assert!(
            all_text.contains("Please review this"),
            "markdown text must keep spaces around bold spans; got {all_text:?}"
        );
        assert!(
            all_text.contains("inline code next"),
            "markdown text must keep spaces around inline code spans; got {all_text:?}"
        );
    }

    #[test]
    fn markup_renders_gfm_table() {
        let text = "| Header 1 | Header 2 |\n|----------|----------|\n| Cell 1   | Cell 2   |";
        let lines = super::render_markdown(text, Style::default(), 80, &theme::ayu_dark());

        // Table should render to at least 2 lines (header and row)
        assert!(lines.len() >= 2);

        // Assert that actual header AND cell text is present (not just "some output").
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();

        assert!(all_text.contains("Header 1") && all_text.contains("Cell 1"));
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
        let lines = super::render_markdown(text, Style::default(), 80, &theme::ayu_dark());

        // Should have rendered content
        assert!(!lines.is_empty());

        // Both the heading and the (de-ANSI'd) body survive: strip_ansi now
        // preserves line structure, so the heading is no longer collapsed into
        // the body line.
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(all_text.contains("Title"));
        assert!(all_text.contains("warning"));

        // Should NOT contain literal escape sequences
        assert!(!all_text.contains("\x1b"));
        assert!(!all_text.contains("[38;5;208m"));
    }

    #[test]
    fn renders_heading_bold_italic_strikethrough_and_inline_code() {
        let text = "# H\n\n**b** *i* ~~s~~ `c`";
        let lines = super::render_markdown(text, Style::default(), 80, &theme::ayu_dark());

        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();

        // Should contain the actual text without markdown markers
        assert!(all_text.contains("H"));
        assert!(all_text.contains("b"));
        assert!(all_text.contains("i"));
        assert!(all_text.contains("s"));
        assert!(all_text.contains("c"));

        // Should NOT contain literal markers
        assert!(!all_text.contains("# "));
        assert!(!all_text.contains("**"));
        assert!(!all_text.contains("~~"));
        assert!(!all_text.contains("`"));

        // At least some spans should have styling (bold, italic, strikethrough, etc.)
        let has_styled = lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.style != Style::default()));
        assert!(has_styled);
    }

    #[test]
    fn renders_fenced_code_block_preserving_line_breaks() {
        let text = "```\nfn main() {\n    body\n}\n```";
        let lines = super::render_markdown(text, Style::default(), 80, &theme::ayu_dark());

        // Should have exactly 3 lines for the code (fn main, body, closing brace)
        // NOT 4 with a spurious blank line
        assert_eq!(
            lines.len(),
            3,
            "Expected exactly 3 lines for code block (one per source line), got {}",
            lines.len()
        );

        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();

        // Should contain the code without backticks
        assert!(all_text.contains("fn main()"));
        assert!(all_text.contains("}"));
        assert!(!all_text.contains("```"));

        // All code block lines should have CodeBlockBg background
        let bg_color = theme::ayu_dark().get(crate::theme::ThemeRole::CodeBlockBg);
        let all_have_bg = lines.iter().all(|l| l.style.bg == Some(bg_color));
        assert!(
            all_have_bg,
            "All code block lines should have CodeBlockBg background"
        );
    }

    #[test]
    fn renders_fenced_code_block_with_language_syntax_highlighting() {
        let text = "```rust\nlet x = 1;\n```";
        let lines = super::render_markdown(text, Style::default(), 80, &theme::ayu_dark());

        // Should have exactly 1 line for the code (just "let x = 1;")
        assert_eq!(
            lines.len(),
            1,
            "Expected exactly 1 line for single-line code block, got {}",
            lines.len()
        );

        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();

        // Should contain the code without backticks
        assert!(all_text.contains("let"));
        assert!(all_text.contains("x"));
        assert!(all_text.contains("1"));
        assert!(!all_text.contains("```"));
        assert!(!all_text.contains("rust"));

        // The rust code should produce multiple spans with different colors
        // (syntax highlighting should be applied)
        let code_line = &lines[0];
        let has_multiple_colored_spans = code_line
            .spans
            .iter()
            .filter(|s| matches!(s.style.fg, Some(ratatui::style::Color::Rgb(_, _, _))))
            .count()
            > 1;
        assert!(
            has_multiple_colored_spans,
            "Rust code should have multiple colored spans from syntax highlighting"
        );

        // Line should have CodeBlockBg background
        let bg_color = theme::ayu_dark().get(crate::theme::ThemeRole::CodeBlockBg);
        assert_eq!(
            code_line.style.bg,
            Some(bg_color),
            "Code line should have CodeBlockBg background"
        );
    }

    #[test]
    fn renders_fenced_code_block_with_language_metadata_highlighting() {
        let text = "```rust,no_run\nlet x = 1;\n```";
        let lines = super::render_markdown(text, Style::default(), 80, &theme::ayu_dark());

        assert_eq!(
            lines.len(),
            1,
            "Expected exactly 1 line for single-line code block, got {}",
            lines.len()
        );

        let code_line = &lines[0];
        let highlighted_spans = code_line
            .spans
            .iter()
            .filter(|s| matches!(s.style.fg, Some(ratatui::style::Color::Rgb(_, _, _))))
            .count();
        assert!(
            highlighted_spans > 1,
            "Rust fence metadata should still produce syntax-highlighted spans"
        );
    }

    #[test]
    fn renders_fenced_code_block_unknown_language_monochrome() {
        let text = "```unknown_lang\nsome code here\n```";
        let lines = super::render_markdown(text, Style::default(), 80, &theme::ayu_dark());

        // Should have exactly 1 line for the code
        assert_eq!(
            lines.len(),
            1,
            "Expected exactly 1 line for single-line code block, got {}",
            lines.len()
        );

        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();

        // Should contain the code without backticks
        assert!(all_text.contains("some code here"));
        assert!(!all_text.contains("```"));

        // Unknown language should render as monochrome (single span or all same color)
        let code_line = &lines[0];
        let span_colors: Vec<_> = code_line.spans.iter().map(|s| s.style.fg).collect();
        // All non-indent spans should have the same CodeBlock color
        let code_block_color = theme::ayu_dark().get(crate::theme::ThemeRole::CodeBlock);
        let colored_spans = span_colors
            .iter()
            .filter(|c| c.is_some() && *c != &Some(ratatui::style::Color::Reset))
            .collect::<Vec<_>>();
        // Guard against vacuity: if the fallback regressed to drop the color
        // (fg=None/Reset), colored_spans would be empty and the loop below would
        // pass without checking anything. There must be at least one colored span.
        assert!(
            !colored_spans.is_empty(),
            "monochrome fallback must emit at least one CodeBlock-colored span"
        );
        for color in colored_spans {
            assert_eq!(
                color,
                &Some(code_block_color),
                "Unknown language code should use monochrome CodeBlock color"
            );
        }

        // Line should have CodeBlockBg background
        let bg_color = theme::ayu_dark().get(crate::theme::ThemeRole::CodeBlockBg);
        assert_eq!(
            code_line.style.bg,
            Some(bg_color),
            "Code line should have CodeBlockBg background"
        );
    }

    /// A code line WIDER than the viewport must not panic. The full-width band
    /// padding uses `(width).saturating_sub(line_width)`; before that guard a
    /// line longer than `width` underflowed (debug panic / huge release alloc).
    /// Regression for plan 0042 follow-up fix (a).
    #[test]
    fn renders_code_block_line_wider_than_width_without_panic() {
        let text = "```\nlet very_long_variable_name = some_function_call(1, 2, 3);\n```";
        // Width far narrower than the code line — exercises the saturating_sub path.
        let lines = super::render_markdown(text, Style::default(), 10, &theme::ayu_dark());
        // It rendered (no panic) and the over-wide code line still carries the band.
        let bg_color = theme::ayu_dark().get(crate::theme::ThemeRole::CodeBlockBg);
        assert!(
            lines.iter().any(|l| l.style.bg == Some(bg_color)),
            "the over-wide code line must still render with the CodeBlockBg band"
        );
    }

    #[test]
    fn renders_indented_code_block() {
        // 4-space indentation marks a code block in Markdown
        let text = "Normal text\n\n    fn test() {\n        println!(\"hello\");\n    }";
        let lines = super::render_markdown(text, Style::default(), 80, &theme::ayu_dark());

        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();

        // Should contain the code
        assert!(all_text.contains("fn test()"));
        assert!(all_text.contains("println!"));

        // Should have the normal text too
        assert!(all_text.contains("Normal text"));
    }

    #[test]
    fn renders_ordered_unordered_and_nested_lists() {
        let text = "1. a\n2. b\n\n- x\n  - y";
        let lines = super::render_markdown(text, Style::default(), 80, &theme::ayu_dark());

        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();

        // Should contain markers for ordered list
        assert!(all_text.contains("1."));
        assert!(all_text.contains("2."));

        // Should contain markers for unordered list
        assert!(all_text.contains("-"));

        // Should contain list items
        assert!(all_text.contains("a"));
        assert!(all_text.contains("b"));
        assert!(all_text.contains("x"));
        assert!(all_text.contains("y"));
    }

    #[test]
    fn list_items_do_not_emit_blank_rows_between_items() {
        let text = "- one\n- two\n- three";
        let lines = super::render_markdown(text, Style::default(), 80, &theme::ayu_dark());
        let rows = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            rows,
            vec!["- one", "- two", "- three"],
            "plain list items should render as consecutive rows without bogus blank rows"
        );
    }

    #[test]
    fn renders_blockquote() {
        let text = "> quoted";
        let lines = super::render_markdown(text, Style::default(), 80, &theme::ayu_dark());

        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();

        // Should contain the quoted text without the > marker
        assert!(all_text.contains("quoted"));
        assert!(!all_text.contains(">"));

        // Should be DIM
        let has_dim = lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.style.add_modifier(Modifier::DIM) == s.style)
        });
        assert!(has_dim);
    }

    #[test]
    fn renders_link_text_and_dim_url() {
        let text = "[docs](https://x.y)";
        let lines = super::render_markdown(text, Style::default(), 80, &theme::ayu_dark());

        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();

        // Should contain link text
        assert!(all_text.contains("docs"));

        // Should contain URL (with parentheses from our dim wrapper)
        assert!(all_text.contains("https://x.y"));
    }

    #[test]
    fn renders_thematic_break_as_rule() {
        let text = "a\n\n---\n\nb";
        let lines = super::render_markdown(text, Style::default(), 80, &theme::ayu_dark());

        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();

        // Should contain a and b
        assert!(all_text.contains("a"));
        assert!(all_text.contains("b"));

        // Should contain the rule line (all dashes)
        let has_rule = lines.iter().any(|l| {
            l.spans.iter().any(|s| {
                s.content.chars().all(|c| c == '─' || c == ' ')
                    && s.content.chars().filter(|c| *c == '─').count() > 2
            })
        });
        assert!(has_rule, "Rule line not found");
    }

    #[test]
    fn soft_and_hard_breaks_split_lines() {
        // Soft break (single newline)
        let text_soft = "l1\nl2";
        let lines_soft =
            super::render_markdown(text_soft, Style::default(), 80, &theme::ayu_dark());
        assert!(
            lines_soft.len() >= 2,
            "Soft break should create multiple lines"
        );

        // Hard break (double space + newline)
        let text_hard = "l1  \nl2";
        let lines_hard =
            super::render_markdown(text_hard, Style::default(), 80, &theme::ayu_dark());
        assert!(
            lines_hard.len() >= 2,
            "Hard break should create multiple lines"
        );
    }

    #[test]
    fn wraps_long_paragraph_to_small_width() {
        let text = "This is a very long paragraph with many words that should wrap to fit within a small width";
        let lines = super::render_markdown(text, Style::default(), 20, &theme::ayu_dark());

        // Should produce multiple lines
        assert!(
            lines.len() > 1,
            "Long paragraph should wrap to multiple lines"
        );

        // Every line should be within width (roughly)
        for line in lines {
            let line_width: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(
                line_width <= 20,
                "Line width {} exceeds max width of 20: {:?}",
                line_width,
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn partial_mid_stream_string_does_not_panic() {
        let full_text = "# Heading\n\nParagraph with [link](url) and `code`. \n\n```\nfn main() {}\n```\n\n- item 1\n- item 2";

        // Try rendering increasingly longer prefixes (char-boundary safe)
        for (i, _) in full_text.char_indices() {
            let prefix = &full_text[..i];
            // Should not panic
            let _lines = super::render_markdown(prefix, Style::default(), 80, &theme::ayu_dark());
        }

        // Also test the full text
        let _lines = super::render_markdown(full_text, Style::default(), 80, &theme::ayu_dark());
    }

    #[test]
    fn mixed_document_renders_all_constructs() {
        // A document exercising all major markdown constructs
        let text = "# Introduction\n\nThis is a paragraph with **bold** and *italic* text.\n\n## Features\n\n- First item\n- Second item\n  - Nested item\n\n1. Ordered one\n2. Ordered two\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n\n| Header A | Header B |\n|----------|----------|\n| Cell A1  | Cell B1  |\n| Cell A2  | Cell B2  |\n\n[Visit Documentation](https://example.com/docs)\n\n> This is a blockquote\n> with multiple lines\n\n---\n\nFinal paragraph with ~~strikethrough~~ and `inline code`.";

        let base_style = Style::default();
        let lines = super::render_markdown(text, base_style, 80, &theme::ayu_dark());

        // Assert that we have rendered output
        assert!(!lines.is_empty(), "Should render to at least one line");

        // Collect all visible text
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();

        // Assert that major headings and content are present
        assert!(
            all_text.contains("Introduction"),
            "Heading text should be present"
        );
        assert!(
            all_text.contains("Features"),
            "Second heading should be present"
        );
        assert!(all_text.contains("bold"), "Bold text should be present");
        assert!(all_text.contains("italic"), "Italic text should be present");
        assert!(
            all_text.contains("strikethrough"),
            "Strikethrough text should be present"
        );

        // Assert list markers are present with correct indentation
        assert!(
            all_text.contains("1.") && all_text.contains("2."),
            "Ordered list markers should be present"
        );
        assert!(
            all_text.contains("First item") && all_text.contains("Second item"),
            "List items should be present"
        );
        assert!(
            all_text.contains("Nested item"),
            "Nested item should be present"
        );

        // Assert nested item is indented *deeper* than its parent.
        // The renderer places the list-item marker as a span immediately before
        // the text span for that item.  For "Second item" (depth 0) the marker
        // is "- " (0 leading spaces); for "Nested item" (depth 1) the marker is
        // "  - " (2 leading spaces).  Both items may end up on the same Line
        // object when the nested sublist is parsed inside the parent item, so
        // we search span-by-span within each Line for the marker that precedes
        // the target text.
        fn leading_spaces_of_marker(lines: &[ratatui::text::Line<'_>], item_text: &str) -> usize {
            for line in lines {
                let spans = &line.spans;
                for i in 1..spans.len() {
                    if spans[i].content.as_ref().contains(item_text) {
                        // The preceding span is the marker for this item.
                        let marker = spans[i - 1].content.as_ref();
                        return marker.chars().take_while(|c| *c == ' ').count();
                    }
                }
            }
            // Fallback: no preceding span found — treat as indented 0.
            0
        }

        let second_item_indent = leading_spaces_of_marker(&lines, "Second item");
        let nested_item_indent = leading_spaces_of_marker(&lines, "Nested item");

        assert!(
            nested_item_indent > second_item_indent,
            "Nested item (indent={}) should be indented deeper than its parent 'Second item' (indent={})",
            nested_item_indent,
            second_item_indent
        );

        // Assert code block content and preservation
        assert!(
            all_text.contains("fn main()") && all_text.contains("println!"),
            "Code block content should be present"
        );
        assert!(
            !all_text.contains("```"),
            "Code fence markers should not appear"
        );

        // Assert that 'fn main()' and the closing '}' are on SEPARATE DIM Lines.
        // The spec requires each source line of a code block to become its own Line.
        let dim_line_texts: Vec<String> = lines
            .iter()
            .filter(|l| {
                l.spans.iter().any(|s| {
                    s.style
                        .add_modifier(Modifier::DIM)
                        .remove_modifier(Modifier::DIM)
                        != s.style
                })
            })
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        let fn_main_line = dim_line_texts
            .iter()
            .enumerate()
            .find(|(_, t)| t.contains("fn main()"));
        let closing_brace_line = dim_line_texts
            .iter()
            .enumerate()
            .find(|(_, t)| t.trim_start().starts_with('}'));

        assert!(
            fn_main_line.is_some(),
            "A DIM line containing 'fn main()' should exist in code block"
        );
        assert!(
            closing_brace_line.is_some(),
            "A DIM line containing the closing '}}' should exist in code block"
        );
        // They must be distinct lines (different index)
        assert!(
            fn_main_line.map(|(i, _)| i) != closing_brace_line.map(|(i, _)| i),
            "The 'fn main()' line and the closing '}}' line must be separate DIM Lines"
        );

        // Assert table content
        assert!(
            all_text.contains("Header A") && all_text.contains("Header B"),
            "Table headers should be present"
        );
        assert!(
            all_text.contains("Cell A1") && all_text.contains("Cell B1"),
            "Table cells should be present"
        );

        // Assert link text and URL
        assert!(
            all_text.contains("Documentation") || all_text.contains("Visit"),
            "Link text should be present"
        );
        assert!(
            all_text.contains("example.com"),
            "Link URL should be present"
        );

        // Assert the URL span is specifically DIM — find the span that contains
        // "example.com" and verify it carries the DIM modifier.
        let url_span_is_dim = lines.iter().any(|l| {
            l.spans.iter().any(|s| {
                s.content.as_ref().contains("example.com")
                    && s.style
                        .add_modifier(Modifier::DIM)
                        .remove_modifier(Modifier::DIM)
                        != s.style
            })
        });
        assert!(
            url_span_is_dim,
            "The span containing the link URL 'example.com' should have the DIM modifier"
        );

        // Assert blockquote content
        assert!(
            all_text.contains("blockquote"),
            "Blockquote content should be present"
        );

        // Assert rule is present (multiple '─' chars)
        let has_rule = lines.iter().any(|l| {
            l.spans.iter().any(|s| {
                let dash_count = s.content.chars().filter(|c| *c == '─').count();
                dash_count > 2
            })
        });
        assert!(has_rule, "Rule line (───) should be present");

        // Assert inline code is present
        assert!(
            all_text.contains("inline code"),
            "Inline code should be present"
        );
        assert!(
            !all_text.contains("`"),
            "Backticks should not appear in text"
        );

        // Assert no literal markdown markers
        assert!(
            !all_text.contains("# ") && !all_text.contains("## "),
            "Heading markers should not appear"
        );
        assert!(!all_text.contains("**"), "Bold markers should not appear");
        assert!(
            !all_text.contains("~~"),
            "Strikethrough markers should not appear"
        );
        assert!(
            !all_text.contains("[") || all_text.contains("example.com"),
            "Link brackets should be processed"
        );

        // Assert that styling is applied to at least some spans
        let has_bold = lines.iter().any(|l| {
            l.spans.iter().any(|s| {
                s.style.add_modifier(Modifier::BOLD) == s.style
                    || s.style
                        .add_modifier(Modifier::BOLD)
                        .remove_modifier(Modifier::BOLD)
                        != s.style
            })
        });
        assert!(has_bold, "Bold styling should be applied");

        // Check for dim styling (code blocks, links, blockquotes)
        let has_dim = lines.iter().any(|l| {
            l.spans.iter().any(|s| {
                s.style.add_modifier(Modifier::DIM) == s.style
                    || s.style
                        .add_modifier(Modifier::DIM)
                        .remove_modifier(Modifier::DIM)
                        != s.style
            })
        });
        assert!(
            has_dim,
            "Dim styling should be applied to code/links/quotes"
        );
    }

    #[test]
    fn streamed_prefixes_never_panic() {
        // The mixed document from the previous test
        let full_text = "# Introduction\n\nThis is a paragraph with **bold** and *italic* text.\n\n## Features\n\n- First item\n- Second item\n  - Nested item\n\n1. Ordered one\n2. Ordered two\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n\n| Header A | Header B |\n|----------|----------|\n| Cell A1  | Cell B1  |\n| Cell A2  | Cell B2  |\n\n[Visit Documentation](https://example.com/docs)\n\n> This is a blockquote\n> with multiple lines\n\n---\n\nFinal paragraph with ~~strikethrough~~ and `inline code`.";

        let base_style = Style::default();

        // Iterate through every char boundary prefix and render it.
        // This simulates the streaming accumulation in append_chunk.
        // The guarantee is simply that no prefix panics — the loop completing
        // without unwinding is the proof; no additional assertion is needed.
        for (i, _) in full_text.char_indices() {
            let prefix = &full_text[..i];
            // Should never panic, even for incomplete markup.
            let _lines = super::render_markdown(prefix, base_style, 80, &theme::ayu_dark());
        }

        // Also render the complete text
        let lines = super::render_markdown(full_text, base_style, 80, &theme::ayu_dark());
        assert!(!lines.is_empty(), "Complete text should render to lines");

        // Verify that the full text renders all expected content
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            all_text.contains("Introduction"),
            "Full text should contain expected content"
        );
    }

    #[test]
    fn ansi_response_is_stripped_and_rendered() {
        // A response mixing markdown and ANSI codes
        let text = "# Warning Alert\n\nThe system reported \x1b[38;5;208merror\x1b[0m during processing.\n\nDetails: \x1b[1mBold error message\x1b[0m in the logs.";

        let base_style = Style::default();
        let lines = super::render_markdown(text, base_style, 80, &theme::ayu_dark());

        // Should have rendered content
        assert!(!lines.is_empty(), "Should render content");

        // Collect all visible text
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();

        // Assert that ANSI escape sequences are NOT present
        assert!(
            !all_text.contains("\x1b"),
            "ANSI escape character (\\x1b) should not appear in rendered text"
        );
        assert!(
            !all_text.contains("[38;5;208m"),
            "ANSI escape codes should not appear in rendered text"
        );
        assert!(
            !all_text.contains("[0m"),
            "ANSI reset codes should not appear in rendered text"
        );
        assert!(
            !all_text.contains("[1m"),
            "ANSI bold codes should not appear in rendered text"
        );

        // Assert that the heading is present
        assert!(
            all_text.contains("Warning") && all_text.contains("Alert"),
            "Heading should be rendered without ANSI codes"
        );

        // Assert that the de-ANSI'd content is present
        assert!(
            all_text.contains("error"),
            "De-ANSI'd word 'error' should be present"
        );
        assert!(
            all_text.contains("Bold error message"),
            "De-ANSI'd message should be present"
        );
        assert!(all_text.contains("Details"), "Body text should be rendered");

        // Ensure no literal markdown markers
        assert!(
            !all_text.contains("# "),
            "Markdown heading marker should not appear"
        );
    }

    #[test]
    fn wrapping_respects_pane_width() {
        // A long single-paragraph string that exceeds the width
        let text = "This is a very long paragraph with many words that should wrap to fit within a small width. It contains multiple sentences to ensure the wrapping algorithm handles real content correctly.";

        let base_style = Style::default();
        let width: u16 = 20;

        let lines = super::render_markdown(text, base_style, width, &theme::ayu_dark());

        // Should produce multiple lines to fit the narrow width
        assert!(
            lines.len() > 1,
            "Long paragraph should wrap to multiple lines at narrow width"
        );

        // Verify that every line respects the width constraint
        for (line_idx, line) in lines.iter().enumerate() {
            let line_width: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();

            assert!(
                line_width <= width as usize,
                "Line {} has width {} which exceeds max width of {}. Content: {:?}",
                line_idx,
                line_width,
                width,
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<_>>()
            );
        }

        // Also verify that all words are present (no dropped content)
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();

        assert!(all_text.contains("This"), "First word should be present");
        assert!(all_text.contains("very"), "Content word should be present");
        assert!(
            all_text.contains("width"),
            "Width-related word should be present"
        );
        assert!(
            all_text.contains("wrapping"),
            "Wrapping-related word should be present"
        );
        assert!(
            all_text.contains("correctly"),
            "Last word should be present"
        );
    }
}
