//! Column reflow and line rewrapping algorithms for terminal width transitions.

use crate::grid::{Cell, CellFlags, Grid, MAX_ROW_POOL_CAPACITY, Row};
use crate::kitty::model::ImagePlacement;

pub fn shrink_rows(
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

/// Intermediate representation of a terminal cell during reflow, tracking flags, placeholders, and cluster text.
pub type ReflowCell = (Cell, Option<(u16, u16, u8)>, Option<String>);

/// Reflows the primary screen buffer and scrollback history across a new column width.
pub fn reflow_primary_screen(grid: &mut Grid, new_cols: usize, new_rows: usize) {
    let old_cursor_abs = grid.scrollback.len() + grid.cursor.row;
    let old_saved_cursor_abs = grid.scrollback.len() + grid.saved_cursor.row;
    let old_cursor_col = grid.cursor.col;
    let old_saved_cursor_col = grid.saved_cursor.col;

    let old_total_rows = grid.scrollback.len() + grid.lines.len();
    let old_view_top = if grid.viewport_offset > 0 {
        Some(old_total_rows.saturating_sub(grid.lines.len() + grid.viewport_offset))
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

    for (abs_row, row) in grid.scrollback.iter().chain(grid.lines.iter()).enumerate() {
        if grid.has_prompt_mark_at(abs_row) {
            current_has_prompt_mark = true;
        }

        for p in grid.graphics.placements() {
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
            .rposition(|c| c != &Cell::default())
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
            if cell.flags.contains(CellFlags::WRAP_SPACER) {
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
            current_cells.push((*cell, ph, row.cluster_text(col_idx).map(ToOwned::to_owned)));
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
                new_prompt_marks.insert(grid.total_evicted_rows + row_idx);
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
                    .contains(CellFlags::WIDE_CHAR)
            {
                chunk_len - 1
            } else {
                chunk_len
            };

            let row_idx = new_all_rows.len();
            if offset == 0 && lline.has_prompt_mark {
                new_prompt_marks.insert(grid.total_evicted_rows + row_idx);
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
                    && (*pl_offset < offset + actual_len || (is_last_chunk && *pl_offset >= offset))
                {
                    let mut p_mapped = p.clone();
                    p_mapped.line = row_idx;
                    new_placements.push(p_mapped);
                }
            }

            let mut row = Row::new(new_cols);
            for (i, (cell, ph, cluster)) in
                lline.cells[offset..offset + actual_len].iter().enumerate()
            {
                row.cells[i] = *cell;
                if let Some(coord) = ph {
                    row.placeholders
                        .get_or_insert_with(|| Box::new(std::collections::HashMap::new()))
                        .insert(i, *coord);
                }
                if let Some(text) = cluster {
                    row.set_cluster_text(i, text.clone());
                }
            }
            if actual_len < chunk_len {
                for cell in &mut row.cells[actual_len..chunk_len] {
                    cell.flags |= CellFlags::WRAP_SPACER;
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
        grid.lines.clear();
        grid.lines.extend(new_all_rows.split_off(from_top));
        grid.scrollback.clear();
        grid.scrollback.extend(new_all_rows);
        from_top
    } else {
        grid.lines.clear();
        grid.lines.extend(new_all_rows);
        grid.scrollback.clear();
        0
    };

    // Enforce max scrollback capacity
    let mut discarded_from_scrollback = 0;
    if grid.scrollback.len() > grid.max_scrollback {
        let excess = grid.scrollback.len() - grid.max_scrollback;
        for _ in 0..excess {
            if let Some(row) = grid.scrollback.pop_front() {
                grid.recycle_row(row);
            }
        }
        discarded_from_scrollback = excess;
        grid.total_evicted_rows = grid.total_evicted_rows.saturating_add(excess);
        while let Some(&first) = new_prompt_marks.first() {
            if first < grid.total_evicted_rows {
                new_prompt_marks.pop_first();
            } else {
                break;
            }
        }
    }

    // Update placements
    let total_screen_lines = grid.scrollback.len() + new_rows;
    new_placements.retain_mut(|p| {
        if p.line < discarded_from_scrollback {
            false
        } else {
            p.line -= discarded_from_scrollback;
            p.line < total_screen_lines
        }
    });
    grid.graphics.update_placements_after_reflow(new_placements);

    // Apply updated cursor
    let max_row = new_rows.saturating_sub(1);
    let max_col = new_cols.saturating_sub(1);

    if let Some((abs_row, col)) = new_cursor_pos {
        let adjusted_row = abs_row.saturating_sub(discarded_from_scrollback);
        if adjusted_row >= scrollback_count {
            grid.cursor.row = (adjusted_row - scrollback_count).min(max_row);
        } else {
            grid.cursor.row = 0;
        }
        grid.cursor.col = col.min(max_col);
    } else {
        grid.cursor.row = grid.cursor.row.min(max_row);
        grid.cursor.col = grid.cursor.col.min(max_col);
    }

    if let Some((abs_row, col)) = new_saved_cursor_pos {
        let adjusted_row = abs_row.saturating_sub(discarded_from_scrollback);
        if adjusted_row >= scrollback_count {
            grid.saved_cursor.row = (adjusted_row - scrollback_count).min(max_row);
        } else {
            grid.saved_cursor.row = 0;
        }
        grid.saved_cursor.col = col.min(max_col);
    } else {
        grid.saved_cursor.row = grid.saved_cursor.row.min(max_row);
        grid.saved_cursor.col = grid.saved_cursor.col.min(max_col);
    }

    grid.prompt_marks = new_prompt_marks;
    grid.cols = new_cols;
    grid.rows = new_rows;
    grid.scroll_region_top = 0;
    grid.scroll_region_bottom = max_row;

    if let Some(vtr) = new_view_top_row {
        let adjusted_vtr = vtr.saturating_sub(discarded_from_scrollback);
        grid.viewport_offset = grid
            .scrollback
            .len()
            .saturating_sub(adjusted_vtr)
            .min(grid.scrollback.len());
    } else {
        grid.viewport_offset = 0;
    }

    grid.mark_all_dirty();
    grid.row_pool.truncate(MAX_ROW_POOL_CAPACITY);
}

pub struct LogicalLine {
    cells: Vec<ReflowCell>,
    cursor_offset: Option<usize>,
    saved_cursor_offset: Option<usize>,
    view_top_offset: Option<usize>,
    placements: Vec<(usize, ImagePlacement)>,
    has_prompt_mark: bool,
}
