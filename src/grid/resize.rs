//! Terminal grid dimensions resizing, scrollback line restoration, and placement adjustment.

use crate::grid::{Cursor, Grid, MAX_ROW_POOL_CAPACITY, Row};
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

type ReflowCell = (crate::grid::Cell, Option<(u16, u16, u8)>);

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
                let row = self.alloc_row(new_cols);
                self.lines.push(row);
            }
            if self.alt_lines.is_some() {
                for _ in old_rows..new_rows {
                    let row = self.alloc_row(new_cols);
                    if let Some(alt) = &mut self.alt_lines {
                        alt.push(row);
                    }
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
                        self.total_evicted_rows = self.total_evicted_rows.saturating_add(from_top);
                        while let Some(&first) = self.prompt_marks.first() {
                            if first < self.total_evicted_rows {
                                self.prompt_marks.pop_first();
                            } else {
                                break;
                            }
                        }
                    }
                }
            }

            let bottom_line = self.scrollback.len() + new_rows;
            let abs_bottom = self.total_evicted_rows + bottom_line;
            self.placements.retain(|p| p.line < bottom_line);
            self.alt_placements.retain(|p| p.line < bottom_line);
            self.prompt_marks.retain(|&m| m < abs_bottom);
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
        self.row_pool.truncate(MAX_ROW_POOL_CAPACITY);
    }

    fn reflow_primary_screen(&mut self, new_cols: usize, new_rows: usize) {
        let old_cursor_abs = self.scrollback.len() + self.cursor.row;
        let old_saved_cursor_abs = self.scrollback.len() + self.saved_cursor.row;
        let old_cursor_col = self.cursor.col;
        let old_saved_cursor_col = self.saved_cursor.col;

        let old_total_rows = self.scrollback.len() + self.lines.len();
        let old_view_top = if self.viewport_offset > 0 {
            Some(old_total_rows.saturating_sub(self.lines.len() + self.viewport_offset))
        } else {
            None
        };

        let mut logical_lines: Vec<LogicalLine> = Vec::new();
        let mut current_cells: Vec<ReflowCell> = Vec::new();
        let mut current_cursor_offset: Option<usize> = None;
        let mut current_saved_cursor_offset: Option<usize> = None;
        let mut current_view_top_offset: Option<usize> = None;
        let mut current_placements: Vec<(usize, ImagePlacement)> = Vec::new();
        let mut current_has_prompt_mark = false;

        for (abs_row, row) in self.scrollback.iter().chain(self.lines.iter()).enumerate() {
            if self.has_prompt_mark_at(abs_row) {
                current_has_prompt_mark = true;
            }

            for p in &self.placements {
                if p.line == abs_row {
                    current_placements.push((current_cells.len(), p.clone()));
                }
            }

            let is_cursor_row = abs_row == old_cursor_abs;
            let is_saved_cursor_row = abs_row == old_saved_cursor_abs;
            let is_view_top_row = Some(abs_row) == old_view_top;

            let trailing_content = row
                .cells
                .iter()
                .rposition(|c| c != &crate::grid::Cell::default())
                .map_or(0, |idx| idx + 1);

            let is_trailing_blank = abs_row > old_cursor_abs
                && abs_row > old_saved_cursor_abs
                && trailing_content == 0
                && !row.wrapped
                && !current_has_prompt_mark
                && current_placements.is_empty();
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
                if is_view_top_row && current_view_top_offset.is_none() {
                    current_view_top_offset = Some(current_cells.len());
                }
                let ph = row
                    .placeholders
                    .as_ref()
                    .and_then(|m| m.get(&col_idx))
                    .copied();
                current_cells.push((*cell, ph));
            }

            if is_cursor_row && current_cursor_offset.is_none() {
                current_cursor_offset = Some(current_cells.len());
            }
            if is_saved_cursor_row && current_saved_cursor_offset.is_none() {
                current_saved_cursor_offset = Some(current_cells.len());
            }
            if is_view_top_row && current_view_top_offset.is_none() {
                current_view_top_offset = Some(current_cells.len());
            }

            if !row.wrapped {
                logical_lines.push(LogicalLine {
                    cells: std::mem::take(&mut current_cells),
                    cursor_offset: current_cursor_offset.take(),
                    saved_cursor_offset: current_saved_cursor_offset.take(),
                    view_top_offset: current_view_top_offset.take(),
                    placements: std::mem::take(&mut current_placements),
                    has_prompt_mark: current_has_prompt_mark,
                });
                current_has_prompt_mark = false;
            }
        }

        if !current_cells.is_empty()
            || current_cursor_offset.is_some()
            || current_saved_cursor_offset.is_some()
            || !current_placements.is_empty()
        {
            logical_lines.push(LogicalLine {
                cells: current_cells,
                cursor_offset: current_cursor_offset,
                saved_cursor_offset: current_saved_cursor_offset,
                view_top_offset: current_view_top_offset,
                placements: current_placements,
                has_prompt_mark: current_has_prompt_mark,
            });
        }

        // Rewrap each logical line into rows of width new_cols
        let mut new_all_rows: std::collections::VecDeque<Row> = std::collections::VecDeque::new();
        let mut new_prompt_marks = std::collections::BTreeSet::new();
        let mut new_placements: Vec<ImagePlacement> = Vec::new();
        let mut new_cursor_pos: Option<(usize, usize)> = None;
        let mut new_saved_cursor_pos: Option<(usize, usize)> = None;
        let mut new_view_top_row: Option<usize> = None;

        for lline in logical_lines {
            if lline.cells.is_empty() {
                let row_idx = new_all_rows.len();
                if lline.has_prompt_mark {
                    new_prompt_marks.insert(self.total_evicted_rows + row_idx);
                }
                if lline.cursor_offset.is_some() {
                    new_cursor_pos = Some((row_idx, 0));
                }
                if lline.saved_cursor_offset.is_some() {
                    new_saved_cursor_pos = Some((row_idx, 0));
                }
                if lline.view_top_offset.is_some() && new_view_top_row.is_none() {
                    new_view_top_row = Some(row_idx);
                }
                for (_, mut p) in lline.placements {
                    p.line = row_idx;
                    new_placements.push(p);
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
                    && chunk_len > 1
                    && lline.cells[offset + chunk_len - 1]
                        .0
                        .flags
                        .contains(crate::grid::CellFlags::WIDE_CHAR)
                {
                    chunk_len - 1
                } else {
                    chunk_len
                };

                let row_idx = new_all_rows.len();
                if offset == 0 && lline.has_prompt_mark {
                    new_prompt_marks.insert(self.total_evicted_rows + row_idx);
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
                if let Some(vto) = lline.view_top_offset
                    && vto >= offset
                    && (vto < offset + actual_len || (is_last_chunk && vto >= offset))
                    && new_view_top_row.is_none()
                {
                    new_view_top_row = Some(row_idx);
                }
                for (pl_offset, p) in &lline.placements {
                    if *pl_offset >= offset
                        && (*pl_offset < offset + actual_len
                            || (is_last_chunk && *pl_offset >= offset))
                    {
                        let mut p_mapped = p.clone();
                        p_mapped.line = row_idx;
                        new_placements.push(p_mapped);
                    }
                }

                let mut row = Row::new(new_cols);
                for (i, (cell, ph)) in lline.cells[offset..offset + actual_len].iter().enumerate() {
                    row.cells[i] = *cell;
                    if let Some(coord) = ph {
                        row.placeholders
                            .get_or_insert_with(|| Box::new(std::collections::HashMap::new()))
                            .insert(i, *coord);
                    }
                }
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
        let cursor_target_row = new_cursor_pos.map_or(total_count.saturating_sub(1), |(r, _)| r);

        let scrollback_count = if total_count > new_rows {
            let needed = total_count - new_rows;
            let rows_below_cursor = total_count
                .saturating_sub(1)
                .saturating_sub(cursor_target_row);
            let from_bottom = needed.min(rows_below_cursor);
            let from_top = needed - from_bottom;

            new_all_rows.truncate(total_count - from_bottom);
            self.lines.clear();
            self.lines.extend(new_all_rows.split_off(from_top));
            self.scrollback.clear();
            self.scrollback.extend(new_all_rows);
            from_top
        } else {
            self.lines.clear();
            self.lines.extend(new_all_rows);
            self.scrollback.clear();
            0
        };

        // Enforce max scrollback capacity
        let mut discarded_from_scrollback = 0;
        if self.scrollback.len() > self.max_scrollback {
            let excess = self.scrollback.len() - self.max_scrollback;
            for _ in 0..excess {
                if let Some(row) = self.scrollback.pop_front() {
                    self.recycle_row(row);
                }
            }
            discarded_from_scrollback = excess;
            self.total_evicted_rows = self.total_evicted_rows.saturating_add(excess);
            while let Some(&first) = new_prompt_marks.first() {
                if first < self.total_evicted_rows {
                    new_prompt_marks.pop_first();
                } else {
                    break;
                }
            }
        }

        // Update placements
        let total_screen_lines = self.scrollback.len() + new_rows;
        new_placements.retain_mut(|p| {
            if p.line < discarded_from_scrollback {
                false
            } else {
                p.line -= discarded_from_scrollback;
                p.line < total_screen_lines
            }
        });
        self.placements = new_placements;

        // Apply updated cursor
        let max_row = new_rows.saturating_sub(1);
        let max_col = new_cols.saturating_sub(1);

        if let Some((abs_row, col)) = new_cursor_pos {
            let adjusted_row = abs_row.saturating_sub(discarded_from_scrollback);
            if adjusted_row >= scrollback_count {
                self.cursor.row = (adjusted_row - scrollback_count).min(max_row);
            } else {
                self.cursor.row = 0;
            }
            self.cursor.col = col.min(max_col);
        } else {
            self.cursor.row = self.cursor.row.min(max_row);
            self.cursor.col = self.cursor.col.min(max_col);
        }

        if let Some((abs_row, col)) = new_saved_cursor_pos {
            let adjusted_row = abs_row.saturating_sub(discarded_from_scrollback);
            if adjusted_row >= scrollback_count {
                self.saved_cursor.row = (adjusted_row - scrollback_count).min(max_row);
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

        if let Some(vtr) = new_view_top_row {
            let adjusted_vtr = vtr.saturating_sub(discarded_from_scrollback);
            self.viewport_offset = self
                .scrollback
                .len()
                .saturating_sub(adjusted_vtr)
                .min(self.scrollback.len());
        } else {
            self.viewport_offset = 0;
        }

        self.mark_all_dirty();
        self.row_pool.truncate(MAX_ROW_POOL_CAPACITY);
    }
}

struct LogicalLine {
    cells: Vec<ReflowCell>,
    cursor_offset: Option<usize>,
    saved_cursor_offset: Option<usize>,
    view_top_offset: Option<usize>,
    placements: Vec<(usize, ImagePlacement)>,
    has_prompt_mark: bool,
}
