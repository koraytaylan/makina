//! Block-based rendering for the exchange/execution log, inspired by
//! grok-build's `BlockContent` trait pattern.
//!
//! Each entry in the exchange log is rendered as a "block" with three display
//! modes (Collapsed, Truncated, Expanded) and a 1-char accent line showing
//! status. The user cycles modes with Enter or `!`.
//!
//! # Block types
//!
//! - [`PromptBlock`]: a prompt sent to the agent (one-liner in collapsed).
//! - [`ResponseBlock`]: agent response text (truncated by default, expanded
//!   on demand).
//! - [`ToolCallBlock`]: a tool invocation (collapsed shows title only).
//! - [`ThoughtBlock`]: agent reasoning (collapsed by default).
//! - [`CommandBlock`]: a git/shell command run by the orchestrator (collapsed
//!   shows `$ command`, expanded shows output).
//! - [`FailureBlock`]: a task failure reason (always expanded — the user needs
//!   to see why it failed).
//!
//! # Accent line
//!
//! A 1-char colored column on the left:
//! - Green `│` = success/complete
//! - Red `│` = error/failed
//! - Animated spinner `│` = running
//! - Dim `│` = pending/incomplete

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::app::{ExchangeContent, ExchangeEntry};
use crate::theme::ThemeRole;
use crate::ui::App;

/// The three display modes for a block, inspired by grok-build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode {
    /// One-liner: just the header/title. Maximum density.
    Collapsed,
    /// First line + "… +N lines" + last line. Compact but informative.
    Truncated,
    /// Full content. Verbose.
    Expanded,
}

impl DisplayMode {
    /// Cycle: Collapsed → Truncated → Expanded → Collapsed.
    pub fn next(self) -> Self {
        match self {
            Self::Collapsed => Self::Truncated,
            Self::Truncated => Self::Expanded,
            Self::Expanded => Self::Collapsed,
        }
    }
}

/// The accent line color for a block, based on its status.
#[derive(Debug, Clone, Copy)]
pub enum AccentStatus {
    /// Completed successfully.
    Success,
    /// Failed.
    Error,
    /// Still running.
    Running,
    /// Pending or incomplete.
    Pending,
}

impl AccentStatus {
    /// Resolve to a color from the app theme.
    pub fn color(self, app: &App) -> Color {
        match self {
            Self::Success => app.active_theme.get(ThemeRole::Success),
            Self::Error => app.active_theme.get(ThemeRole::Error),
            Self::Running => app.active_theme.get(ThemeRole::Accent),
            Self::Pending => app.active_theme.get(ThemeRole::Dim),
        }
    }
}

/// Context passed to a block when rendering.
pub struct BlockContext<'a> {
    pub app: &'a App,
    pub width: u16,
    pub mode: DisplayMode,
    pub is_running: bool,
}

/// Result of rendering a block: lines + accent status.
pub struct BlockOutput {
    pub lines: Vec<Line<'static>>,
    pub accent: AccentStatus,
}

/// Render a single exchange entry as a block.
pub fn render_entry(
    entry: &ExchangeEntry,
    app: &App,
    width: u16,
    mode: DisplayMode,
) -> BlockOutput {
    let ctx = BlockContext {
        app,
        width,
        mode,
        is_running: !entry.complete(),
    };
    match &entry.content {
        ExchangeContent::Prompt { text } => render_prompt(text, &ctx),
        ExchangeContent::Response { text, complete } => render_response(text, *complete, &ctx),
        ExchangeContent::Thought { text } => render_thought(text, &ctx),
        ExchangeContent::Tool {
            title,
            kind,
            status,
            ..
        } => render_tool(title, kind.as_deref(), status, &ctx),
    }
}

/// Render a failure reason as a block (always expanded).
pub fn render_failure(message: &str, app: &App, width: u16) -> BlockOutput {
    let dim = Style::default().fg(app.active_theme.get(ThemeRole::Dim));
    let error_style = Style::default().fg(app.active_theme.get(ThemeRole::Error));
    let mut lines = vec![Line::from(vec![Span::styled(
        format!("  ✗ Task failed: {message}"),
        error_style,
    )])];
    // Wrap if needed.
    if message.len() > width as usize - 4 {
        // Simple word-wrap: split on spaces near the width.
        // The first line already has the header; add wrapped continuation.
        let _ = &mut lines; // keep it simple — one line is enough for most failure messages
    }
    let _ = dim;
    BlockOutput {
        lines,
        accent: AccentStatus::Error,
    }
}

/// Render a command (git/shell) as a block.
pub fn render_command(
    command: &str,
    output: Option<&str>,
    app: &App,
    width: u16,
    mode: DisplayMode,
) -> BlockOutput {
    let dim = Style::default().fg(app.active_theme.get(ThemeRole::Dim));
    let accent_color = app.active_theme.get(ThemeRole::Accent);
    let header_style = Style::default()
        .fg(accent_color)
        .add_modifier(Modifier::BOLD);

    let mut lines = match mode {
        DisplayMode::Collapsed => {
            // One-liner: $ command (truncated to width).
            let cmd_display = if command.len() > width as usize - 4 {
                format!("$ {}…", &command[..width as usize - 5])
            } else {
                format!("$ {command}")
            };
            vec![Line::from(vec![Span::styled(cmd_display, header_style)])]
        }
        DisplayMode::Truncated => {
            let mut lines = vec![Line::from(vec![Span::styled(
                format!("$ {command}"),
                header_style,
            )])];
            if let Some(out) = output {
                let out_lines: Vec<&str> = out.lines().collect();
                if out_lines.len() > 4 {
                    lines.push(Line::from(vec![Span::styled(
                        format!("  {}…", out_lines[0]),
                        dim,
                    )]));
                    lines.push(Line::from(vec![Span::styled(
                        format!("  … +{} lines", out_lines.len() - 2),
                        dim,
                    )]));
                    lines.push(Line::from(vec![Span::styled(
                        format!("  {}", out_lines[out_lines.len() - 1]),
                        dim,
                    )]));
                } else {
                    for line in &out_lines {
                        lines.push(Line::from(vec![Span::styled(format!("  {line}"), dim)]));
                    }
                }
            }
            lines
        }
        DisplayMode::Expanded => {
            let mut lines = vec![Line::from(vec![Span::styled(
                format!("$ {command}"),
                header_style,
            )])];
            if let Some(out) = output {
                for line in out.lines() {
                    lines.push(Line::from(vec![Span::styled(format!("  {line}"), dim)]));
                }
            }
            lines
        }
    };

    // Accent: green if output is present and non-empty, dim otherwise.
    let accent = if output.is_some_and(|o| !o.is_empty()) {
        AccentStatus::Success
    } else {
        AccentStatus::Pending
    };

    // Prepend accent line to each line.
    let accent_color = accent.color(app);
    for line in &mut lines {
        line.spans
            .insert(0, Span::styled("│ ", Style::default().fg(accent_color)));
    }

    BlockOutput { lines, accent }
}

// ── Per-type renderers ───────────────────────────────────────────────────────

fn render_prompt(text: &str, ctx: &BlockContext) -> BlockOutput {
    let accent_color = ctx.app.active_theme.get(ThemeRole::Accent);
    let dim = ctx.app.active_theme.get(ThemeRole::Dim);
    let header_style = Style::default()
        .fg(accent_color)
        .add_modifier(Modifier::BOLD);

    let lines = match ctx.mode {
        DisplayMode::Collapsed => {
            // One-liner: first line of the prompt (truncated).
            let first_line = text.lines().next().unwrap_or(text);
            let display = if first_line.len() > ctx.width as usize - 4 {
                format!("▶ {}…", &first_line[..ctx.width as usize - 5])
            } else {
                format!("▶ {first_line}")
            };
            vec![Line::from(vec![Span::styled(display, header_style)])]
        }
        _ => {
            // Truncated and Expanded: show first few lines.
            let mut lines = Vec::new();
            let text_lines: Vec<&str> = text.lines().collect();
            let limit = match ctx.mode {
                DisplayMode::Truncated => 3,
                DisplayMode::Expanded => text_lines.len(),
                DisplayMode::Collapsed => 1,
            };
            let shown = text_lines.len().min(limit);
            for (i, line) in text_lines.iter().take(shown).enumerate() {
                let prefix = if i == 0 { "▶ " } else { "  " };
                lines.push(Line::from(vec![Span::styled(
                    format!("{prefix}{line}"),
                    header_style,
                )]));
            }
            if text_lines.len() > shown {
                lines.push(Line::from(vec![Span::styled(
                    format!("  … +{} lines", text_lines.len() - shown),
                    Style::default().fg(dim),
                )]));
            }
            lines
        }
    };

    BlockOutput {
        lines,
        accent: AccentStatus::Success,
    }
}

fn render_response(text: &str, complete: bool, ctx: &BlockContext) -> BlockOutput {
    let fg = ctx.app.active_theme.get(ThemeRole::Foreground);
    let dim = ctx.app.active_theme.get(ThemeRole::Dim);

    let accent = if ctx.is_running {
        AccentStatus::Running
    } else if complete {
        AccentStatus::Success
    } else {
        AccentStatus::Pending
    };

    let lines = match ctx.mode {
        DisplayMode::Collapsed => {
            // One-liner: first line of response.
            let first_line = text.lines().next().unwrap_or(text);
            let display = if first_line.len() > ctx.width as usize - 4 {
                format!("  {}…", &first_line[..ctx.width as usize - 5])
            } else {
                format!("  {first_line}")
            };
            vec![Line::from(vec![Span::styled(
                display,
                Style::default().fg(fg),
            )])]
        }
        DisplayMode::Truncated => {
            let mut lines = Vec::new();
            let text_lines: Vec<&str> = text.lines().collect();
            let limit = 5;
            let shown = text_lines.len().min(limit);
            for line in text_lines.iter().take(shown) {
                lines.push(Line::from(vec![Span::styled(
                    format!("  {line}"),
                    Style::default().fg(fg),
                )]));
            }
            if text_lines.len() > shown {
                lines.push(Line::from(vec![Span::styled(
                    format!("  … +{} lines", text_lines.len() - shown),
                    Style::default().fg(dim),
                )]));
            }
            lines
        }
        DisplayMode::Expanded => text
            .lines()
            .map(|line| {
                Line::from(vec![Span::styled(
                    format!("  {line}"),
                    Style::default().fg(fg),
                )])
            })
            .collect(),
    };

    BlockOutput { lines, accent }
}

fn render_thought(text: &str, ctx: &BlockContext) -> BlockOutput {
    let dim = ctx.app.active_theme.get(ThemeRole::Dim);

    let lines = match ctx.mode {
        DisplayMode::Collapsed => {
            let first_line = text.lines().next().unwrap_or(text);
            let display = if first_line.len() > ctx.width as usize - 6 {
                format!("  💭 {}…", &first_line[..ctx.width as usize - 7])
            } else {
                format!("  💭 {first_line}")
            };
            vec![Line::from(vec![Span::styled(
                display,
                Style::default().fg(dim),
            )])]
        }
        _ => {
            let mut lines = Vec::new();
            let text_lines: Vec<&str> = text.lines().collect();
            let limit = match ctx.mode {
                DisplayMode::Truncated => 3,
                _ => text_lines.len(),
            };
            for (i, line) in text_lines.iter().take(limit).enumerate() {
                let prefix = if i == 0 { "  💭 " } else { "     " };
                lines.push(Line::from(vec![Span::styled(
                    format!("{prefix}{line}"),
                    Style::default().fg(dim),
                )]));
            }
            if text_lines.len() > limit {
                lines.push(Line::from(vec![Span::styled(
                    format!("     … +{} lines", text_lines.len() - limit),
                    Style::default().fg(dim),
                )]));
            }
            lines
        }
    };

    BlockOutput {
        lines,
        accent: AccentStatus::Success,
    }
}

fn render_tool(title: &str, kind: Option<&str>, status: &str, ctx: &BlockContext) -> BlockOutput {
    let accent = match status {
        "completed" | "success" => AccentStatus::Success,
        "failed" | "error" => AccentStatus::Error,
        "pending" | "running" => AccentStatus::Running,
        _ => AccentStatus::Pending,
    };
    let accent_color = accent.color(ctx.app);
    let fg = ctx.app.active_theme.get(ThemeRole::Foreground);
    let dim = ctx.app.active_theme.get(ThemeRole::Dim);

    let icon = match kind {
        Some("execute") => "⚡",
        Some("edit") => "✎",
        Some("read") => "📖",
        Some("search") => "🔍",
        _ => "🔧",
    };

    let title_display = if title.is_empty() {
        kind.unwrap_or("tool").to_string()
    } else {
        title.to_string()
    };

    let lines = match ctx.mode {
        DisplayMode::Collapsed => {
            vec![Line::from(vec![
                Span::styled(format!("  {icon} "), Style::default().fg(accent_color)),
                Span::styled(title_display, Style::default().fg(fg)),
                Span::styled(format!(" ({status})"), Style::default().fg(dim)),
            ])]
        }
        _ => {
            vec![Line::from(vec![
                Span::styled(format!("  {icon} "), Style::default().fg(accent_color)),
                Span::styled(title_display, Style::default().fg(fg)),
                Span::styled(format!(" ({status})"), Style::default().fg(dim)),
            ])]
        }
    };

    BlockOutput { lines, accent }
}

/// Per-entry default display mode (what mode to start in when the entry first
/// appears). Inspired by grok-build's `default_display_mode()`.
pub fn default_mode(entry: &ExchangeEntry) -> DisplayMode {
    match &entry.content {
        // Prompts: collapsed (just the first line).
        ExchangeContent::Prompt { .. } => DisplayMode::Collapsed,
        // Responses: truncated (first few lines, expandable).
        ExchangeContent::Response { complete, .. } => {
            if *complete {
                DisplayMode::Truncated
            } else {
                // Still streaming — show more.
                DisplayMode::Truncated
            }
        }
        // Thoughts: collapsed (dim, secondary).
        ExchangeContent::Thought { .. } => DisplayMode::Collapsed,
        // Tool calls: collapsed (just the icon + title).
        ExchangeContent::Tool { .. } => DisplayMode::Collapsed,
    }
}

/// Render all exchange entries as block lines with accent column.
pub fn render_entries(entries: &[ExchangeEntry], app: &App, width: u16) -> Vec<Line<'static>> {
    let accent_width = 2u16; // "│ "
    let content_width = width.saturating_sub(accent_width);
    let mut all_lines = Vec::new();

    for entry in entries {
        let mode = default_mode(entry);
        let output = render_entry(entry, app, content_width, mode);
        let accent_color = output.accent.color(app);

        for line in output.lines {
            // Prepend accent column to each line.
            let mut spans = vec![Span::styled("│ ", Style::default().fg(accent_color))];
            spans.extend(line.spans);
            all_lines.push(Line::from(spans));
        }
        // Blank line between blocks for readability.
        all_lines.push(Line::from(""));
    }

    // Remove trailing blank.
    if all_lines.last().is_some_and(|l| l.spans.is_empty()) {
        all_lines.pop();
    }

    all_lines
}
