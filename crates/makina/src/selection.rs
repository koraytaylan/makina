//! Mouse-driven text selection over the rendered screen.
//!
//! # Why this exists
//!
//! The TUI enables mouse capture (see [`crate::tui`]) so the scroll wheel can
//! drive the exchange pane. The catch is that mouse capture and the terminal's
//! *own* click-drag text selection are mutually exclusive: once any mouse
//! tracking mode is on, the terminal forwards click/drag events to the
//! application instead of selecting text itself. That left users unable to
//! select & copy anything on screen unless they reached for a terminal-specific
//! bypass modifier (Shift, or Option in iTerm2).
//!
//! This module re-implements selection *in the application*: every crossterm
//! mouse event carries the pointer's `(column, row)` cell, so we can track the
//! dragged region ourselves, paint the highlight, and pull the selected text
//! straight out of the rendered [`Buffer`]. The event loop then copies that
//! text to the system clipboard via OSC 52. The result keeps the scroll wheel
//! *and* gives mouse selection that works in any terminal, with no modifier key.
//!
//! # Pane confinement
//!
//! A selection is confined to a single pane: the [`bounds`](Selection::bounds)
//! rectangle of whichever pane the drag began in. The render pass records the
//! pane rectangles (see `App::set_selection_panes`) and the event layer picks
//! the one under the anchor. Confinement is what keeps a wide drag in the
//! content pane from also sweeping the sidebar on the rows the two share — the
//! moving end is clamped into `bounds`, and both the highlight and the
//! extracted text are limited to it.
//!
//! # Geometry
//!
//! Within `bounds`, selection follows *text-flow* (linear) order, matching how
//! a terminal or editor selects: from the anchor cell to the cursor cell in
//! reading order (top-to-bottom, then left-to-right). The first row runs from
//! the start column to the right edge of `bounds`, full-width rows in between,
//! and the last row from the left edge to the end column. [`Selection::row_range`]
//! is the single source of truth shared by both the highlight
//! ([`Selection::highlight`]) and the text extraction ([`Selection::extract`])
//! so the two never disagree.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;

/// An in-progress or completed text selection, in terminal cell coordinates.
///
/// Coordinates are absolute screen cells (0-based from the top-left), exactly as
/// crossterm reports them and as the full-screen render [`Buffer`] is indexed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// The cell where the drag began (left button down). Always inside
    /// [`bounds`](Self::bounds).
    pub anchor: (u16, u16),
    /// The cell under the pointer now (updated on drag and on button up). May
    /// lie outside [`bounds`](Self::bounds); the geometry clamps it back in.
    pub cursor: (u16, u16),
    /// `true` while the button is still held (a live drag); `false` once
    /// released. A released selection keeps its highlight until the next click.
    pub active: bool,
    /// The pane rectangle the selection is confined to. The drag began inside
    /// it; the moving end is clamped into it so a wide drag never bleeds into a
    /// neighbouring pane.
    pub bounds: Rect,
    /// Whether the pane holds wrapped prose, in which case a row break may be
    /// the renderer's rather than the author's. See [`unwrap_rows`].
    pub flow: bool,
}

impl Selection {
    /// Begin a new selection anchored at `(x, y)`, confined to `bounds`.
    pub fn start(x: u16, y: u16, bounds: Rect) -> Self {
        Self {
            anchor: (x, y),
            cursor: (x, y),
            active: true,
            bounds,
            flow: false,
        }
    }

    /// Begin a selection in a pane whose content is wrapped prose.
    pub fn start_flowing(x: u16, y: u16, bounds: Rect) -> Self {
        Self {
            flow: true,
            ..Self::start(x, y, bounds)
        }
    }

    /// Move the free end of the selection to `(x, y)` (clamped into `bounds`
    /// only when the geometry is computed, so the raw cursor is preserved).
    pub fn extend(&mut self, x: u16, y: u16) {
        self.cursor = (x, y);
    }

    /// Whether the selection still covers a single cell — i.e. a plain click
    /// with no drag. Such a selection yields no text and is discarded.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }

    /// Clamp a screen cell into [`bounds`](Self::bounds).
    fn clamp(&self, p: (u16, u16)) -> (u16, u16) {
        let b = self.bounds;
        let x = p.0.clamp(b.left(), b.right().saturating_sub(1));
        let y = p.1.clamp(b.top(), b.bottom().saturating_sub(1));
        (x, y)
    }

    /// The `(start, end)` cell pair — clamped into `bounds` — in reading order:
    /// `start` is never after `end` (top-to-bottom, then left-to-right).
    fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        let a = self.clamp(self.anchor);
        let c = self.clamp(self.cursor);
        // Compare row-major (row, then column).
        if (a.1, a.0) <= (c.1, c.0) {
            (a, c)
        } else {
            (c, a)
        }
    }

    /// The inclusive column range `[start, end]` selected on `row`, in text-flow
    /// order, clamped to `bounds`. `None` if `row` lies outside the selection or
    /// `bounds` is degenerate.
    fn row_range(&self, row: u16) -> Option<(u16, u16)> {
        let b = self.bounds;
        if b.width == 0 || b.height == 0 {
            return None;
        }
        let ((sx, sy), (ex, ey)) = self.ordered();
        if row < sy || row > ey {
            return None;
        }
        let left = b.left();
        let right = b.right() - 1; // width > 0 ⇒ no underflow
        let start = if row == sy { sx } else { left };
        let end = if row == ey { ex } else { right };
        Some((start, end))
    }

    /// Paint the selected cells with themed selection colors over `buf`.
    ///
    /// Colors each cell with the theme's `SelectionBg` background and `Foreground`
    /// text color. A no-op for an empty (single-cell) selection so a plain click
    /// never flashes a stray styled cell. Applied last in the render pass so it
    /// overrides whatever pane or overlay drew underneath — but only within
    /// [`bounds`](Self::bounds).
    pub fn highlight(&self, buf: &mut Buffer, theme: &crate::theme::Theme) {
        if self.is_empty() {
            return;
        }
        let style = Style::default()
            .bg(theme.get(crate::theme::ThemeRole::SelectionBg))
            .fg(theme.get(crate::theme::ThemeRole::Foreground));
        for row in self.bounds.top()..self.bounds.bottom() {
            if let Some((start, end)) = self.row_range(row) {
                for col in start..=end {
                    if let Some(cell) = buf.cell_mut(Position::new(col, row)) {
                        cell.set_style(style);
                    }
                }
            }
        }
    }

    /// Extract the selected text from the rendered `buf`.
    ///
    /// Each row's trailing whitespace (the padding cells beyond a line's content
    /// render as spaces) is trimmed. Rows are joined with `\n` — except in a
    /// [`flow`](Self::flow) pane, where [`unwrap_rows`] puts a newline only
    /// where the author wrote one. Returns `None` for an empty selection or one
    /// that covers only whitespace.
    pub fn extract(&self, buf: &Buffer) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let ((_, sy), (_, ey)) = self.ordered();
        let mut lines: Vec<String> = Vec::new();
        for row in sy..=ey {
            let Some((start, end)) = self.row_range(row) else {
                continue;
            };
            let mut line = String::new();
            for col in start..=end {
                if let Some(cell) = buf.cell(Position::new(col, row)) {
                    line.push_str(cell.symbol());
                }
            }
            lines.push(line.trim_end().to_string());
        }
        let text = if self.flow {
            unwrap_rows(&lines, self.bounds.width)
        } else {
            lines.join("\n")
        };
        if text.trim().is_empty() {
            None
        } else {
            Some(text)
        }
    }
}

/// Rejoin rows that word wrap split, leaving the author's own newlines alone.
///
/// A wrapped paragraph is one line of text that the renderer had to draw on
/// several rows. Copying it row by row pasted a hard break into the middle of
/// every sentence — the text came back as the pane happened to be wide, not as
/// it was written.
///
/// The rows alone do not say which breaks are which, but the wrap that made
/// them does: every wrapper here is greedy, so it breaks before a word *only*
/// when that word could not fit on the row. So if the next row's first word
/// would still have fitted, the break must have been written; if it could not
/// have, the break is the wrap's and the rows are one line.
///
/// This is exact for greedy wrapping and errs the safe way elsewhere: a short
/// row (a heading, a list item, a blank line between paragraphs) always keeps
/// its newline, because a short row leaves room for the next word.
fn unwrap_rows(rows: &[String], width: u16) -> String {
    let mut out = String::new();
    for (index, row) in rows.iter().enumerate() {
        if index == 0 {
            out.push_str(row);
            continue;
        }
        let previous = rows[index - 1].as_str();
        // The indent a continuation row inherits is layout, not content: it is
        // the same rail/indent the first row already carries.
        let continued = row.trim_start();
        let word = continued.split(' ').next().unwrap_or_default();
        let soft_wrapped = !previous.is_empty()
            && !continued.is_empty()
            && previous.chars().count() + 1 + word.chars().count() > width as usize;
        if soft_wrapped {
            out.push(' ');
            out.push_str(continued);
        } else {
            out.push('\n');
            out.push_str(row);
        }
    }
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a buffer from `lines`, each written from column 0 on its own row.
    fn buffer(lines: &[&str]) -> Buffer {
        let width = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
        let mut buf = Buffer::empty(Rect::new(0, 0, width, lines.len() as u16));
        for (row, line) in lines.iter().enumerate() {
            buf.set_string(0, row as u16, line, Style::default());
        }
        buf
    }

    /// A selection over the whole of `buf` (no pane clipping).
    fn whole(buf: &Buffer, anchor: (u16, u16), cursor: (u16, u16)) -> Selection {
        Selection {
            anchor,
            cursor,
            active: false,
            bounds: buf.area,
            flow: false,
        }
    }

    /// A selection over the whole of `buf`, in a pane of wrapped prose.
    fn whole_flowing(buf: &Buffer, anchor: (u16, u16), cursor: (u16, u16)) -> Selection {
        Selection {
            flow: true,
            ..whole(buf, anchor, cursor)
        }
    }

    /// A paragraph the renderer had to break comes back as one paragraph.
    #[test]
    fn a_wrapped_paragraph_is_copied_as_one_line() {
        // Greedy wrap of "the quick brown fox jumps over it" at width 20.
        let buf = buffer(&["the quick brown fox", "jumps over it"]);
        let sel = whole_flowing(&buf, (0, 0), (12, 1));
        assert_eq!(
            sel.extract(&buf).as_deref(),
            Some("the quick brown fox jumps over it"),
            "a break the pane width caused is not a break the author wrote",
        );
    }

    /// A newline the author wrote survives being copied.
    #[test]
    fn an_authored_newline_is_kept() {
        // "brown" would have fitted after "the quick" — so that break was
        // written, not wrapped.
        let buf = buffer(&["the quick           ", "brown fox           "]);
        let sel = whole_flowing(&buf, (0, 0), (19, 1));
        assert_eq!(sel.extract(&buf).as_deref(), Some("the quick\nbrown fox"),);
    }

    /// The blank line between two paragraphs keeps both breaks.
    #[test]
    fn a_blank_line_between_paragraphs_survives() {
        let buf = buffer(&["the quick brown fox", "", "jumps over the lazy"]);
        let sel = whole_flowing(&buf, (0, 0), (18, 2));
        assert_eq!(
            sel.extract(&buf).as_deref(),
            Some("the quick brown fox\n\njumps over the lazy"),
        );
    }

    /// The indent a continuation row inherits is layout, not content.
    #[test]
    fn a_continuation_row_does_not_carry_its_indent_into_the_text() {
        let buf = buffer(&["  the quick brown fo", "  x jumps over it   "]);
        let sel = whole_flowing(&buf, (0, 0), (19, 1));
        assert_eq!(
            sel.extract(&buf).as_deref(),
            Some("  the quick brown fo x jumps over it"),
        );
    }

    /// Outside a prose pane a row is a line, whatever its width: a tree, a
    /// table, or a list must not have its rows run together.
    #[test]
    fn rows_outside_a_prose_pane_keep_every_break() {
        let buf = buffer(&["abcde", "fghij", "klmno"]);
        let sel = whole(&buf, (0, 0), (4, 2));
        assert_eq!(sel.extract(&buf).as_deref(), Some("abcde\nfghij\nklmno"));
    }

    #[test]
    fn ordered_normalises_anchor_after_cursor() {
        // Drag up-and-left: anchor is below/after the cursor.
        let sel = Selection {
            anchor: (4, 2),
            cursor: (1, 0),
            active: false,
            bounds: Rect::new(0, 0, 100, 100),
            flow: false,
        };
        assert_eq!(sel.ordered(), ((1, 0), (4, 2)));
    }

    #[test]
    fn single_row_selection_extracts_inclusive_span() {
        let buf = buffer(&["hello world"]);
        let sel = whole(&buf, (0, 0), (4, 0)); // 'o' of hello, inclusive
        assert_eq!(sel.extract(&buf).as_deref(), Some("hello"));
    }

    #[test]
    fn multi_row_selection_flows_like_text() {
        // First row from the start column to the edge, full middle row, last
        // row from the left edge to the end column.
        let buf = buffer(&["abcde", "fghij", "klmno"]);
        let sel = whole(&buf, (2, 0), (1, 2)); // 'c' .. 'l'
        assert_eq!(sel.extract(&buf).as_deref(), Some("cde\nfghij\nkl"));
    }

    #[test]
    fn reversed_drag_extracts_same_text() {
        let buf = buffer(&["abcde", "fghij", "klmno"]);
        let forward = whole(&buf, (2, 0), (1, 2));
        let backward = whole(&buf, (1, 2), (2, 0));
        assert_eq!(forward.extract(&buf), backward.extract(&buf));
    }

    #[test]
    fn trailing_padding_is_trimmed_per_row() {
        // "hi" padded to width 5; selecting the whole row drops the padding.
        let buf = buffer(&["hi", "world"]);
        let sel = whole(&buf, (0, 0), (4, 0));
        assert_eq!(sel.extract(&buf).as_deref(), Some("hi"));
    }

    #[test]
    fn empty_selection_yields_no_text() {
        let buf = buffer(&["hello"]);
        let sel = Selection::start(2, 0, buf.area); // click, no drag
        assert!(sel.is_empty());
        assert_eq!(sel.extract(&buf), None);
    }

    #[test]
    fn whitespace_only_selection_yields_no_text() {
        let buf = buffer(&["     "]);
        let sel = whole(&buf, (0, 0), (4, 0));
        assert_eq!(sel.extract(&buf), None);
    }

    /// A selection confined to the right-hand pane never picks up the left
    /// pane's cells, even when the drag's free end is pulled left into it.
    #[test]
    fn bounds_confine_selection_to_its_pane() {
        // Two side-by-side "panes": cols 0-2 (left) and cols 3-5 (right).
        let buf = buffer(&["LLLRRR", "lllrrr"]);
        let right = Rect::new(3, 0, 3, 2);

        // Straight drag down the right pane.
        let sel = Selection {
            anchor: (3, 0),
            cursor: (5, 1),
            active: false,
            bounds: right,
            flow: false,
        };
        assert_eq!(sel.extract(&buf).as_deref(), Some("RRR\nrrr"));

        // Dragging the free end left into the LEFT pane stays clipped to the
        // right pane — the left columns are never selected.
        let crossing = Selection {
            anchor: (3, 0),
            cursor: (0, 1), // clamps to column 3
            active: false,
            bounds: right,
            flow: false,
        };
        let text = crossing.extract(&buf).unwrap();
        assert!(
            !text.contains('L') && !text.contains('l'),
            "left pane leaked: {text:?}"
        );
    }

    #[test]
    fn highlight_styles_only_cells_within_bounds() {
        // Bounds are the right pane (cols 3-5). Dragging from its right edge out
        // into the left pane selects the whole right pane but never the left:
        // the free end clamps to column 3.
        let th = crate::theme::ayu_dark();
        let mut buf = buffer(&["LLLRRR"]);
        let sel = Selection {
            anchor: (5, 0),
            cursor: (0, 0), // clamps to column 3
            active: false,
            bounds: Rect::new(3, 0, 3, 1),
            flow: false,
        };
        sel.highlight(&mut buf, &th);
        let has_selection_style = |x: u16| {
            let cell = buf.cell(Position::new(x, 0)).unwrap();
            cell.bg == th.get(crate::theme::ThemeRole::SelectionBg)
                && cell.fg == th.get(crate::theme::ThemeRole::Foreground)
        };
        assert!(
            !has_selection_style(0) && !has_selection_style(1) && !has_selection_style(2),
            "left pane untouched"
        );
        assert!(
            has_selection_style(3) && has_selection_style(4) && has_selection_style(5),
            "right pane highlighted"
        );
    }

    #[test]
    fn highlight_skips_empty_selection() {
        let th = crate::theme::ayu_dark();
        let mut buf = buffer(&["abcde"]);
        Selection::start(2, 0, buf.area).highlight(&mut buf, &th);
        for x in 0..5 {
            let cell = buf.cell(Position::new(x, 0)).unwrap();
            assert!(
                cell.bg != th.get(crate::theme::ThemeRole::SelectionBg),
                "a plain click must not highlight any cell"
            );
        }
    }
}
