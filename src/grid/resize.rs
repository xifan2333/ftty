//! Terminal grid dimensions resizing, scrollback line restoration, and placement adjustment.

use crate::grid::{Cursor, Grid, Row};
use crate::kitty::ImagePlacement;

/// Drops `to_remove` rows from `lines`, discarding rows below `cursor_row` first so the
/// cursor's line survives. Returns the rows removed from the top, in order.
pub(crate) fn shrink_rows(
    lines: &mut Vec<Row>,
    to_remove: usize,
    cursor_row: usize,
    new_rows: usize,
) -> Vec<Row> {
    let below_cursor = lines.len().saturating_sub(1).saturating_sub(cursor_row);
    let from_bottom = to_remove.min(below_cursor);
    let from_top = to_remove - from_bottom;
    let removed = lines.drain(0..from_top).collect();
    lines.truncate(new_rows);
    removed
}

/// Moves every placement anchored to a surviving row up by `amount`, dropping the ones whose row
/// was removed. Used when a screen trims its top rows without saving them to scrollback.
pub(crate) fn shift_placements(placements: &mut Vec<ImagePlacement>, amount: usize) {
    if amount == 0 {
        return;
    }
    placements.retain_mut(|p| {
        if p.line < amount {
            false
        } else {
            p.line -= amount;
            true
        }
    });
}

impl Grid {
    /// Resizes the grid dimensions.
    ///
    /// Shrinking keeps the cursor's line on screen and discards rows below it first, so content
    /// above the cursor does not scroll away while the area beneath it is still empty.
    pub fn resize(&mut self, new_cols: usize, new_rows: usize) {
        let new_cols = new_cols.max(1);
        let new_rows = new_rows.max(1);
        let old_rows = self.rows;

        for row in &mut self.lines {
            row.resize(new_cols);
        }
        for row in &mut self.scrollback {
            row.resize(new_cols);
        }
        if let Some(alt) = &mut self.alt_lines {
            for row in alt.iter_mut() {
                row.resize(new_cols);
            }
        }

        if new_rows > old_rows {
            let needed = new_rows - old_rows;
            let active_on_alt = self.alt_lines.is_some();
            let pull_from_scrollback = if !active_on_alt {
                needed.min(self.scrollback.len())
            } else {
                0
            };

            for _ in 0..pull_from_scrollback {
                if let Some(mut row) = self.scrollback.pop_back() {
                    row.resize(new_cols);
                    self.lines.insert(0, row);
                }
            }
            self.cursor.row = self
                .cursor
                .row
                .saturating_add(pull_from_scrollback)
                .min(new_rows - 1);
            self.saved_cursor.row = self
                .saved_cursor
                .row
                .saturating_add(pull_from_scrollback)
                .min(new_rows - 1);
            self.viewport_offset = self.viewport_offset.min(self.scrollback.len());

            let remaining_blanks = needed - pull_from_scrollback;
            for _ in 0..remaining_blanks {
                self.lines.push(Row::new(new_cols));
            }
            if let Some(alt) = &mut self.alt_lines {
                for _ in old_rows..new_rows {
                    alt.push(Row::new(new_cols));
                }
            }
        } else if new_rows < old_rows {
            let to_remove = old_rows - new_rows;

            let active_on_alt = self.alt_lines.is_some();
            if active_on_alt {
                // Alternate screen (Neovim/htop/tmux): rows strictly map to absolute screen rows.
                // Truncate at bottom only; never drain from top or shift surviving row coordinates.
                self.lines.truncate(new_rows);
                self.cursor.row = self.cursor.row.min(new_rows.saturating_sub(1));
                self.saved_cursor.row = self.saved_cursor.row.min(new_rows.saturating_sub(1));

                // Hidden primary screen stashed in alt: trim rows preserving cursor line
                if let Some(alt) = &mut self.alt_lines {
                    let hidden_row = self.alt_cursor.unwrap_or_default().row;
                    let hidden_removed = shrink_rows(alt, to_remove, hidden_row, new_rows);
                    let hidden_from_top = hidden_removed.len();
                    if let Some(cursor) = &mut self.alt_cursor {
                        cursor.row = cursor.row.saturating_sub(hidden_from_top);
                    }
                    if let Some(saved) = &mut self.alt_saved_cursor {
                        saved.row = saved.row.saturating_sub(hidden_from_top);
                    }
                    shift_placements(&mut self.alt_placements, hidden_from_top);
                }
            } else {
                // Primary screen with scrollback: trim below cursor first so active prompt/cursor survives.
                let active_retains = self.max_scrollback > 0;
                let removed_top =
                    shrink_rows(&mut self.lines, to_remove, self.cursor.row, new_rows);
                let from_top = removed_top.len();
                for row in removed_top {
                    self.push_scrollback(row);
                }
                self.cursor.row = self.cursor.row.saturating_sub(from_top);
                self.saved_cursor.row = self.saved_cursor.row.saturating_sub(from_top);
                if !active_retains {
                    shift_placements(&mut self.placements, from_top);
                }
            }

            let bottom_line = self.scrollback.len() + new_rows;
            self.placements.retain(|p| p.line < bottom_line);
            self.alt_placements.retain(|p| p.line < bottom_line);
        }

        self.cols = new_cols;
        self.rows = new_rows;
        self.scroll_region_top = 0;
        self.scroll_region_bottom = new_rows.saturating_sub(1);
        self.mark_all_dirty();
        let max_row = new_rows.saturating_sub(1);
        let max_col = new_cols.saturating_sub(1);
        let clamp = |cursor: &mut Cursor| {
            cursor.row = cursor.row.min(max_row);
            cursor.col = cursor.col.min(max_col);
        };
        clamp(&mut self.cursor);
        clamp(&mut self.saved_cursor);
        if let Some(cursor) = self.alt_cursor.as_mut() {
            clamp(cursor);
        }
        if let Some(cursor) = self.alt_saved_cursor.as_mut() {
            clamp(cursor);
        }
        self.viewport_offset = self.viewport_offset.min(self.scrollback.len());
    }
}
