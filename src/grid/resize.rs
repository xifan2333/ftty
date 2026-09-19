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

        if self.alt_lines.is_none() && new_cols != self.cols {
            self.reflow_primary_screen(new_cols, new_rows);
            return;
        }

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
                    if from_top > 0 {
                        let mut shifted = std::collections::BTreeSet::new();
                        for &m in &self.prompt_marks {
                            if m >= from_top {
                                shifted.insert(m - from_top);
                            }
                        }
                        self.prompt_marks = shifted;
                    }
                }
            }

            let bottom_line = self.scrollback.len() + new_rows;
            self.placements.retain(|p| p.line < bottom_line);
            self.alt_placements.retain(|p| p.line < bottom_line);
            self.prompt_marks.retain(|&m| m < bottom_line);
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

    fn reflow_primary_screen(&mut self, new_cols: usize, new_rows: usize) {
        let old_cursor_abs = self.scrollback.len() + self.cursor.row;
        let old_saved_cursor_abs = self.scrollback.len() + self.saved_cursor.row;
        let old_cursor_col = self.cursor.col;
        let old_saved_cursor_col = self.saved_cursor.col;

        let mut logical_lines: Vec<LogicalLine> = Vec::new();
        let mut current_cells: Vec<crate::grid::Cell> = Vec::new();
        let mut current_cursor_offset: Option<usize> = None;
        let mut current_saved_cursor_offset: Option<usize> = None;
        let mut current_has_prompt_mark = false;

        for (abs_row, row) in self.scrollback.iter().chain(self.lines.iter()).enumerate() {
            if self.prompt_marks.contains(&abs_row) {
                current_has_prompt_mark = true;
            }

            let is_cursor_row = abs_row == old_cursor_abs;
            let is_saved_cursor_row = abs_row == old_saved_cursor_abs;

            let trailing_content = row
                .cells
                .iter()
                .rposition(|c| c != &crate::grid::Cell::default())
                .map_or(0, |idx| idx + 1);

            let is_trailing_blank = abs_row > old_cursor_abs
                && abs_row > old_saved_cursor_abs
                && trailing_content == 0
                && !row.wrapped
                && !current_has_prompt_mark;
            if is_trailing_blank {
                continue;
            }

            let content_len = if row.wrapped {
                row.cells.len()
            } else {
                let mut len = trailing_content;
                if is_cursor_row {
                    len = len.max(old_cursor_col + 1);
                }
                if is_saved_cursor_row {
                    len = len.max(old_saved_cursor_col + 1);
                }
                len
            };

            for (col_idx, cell) in row.cells[..content_len.min(row.cells.len())]
                .iter()
                .enumerate()
            {
                if cell.flags.contains(crate::grid::CellFlags::WRAP_SPACER) {
                    continue;
                }
                if is_cursor_row && col_idx == old_cursor_col {
                    current_cursor_offset = Some(current_cells.len());
                }
                if is_saved_cursor_row && col_idx == old_saved_cursor_col {
                    current_saved_cursor_offset = Some(current_cells.len());
                }
                current_cells.push(*cell);
            }

            if !row.wrapped {
                logical_lines.push(LogicalLine {
                    cells: std::mem::take(&mut current_cells),
                    cursor_offset: current_cursor_offset.take(),
                    saved_cursor_offset: current_saved_cursor_offset.take(),
                    has_prompt_mark: current_has_prompt_mark,
                });
                current_has_prompt_mark = false;
            }
        }

        if !current_cells.is_empty()
            || current_cursor_offset.is_some()
            || current_saved_cursor_offset.is_some()
        {
            logical_lines.push(LogicalLine {
                cells: current_cells,
                cursor_offset: current_cursor_offset,
                saved_cursor_offset: current_saved_cursor_offset,
                has_prompt_mark: current_has_prompt_mark,
            });
        }

        // Rewrap each logical line into rows of width new_cols
        let mut new_all_rows: std::collections::VecDeque<Row> = std::collections::VecDeque::new();
        let mut new_prompt_marks = std::collections::BTreeSet::new();
        let mut new_cursor_pos: Option<(usize, usize)> = None;
        let mut new_saved_cursor_pos: Option<(usize, usize)> = None;

        for lline in logical_lines {
            if lline.cells.is_empty() {
                let row_idx = new_all_rows.len();
                if lline.has_prompt_mark {
                    new_prompt_marks.insert(row_idx);
                }
                if lline.cursor_offset.is_some() {
                    new_cursor_pos = Some((row_idx, 0));
                }
                if lline.saved_cursor_offset.is_some() {
                    new_saved_cursor_pos = Some((row_idx, 0));
                }
                new_all_rows.push_back(Row::new(new_cols));
                continue;
            }

            let mut offset = 0;
            while offset < lline.cells.len() {
                let remaining = lline.cells.len() - offset;
                let chunk_len = remaining.min(new_cols);
                let is_last_chunk = offset + chunk_len >= lline.cells.len();

                // Prevent splitting wide characters at the wrap boundary
                let actual_len = if !is_last_chunk
                    && chunk_len > 0
                    && lline.cells[offset + chunk_len - 1]
                        .flags
                        .contains(crate::grid::CellFlags::WIDE_CHAR)
                {
                    chunk_len - 1
                } else {
                    chunk_len
                };

                let row_idx = new_all_rows.len();
                if offset == 0 && lline.has_prompt_mark {
                    new_prompt_marks.insert(row_idx);
                }

                if let Some(co) = lline.cursor_offset
                    && co >= offset
                    && (co < offset + actual_len || (is_last_chunk && co >= offset))
                {
                    new_cursor_pos = Some((row_idx, (co - offset).min(new_cols - 1)));
                }
                if let Some(sco) = lline.saved_cursor_offset
                    && sco >= offset
                    && (sco < offset + actual_len || (is_last_chunk && sco >= offset))
                {
                    new_saved_cursor_pos = Some((row_idx, (sco - offset).min(new_cols - 1)));
                }

                let mut row = Row::new(new_cols);
                row.cells[..actual_len].copy_from_slice(&lline.cells[offset..offset + actual_len]);
                if actual_len < chunk_len {
                    for cell in &mut row.cells[actual_len..chunk_len] {
                        cell.flags |= crate::grid::CellFlags::WRAP_SPACER;
                    }
                }
                row.wrapped = !is_last_chunk;
                new_all_rows.push_back(row);

                offset += actual_len.max(1);
            }
        }

        // Pad with empty rows to satisfy at least new_rows
        while new_all_rows.len() < new_rows {
            new_all_rows.push_back(Row::new(new_cols));
        }

        let total_count = new_all_rows.len();
        let scrollback_count = total_count.saturating_sub(new_rows);

        self.lines.clear();
        self.lines.extend(new_all_rows.drain(scrollback_count..));

        self.scrollback.clear();
        self.scrollback.extend(new_all_rows);

        // Enforce max scrollback capacity
        if self.scrollback.len() > self.max_scrollback {
            let excess = self.scrollback.len() - self.max_scrollback;
            self.scrollback.drain(0..excess);
            let mut shifted = std::collections::BTreeSet::new();
            for &m in &new_prompt_marks {
                if m >= excess {
                    shifted.insert(m - excess);
                }
            }
            new_prompt_marks = shifted;
        }

        // Apply updated cursor
        let max_row = new_rows.saturating_sub(1);
        let max_col = new_cols.saturating_sub(1);

        if let Some((abs_row, col)) = new_cursor_pos {
            if abs_row >= scrollback_count {
                self.cursor.row = (abs_row - scrollback_count).min(max_row);
            } else {
                self.cursor.row = 0;
            }
            self.cursor.col = col.min(max_col);
        } else {
            self.cursor.row = self.cursor.row.min(max_row);
            self.cursor.col = self.cursor.col.min(max_col);
        }

        if let Some((abs_row, col)) = new_saved_cursor_pos {
            if abs_row >= scrollback_count {
                self.saved_cursor.row = (abs_row - scrollback_count).min(max_row);
            } else {
                self.saved_cursor.row = 0;
            }
            self.saved_cursor.col = col.min(max_col);
        } else {
            self.saved_cursor.row = self.saved_cursor.row.min(max_row);
            self.saved_cursor.col = self.saved_cursor.col.min(max_col);
        }

        self.prompt_marks = new_prompt_marks;
        self.cols = new_cols;
        self.rows = new_rows;
        self.scroll_region_top = 0;
        self.scroll_region_bottom = max_row;
        self.viewport_offset = self.viewport_offset.min(self.scrollback.len());
        self.mark_all_dirty();
    }
}

struct LogicalLine {
    cells: Vec<crate::grid::Cell>,
    cursor_offset: Option<usize>,
    saved_cursor_offset: Option<usize>,
    has_prompt_mark: bool,
}
