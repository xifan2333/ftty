//! Column reflow and line rewrapping algorithms for terminal width transitions.

use std::collections::{BTreeSet, HashMap, VecDeque};

use crate::grid::{Cell, CellFlags, Cursor, Grid, MAX_ROW_POOL_CAPACITY, Row};
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

#[derive(Clone, Copy, Debug)]
struct CursorReflowOffsets {
    old_cursor_abs: usize,
    old_saved_cursor_abs: usize,
    old_cursor_col: usize,
    old_saved_cursor_col: usize,
    old_view_top: Option<usize>,
}

impl CursorReflowOffsets {
    fn from_grid(grid: &Grid) -> Self {
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

        Self {
            old_cursor_abs,
            old_saved_cursor_abs,
            old_cursor_col,
            old_saved_cursor_col,
            old_view_top,
        }
    }
}

pub struct LogicalLine {
    cells: Vec<ReflowCell>,
    cursor_offset: Option<usize>,
    saved_cursor_offset: Option<usize>,
    view_top_offset: Option<usize>,
    placements: Vec<(usize, ImagePlacement)>,
    has_prompt_mark: bool,
}

#[derive(Default)]
struct LogicalLineBuilder {
    cells: Vec<ReflowCell>,
    cursor_offset: Option<usize>,
    saved_cursor_offset: Option<usize>,
    view_top_offset: Option<usize>,
    placements: Vec<(usize, ImagePlacement)>,
    has_prompt_mark: bool,
}

impl LogicalLineBuilder {
    fn is_empty(&self) -> bool {
        self.cells.is_empty()
            && self.cursor_offset.is_none()
            && self.saved_cursor_offset.is_none()
            && self.placements.is_empty()
    }

    fn finish(&mut self) -> LogicalLine {
        LogicalLine {
            cells: std::mem::take(&mut self.cells),
            cursor_offset: self.cursor_offset.take(),
            saved_cursor_offset: self.saved_cursor_offset.take(),
            view_top_offset: self.view_top_offset.take(),
            placements: std::mem::take(&mut self.placements),
            has_prompt_mark: std::mem::take(&mut self.has_prompt_mark),
        }
    }
}

fn is_trailing_blank(
    abs_row: usize,
    row: &Row,
    trailing_content: usize,
    offsets: &CursorReflowOffsets,
    builder: &LogicalLineBuilder,
) -> bool {
    abs_row > offsets.old_cursor_abs
        && abs_row > offsets.old_saved_cursor_abs
        && trailing_content == 0
        && !row.wrapped
        && !builder.has_prompt_mark
        && builder.placements.is_empty()
}

fn compute_row_content_len(
    row: &Row,
    abs_row: usize,
    trailing_content: usize,
    offsets: &CursorReflowOffsets,
) -> usize {
    if row.wrapped {
        row.cells.len()
    } else {
        let mut len = trailing_content;
        if abs_row == offsets.old_cursor_abs {
            len = len.max(offsets.old_cursor_col + 1);
        }
        if abs_row == offsets.old_saved_cursor_abs {
            len = len.max(offsets.old_saved_cursor_col + 1);
        }
        len
    }
}

fn update_offsets_for_cell(
    builder: &mut LogicalLineBuilder,
    col_idx: usize,
    abs_row: usize,
    offsets: &CursorReflowOffsets,
) {
    if abs_row == offsets.old_cursor_abs && col_idx == offsets.old_cursor_col {
        builder.cursor_offset = Some(builder.cells.len());
    }
    if abs_row == offsets.old_saved_cursor_abs && col_idx == offsets.old_saved_cursor_col {
        builder.saved_cursor_offset = Some(builder.cells.len());
    }
    if Some(abs_row) == offsets.old_view_top && builder.view_top_offset.is_none() {
        builder.view_top_offset = Some(builder.cells.len());
    }
}

fn finalize_row_offsets(
    builder: &mut LogicalLineBuilder,
    abs_row: usize,
    offsets: &CursorReflowOffsets,
) {
    if abs_row == offsets.old_cursor_abs && builder.cursor_offset.is_none() {
        builder.cursor_offset = Some(builder.cells.len());
    }
    if abs_row == offsets.old_saved_cursor_abs && builder.saved_cursor_offset.is_none() {
        builder.saved_cursor_offset = Some(builder.cells.len());
    }
    if Some(abs_row) == offsets.old_view_top && builder.view_top_offset.is_none() {
        builder.view_top_offset = Some(builder.cells.len());
    }
}

fn unpack_row_cells(
    builder: &mut LogicalLineBuilder,
    row: &Row,
    abs_row: usize,
    content_len: usize,
    offsets: &CursorReflowOffsets,
) {
    for (col_idx, cell) in row.cells[..content_len.min(row.cells.len())]
        .iter()
        .enumerate()
    {
        if cell.flags.contains(CellFlags::WRAP_SPACER) {
            continue;
        }
        update_offsets_for_cell(builder, col_idx, abs_row, offsets);
        let ph = row
            .placeholders
            .as_ref()
            .and_then(|m| m.get(&col_idx))
            .copied();
        builder
            .cells
            .push((*cell, ph, row.cluster_text(col_idx).map(ToOwned::to_owned)));
    }

    finalize_row_offsets(builder, abs_row, offsets);
}

fn collect_row_placements(builder: &mut LogicalLineBuilder, grid: &Grid, abs_row: usize) {
    for p in grid.graphics.placements() {
        if p.line == abs_row {
            builder.placements.push((builder.cells.len(), p.clone()));
        }
    }
}

fn unpack_logical_lines(grid: &Grid, offsets: &CursorReflowOffsets) -> Vec<LogicalLine> {
    let mut logical_lines = Vec::new();
    let mut builder = LogicalLineBuilder::default();

    for (abs_row, row) in grid.scrollback.iter().chain(grid.lines.iter()).enumerate() {
        if grid.has_prompt_mark_at(abs_row) {
            builder.has_prompt_mark = true;
        }

        collect_row_placements(&mut builder, grid, abs_row);

        let trailing_content = row
            .cells
            .iter()
            .rposition(|c| c != &Cell::default())
            .map_or(0, |idx| idx + 1);

        if is_trailing_blank(abs_row, row, trailing_content, offsets, &builder) {
            continue;
        }

        let content_len = compute_row_content_len(row, abs_row, trailing_content, offsets);
        unpack_row_cells(&mut builder, row, abs_row, content_len, offsets);

        if !row.wrapped {
            logical_lines.push(builder.finish());
        }
    }

    if !builder.is_empty() {
        logical_lines.push(builder.finish());
    }

    logical_lines
}

#[derive(Default)]
struct RewrappedLayout {
    rows: VecDeque<Row>,
    prompt_marks: BTreeSet<usize>,
    placements: Vec<ImagePlacement>,
    cursor_pos: Option<(usize, usize)>,
    saved_cursor_pos: Option<(usize, usize)>,
    view_top_row: Option<usize>,
}

fn rewrap_empty_logical_line(
    layout: &mut RewrappedLayout,
    lline: LogicalLine,
    new_cols: usize,
    total_evicted_rows: usize,
) {
    let row_idx = layout.rows.len();
    if lline.has_prompt_mark {
        layout.prompt_marks.insert(total_evicted_rows + row_idx);
    }
    if lline.cursor_offset.is_some() {
        layout.cursor_pos = Some((row_idx, 0));
    }
    if lline.saved_cursor_offset.is_some() {
        layout.saved_cursor_pos = Some((row_idx, 0));
    }
    if lline.view_top_offset.is_some() && layout.view_top_row.is_none() {
        layout.view_top_row = Some(row_idx);
    }
    for (_, mut p) in lline.placements {
        p.line = row_idx;
        layout.placements.push(p);
    }
    layout.rows.push_back(Row::new(new_cols));
}

fn compute_chunk_len(cells: &[ReflowCell], offset: usize, new_cols: usize) -> (usize, usize, bool) {
    let remaining = cells.len() - offset;
    let chunk_len = remaining.min(new_cols);
    let is_last_chunk = offset + chunk_len >= cells.len();

    let actual_len = if !is_last_chunk
        && chunk_len > 1
        && cells[offset + chunk_len - 1]
            .0
            .flags
            .contains(CellFlags::WIDE_CHAR)
    {
        chunk_len - 1
    } else {
        chunk_len
    };

    (chunk_len, actual_len, is_last_chunk)
}

fn map_chunk_cursors(
    layout: &mut RewrappedLayout,
    lline: &LogicalLine,
    offset: usize,
    actual_len: usize,
    is_last_chunk: bool,
    row_idx: usize,
    new_cols: usize,
) {
    let in_chunk = |target: usize| -> bool {
        target >= offset && (target < offset + actual_len || (is_last_chunk && target >= offset))
    };

    if let Some(co) = lline.cursor_offset
        && in_chunk(co)
    {
        layout.cursor_pos = Some((row_idx, (co - offset).min(new_cols - 1)));
    }
    if let Some(sco) = lline.saved_cursor_offset
        && in_chunk(sco)
    {
        layout.saved_cursor_pos = Some((row_idx, (sco - offset).min(new_cols - 1)));
    }
    if let Some(vto) = lline.view_top_offset
        && in_chunk(vto)
        && layout.view_top_row.is_none()
    {
        layout.view_top_row = Some(row_idx);
    }
}

fn map_chunk_placements(
    layout: &mut RewrappedLayout,
    placements: &[(usize, ImagePlacement)],
    offset: usize,
    actual_len: usize,
    is_last_chunk: bool,
    row_idx: usize,
) {
    let in_chunk = |target: usize| -> bool {
        target >= offset && (target < offset + actual_len || (is_last_chunk && target >= offset))
    };

    for (pl_offset, p) in placements {
        if in_chunk(*pl_offset) {
            let mut p_mapped = p.clone();
            p_mapped.line = row_idx;
            layout.placements.push(p_mapped);
        }
    }
}

fn build_rewrapped_row(
    cells: &[ReflowCell],
    new_cols: usize,
    actual_len: usize,
    chunk_len: usize,
    is_last_chunk: bool,
) -> Row {
    let mut row = Row::new(new_cols);
    for (i, (cell, ph, cluster)) in cells.iter().enumerate() {
        row.cells[i] = *cell;
        if let Some(coord) = ph {
            row.placeholders
                .get_or_insert_with(|| Box::new(HashMap::new()))
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
    row
}

fn rewrap_non_empty_logical_line(
    layout: &mut RewrappedLayout,
    lline: LogicalLine,
    new_cols: usize,
    total_evicted_rows: usize,
) {
    let mut offset = 0;
    while offset < lline.cells.len() {
        let (chunk_len, actual_len, is_last_chunk) =
            compute_chunk_len(&lline.cells, offset, new_cols);
        let row_idx = layout.rows.len();

        if offset == 0 && lline.has_prompt_mark {
            layout.prompt_marks.insert(total_evicted_rows + row_idx);
        }

        map_chunk_cursors(
            layout,
            &lline,
            offset,
            actual_len,
            is_last_chunk,
            row_idx,
            new_cols,
        );
        map_chunk_placements(
            layout,
            &lline.placements,
            offset,
            actual_len,
            is_last_chunk,
            row_idx,
        );

        let row = build_rewrapped_row(
            &lline.cells[offset..offset + actual_len],
            new_cols,
            actual_len,
            chunk_len,
            is_last_chunk,
        );
        layout.rows.push_back(row);

        offset += actual_len.max(1);
    }
}

fn rewrap_logical_lines(
    logical_lines: Vec<LogicalLine>,
    new_cols: usize,
    total_evicted_rows: usize,
) -> RewrappedLayout {
    let mut layout = RewrappedLayout::default();
    for lline in logical_lines {
        if lline.cells.is_empty() {
            rewrap_empty_logical_line(&mut layout, lline, new_cols, total_evicted_rows);
        } else {
            rewrap_non_empty_logical_line(&mut layout, lline, new_cols, total_evicted_rows);
        }
    }
    layout
}

fn partition_screen_and_scrollback(
    lines: &mut Vec<Row>,
    scrollback: &mut VecDeque<Row>,
    mut all_rows: VecDeque<Row>,
    new_rows: usize,
    cursor_target_row: usize,
) -> usize {
    let total_count = all_rows.len();
    if total_count > new_rows {
        let needed = total_count - new_rows;
        let rows_below_cursor = total_count
            .saturating_sub(1)
            .saturating_sub(cursor_target_row);
        let from_bottom = needed.min(rows_below_cursor);
        let from_top = needed - from_bottom;

        all_rows.truncate(total_count - from_bottom);
        lines.clear();
        lines.extend(all_rows.split_off(from_top));
        scrollback.clear();
        scrollback.extend(all_rows);
        from_top
    } else {
        lines.clear();
        lines.extend(all_rows);
        scrollback.clear();
        0
    }
}

fn enforce_scrollback_capacity(grid: &mut Grid, prompt_marks: &mut BTreeSet<usize>) -> usize {
    if grid.scrollback.len() <= grid.max_scrollback {
        return 0;
    }

    let excess = grid.scrollback.len() - grid.max_scrollback;
    for _ in 0..excess {
        if let Some(row) = grid.scrollback.pop_front() {
            grid.recycle_row(row);
        }
    }
    grid.total_evicted_rows = grid.total_evicted_rows.saturating_add(excess);
    while let Some(&first) = prompt_marks.first() {
        if first < grid.total_evicted_rows {
            prompt_marks.pop_first();
        } else {
            break;
        }
    }
    excess
}

fn relocate_cursor(
    cursor: &mut Cursor,
    new_pos: Option<(usize, usize)>,
    discarded_from_scrollback: usize,
    scrollback_count: usize,
    max_row: usize,
    max_col: usize,
) {
    if let Some((abs_row, col)) = new_pos {
        let adjusted_row = abs_row.saturating_sub(discarded_from_scrollback);
        if adjusted_row >= scrollback_count {
            cursor.row = (adjusted_row - scrollback_count).min(max_row);
        } else {
            cursor.row = 0;
        }
        cursor.col = col.min(max_col);
    } else {
        cursor.row = cursor.row.min(max_row);
        cursor.col = cursor.col.min(max_col);
    }
}

fn adjust_image_placements(
    grid: &mut Grid,
    mut placements: Vec<ImagePlacement>,
    discarded_from_scrollback: usize,
    new_rows: usize,
) {
    let total_screen_lines = grid.scrollback.len() + new_rows;
    placements.retain_mut(|p| {
        if p.line < discarded_from_scrollback {
            false
        } else {
            p.line -= discarded_from_scrollback;
            p.line < total_screen_lines
        }
    });
    grid.graphics.update_placements_after_reflow(placements);
}

fn commit_reflowed_layout(
    grid: &mut Grid,
    mut layout: RewrappedLayout,
    new_cols: usize,
    new_rows: usize,
) {
    while layout.rows.len() < new_rows {
        layout.rows.push_back(Row::new(new_cols));
    }

    let total_count = layout.rows.len();
    let cursor_target_row = layout
        .cursor_pos
        .map_or(total_count.saturating_sub(1), |(r, _)| r);

    let scrollback_count = partition_screen_and_scrollback(
        &mut grid.lines,
        &mut grid.scrollback,
        layout.rows,
        new_rows,
        cursor_target_row,
    );

    let discarded = enforce_scrollback_capacity(grid, &mut layout.prompt_marks);
    adjust_image_placements(grid, layout.placements, discarded, new_rows);

    let max_row = new_rows.saturating_sub(1);
    let max_col = new_cols.saturating_sub(1);

    relocate_cursor(
        &mut grid.cursor,
        layout.cursor_pos,
        discarded,
        scrollback_count,
        max_row,
        max_col,
    );
    relocate_cursor(
        &mut grid.saved_cursor,
        layout.saved_cursor_pos,
        discarded,
        scrollback_count,
        max_row,
        max_col,
    );

    grid.prompt_marks = layout.prompt_marks;
    grid.cols = new_cols;
    grid.rows = new_rows;
    grid.scroll_region_top = 0;
    grid.scroll_region_bottom = max_row;

    if let Some(vtr) = layout.view_top_row {
        let adjusted_vtr = vtr.saturating_sub(discarded);
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

/// Reflows the primary screen buffer and scrollback history across a new column width.
pub fn reflow_primary_screen(grid: &mut Grid, new_cols: usize, new_rows: usize) {
    let offsets = CursorReflowOffsets::from_grid(grid);
    let logical_lines = unpack_logical_lines(grid, &offsets);
    let layout = rewrap_logical_lines(logical_lines, new_cols, grid.total_evicted_rows);
    commit_reflowed_layout(grid, layout, new_cols, new_rows);
}
