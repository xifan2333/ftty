//! Terminal cell grid, cursor management, and scrollback ring buffer.

use std::collections::{BTreeSet, HashMap, VecDeque};

use crate::color::Color;
use crate::kitty::model::{DeleteTarget, ImageData, ImagePlacement};
use crate::width::{
    AMBIGUOUS_NARROW, AMBIGUOUS_WIDE, KITTY_PLACEHOLDER, continues_grapheme,
    grapheme_display_width, wcwidth,
};

use diacritics::diacritic_to_index;

mod cell;
mod cursor;
pub mod diacritics;
mod row;

pub use cell::{Cell, CellFlags, ClearMode};
pub use cursor::{Cursor, CursorShape};
pub use row::{PlaceholderMap, Row};

pub(crate) use std::cell::Cell as DirtyCell;

#[doc(hidden)]
pub const MAX_ROW_POOL_CAPACITY: usize = 64;

/// 2D Screen grid with scrollback history and alternate screen support.
#[derive(Debug, Clone)]
pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    pub max_scrollback: usize,

    pub lines: Vec<Row>,
    pub scrollback: VecDeque<Row>,
    pub viewport_offset: usize,

    /// Kitty graphics image decoding, spatial placement indexing, and memory budgeting subsystem.
    pub graphics: crate::kitty::GraphicStore,

    pub cursor: Cursor,
    pub saved_cursor: Cursor,

    pub scroll_region_top: usize,
    pub scroll_region_bottom: usize,
    pub prompt_marks: BTreeSet<usize>,
    pub total_evicted_rows: usize,

    // The hidden primary screen while the alternate screen is active. Each screen owns its saved
    // cursor and placements so a resize on one cannot shift the other's coordinates.
    pub(crate) alt_lines: Option<Vec<Row>>,
    pub(crate) alt_cursor: Option<Cursor>,
    pub(crate) alt_saved_cursor: Option<Cursor>,

    // Recycling pool for evicted or discarded rows to eliminate Vec<Cell> allocations.
    #[doc(hidden)]
    pub row_pool: Vec<Row>,

    /// Root cell of the grapheme cluster currently receiving attachments.
    pub(crate) cluster_anchor: Option<(usize, usize)>,
    /// Reusable scratch buffer for grapheme boundary checks.
    pub(crate) scratch_cluster: String,
    /// Width model for East Asian Ambiguous characters.
    pub(crate) ambiguous_width: u8,
}

impl Grid {
    #[must_use]
    pub fn new(cols: usize, rows: usize, max_scrollback: usize) -> Self {
        let actual_cols = cols.max(1);
        let actual_rows = rows.max(1);
        let lines = (0..actual_rows).map(|_| Row::new(actual_cols)).collect();

        Self {
            cols: actual_cols,
            rows: actual_rows,
            max_scrollback,
            lines,
            scrollback: VecDeque::new(),
            viewport_offset: 0,
            graphics: crate::kitty::GraphicStore::new(),
            cursor: Cursor::default(),
            saved_cursor: Cursor::default(),
            scroll_region_top: 0,
            scroll_region_bottom: actual_rows.saturating_sub(1),
            prompt_marks: BTreeSet::new(),
            total_evicted_rows: 0,
            alt_lines: None,
            alt_cursor: None,
            alt_saved_cursor: None,
            row_pool: Vec::new(),
            cluster_anchor: None,
            scratch_cluster: String::with_capacity(32),
            ambiguous_width: AMBIGUOUS_NARROW,
        }
    }

    #[must_use]
    pub fn is_alt_screen(&self) -> bool {
        self.alt_lines.is_some()
    }

    /// Recycles a discarded row into the row buffer pool if capacity allows.
    ///
    /// If the pool reaches [`MAX_ROW_POOL_CAPACITY`], the incoming row is dropped without allocation.
    /// Cell reset is deferred until checkout via [`alloc_row`] or rotation reset to avoid double clears.
    #[doc(hidden)]
    pub fn recycle_row(&mut self, row: Row) {
        if self.row_pool.len() < MAX_ROW_POOL_CAPACITY {
            self.row_pool.push(row);
        }
    }

    /// Obtains a row from the buffer pool or allocates a new one, ensuring the returned
    /// row matches `cols` and has all cells reset to default.
    #[must_use]
    #[doc(hidden)]
    pub fn alloc_row(&mut self, cols: usize) -> Row {
        self.alloc_row_with_bg(cols, Color::DefaultBackground)
    }

    #[must_use]
    pub fn alloc_row_with_bg(&mut self, cols: usize, bg: Color) -> Row {
        if let Some(mut row) = self.row_pool.pop() {
            if row.cells.len() != cols {
                row.resize(cols);
            }
            row.reset_with_bg(bg);
            row
        } else {
            Row::blank(cols, bg)
        }
    }

    /// Returns the sum of decoded RGBA byte sizes across all stored images.
    #[must_use]
    pub fn total_image_bytes(&self) -> usize {
        self.graphics.total_image_bytes()
    }

    /// Read-only view of stored images keyed by image id.
    #[must_use]
    pub fn images(&self) -> &HashMap<u32, ImageData> {
        self.graphics.images()
    }

    /// Read-only view of image placements.
    #[must_use]
    pub fn placements(&self) -> &[ImagePlacement] {
        self.graphics.placements()
    }

    /// Returns `true` if an image with the given id is currently stored.
    #[must_use]
    pub fn contains_image(&self, id: u32) -> bool {
        self.graphics.contains_image(id)
    }

    /// Adds a decoded image to the graphic store.
    pub fn add_image(&mut self, image: ImageData) {
        let active = crate::kitty::collect_placeholder_ids(
            self.lines
                .iter()
                .chain(self.alt_lines.iter().flatten())
                .chain(self.scrollback.iter()),
        );
        self.graphics.add_image(image, &active);
    }

    /// Adds an image placement instance anchored to grid cells with FIFO eviction.
    pub fn add_placement(&mut self, placement: ImagePlacement) {
        self.graphics.add_placement(placement);
    }

    /// Deletes images and/or placements matching the given delete target.
    pub fn delete_images(&mut self, target: DeleteTarget) {
        let abs_line = self.scrollback.len() + self.cursor.row;
        self.graphics
            .delete_images(target, abs_line, self.cursor.col);
        self.mark_visible_dirty();
    }

    pub fn enter_alt_screen(&mut self) {
        self.reset_width_state();
        if self.alt_lines.is_none() {
            let alt = (0..self.rows).map(|_| Row::new(self.cols)).collect();
            self.alt_lines = Some(std::mem::replace(&mut self.lines, alt));
            self.alt_cursor = Some(self.cursor);
            self.alt_saved_cursor = Some(self.saved_cursor);
            self.cursor = Cursor::default();
            self.saved_cursor = Cursor::default();
            // Each screen owns its placements; the primary's are parked until it is restored.
            self.graphics.switch_to_alt_screen();
            self.viewport_offset = 0;
        }
    }

    /// Restores the primary screen buffer.
    pub fn exit_alt_screen(&mut self) {
        self.reset_width_state();
        if let Some(primary) = self.alt_lines.take() {
            self.lines = primary;
            if let Some(cursor) = self.alt_cursor.take() {
                self.cursor = cursor;
            }
            if let Some(saved) = self.alt_saved_cursor.take() {
                self.saved_cursor = saved;
            }
            self.graphics.restore_from_alt_screen();
            self.viewport_offset = 0;
            self.mark_all_dirty();
        }
    }

    /// Marks all rows in the visible screen, alternate screen, and currently displayed viewport rows as dirty.
    pub fn mark_all_dirty(&self) {
        for row in &self.lines {
            row.dirty.set(true);
        }
        if let Some(alt) = &self.alt_lines {
            for row in alt {
                row.dirty.set(true);
            }
        }
        if self.viewport_offset > 0 {
            self.mark_visible_dirty();
        }
    }

    /// Marks all rows currently displayed in the visible viewport as dirty without traversing full scrollback history.
    pub fn mark_visible_dirty(&self) {
        for r in 0..self.rows {
            self.visible_line(r).dirty.set(true);
        }
    }

    pub(crate) fn evict_oldest_scrollback_row(&mut self) -> Option<Row> {
        let row = self.scrollback.pop_front()?;
        self.graphics.shift_on_scroll_up(1);
        self.total_evicted_rows = self.total_evicted_rows.saturating_add(1);
        while let Some(&first) = self.prompt_marks.first() {
            if first < self.total_evicted_rows {
                self.prompt_marks.pop_first();
            } else {
                break;
            }
        }
        Some(row)
    }

    /// Records an absolute line marker for OSC 133 semantic prompt navigation.
    pub fn add_prompt_mark(&mut self, line: usize) {
        if self.prompt_marks.len() >= 1024
            && let Some(&first) = self.prompt_marks.iter().next()
        {
            self.prompt_marks.remove(&first);
        }
        self.prompt_marks.insert(self.total_evicted_rows + line);
    }

    /// Returns `true` if a prompt marker exists at the specified line relative to current scrollback.
    #[must_use]
    pub fn has_prompt_mark_at(&self, line: usize) -> bool {
        self.prompt_marks
            .contains(&(self.total_evicted_rows + line))
    }

    pub(crate) fn shift_region_marks_up(
        &mut self,
        top_row: usize,
        bottom_row: usize,
        count: usize,
    ) {
        if self.prompt_marks.is_empty() || count == 0 {
            return;
        }
        let sb_len = self.scrollback.len();
        let abs_top = self.total_evicted_rows + sb_len + top_row;
        let abs_bottom = self.total_evicted_rows + sb_len + bottom_row;
        let mut new_marks = BTreeSet::new();
        for &mark in &self.prompt_marks {
            if mark < abs_top || mark > abs_bottom {
                new_marks.insert(mark);
            } else if mark >= abs_top + count {
                new_marks.insert(mark - count);
            }
        }
        self.prompt_marks = new_marks;
    }

    pub(crate) fn shift_region_marks_down(
        &mut self,
        top_row: usize,
        bottom_row: usize,
        count: usize,
    ) {
        if self.prompt_marks.is_empty() || count == 0 {
            return;
        }
        let sb_len = self.scrollback.len();
        let abs_top = self.total_evicted_rows + sb_len + top_row;
        let abs_bottom = self.total_evicted_rows + sb_len + bottom_row;
        let mut new_marks = BTreeSet::new();
        for &mark in &self.prompt_marks {
            if mark < abs_top || mark > abs_bottom {
                new_marks.insert(mark);
            } else if mark + count <= abs_bottom {
                new_marks.insert(mark + count);
            }
        }
        self.prompt_marks = new_marks;
    }

    /// Scrolls the viewport up to the previous semantic prompt boundary.
    pub fn scroll_to_prompt_prev(&mut self) {
        if self.is_alt_screen() || self.prompt_marks.is_empty() {
            return;
        }
        let top_line = self
            .total_evicted_rows
            .saturating_add(self.scrollback.len().saturating_sub(self.viewport_offset));
        if let Some(&mark) = self.prompt_marks.range(..top_line).next_back() {
            let rel_mark = mark.saturating_sub(self.total_evicted_rows);
            let new_offset = self
                .scrollback
                .len()
                .saturating_sub(rel_mark)
                .min(self.scrollback.len());
            if new_offset != self.viewport_offset {
                self.viewport_offset = new_offset;
                self.mark_visible_dirty();
            }
        } else if let Some(&first) = self.prompt_marks.iter().next() {
            let rel_mark = first.saturating_sub(self.total_evicted_rows);
            let new_offset = self
                .scrollback
                .len()
                .saturating_sub(rel_mark)
                .min(self.scrollback.len());
            if new_offset != self.viewport_offset {
                self.viewport_offset = new_offset;
                self.mark_visible_dirty();
            }
        }
    }

    /// Scrolls the viewport down to the next semantic prompt boundary.
    pub fn scroll_to_prompt_next(&mut self) {
        if self.is_alt_screen() || self.viewport_offset == 0 {
            return;
        }
        let top_line = self
            .total_evicted_rows
            .saturating_add(self.scrollback.len().saturating_sub(self.viewport_offset));
        if let Some(&mark) = self.prompt_marks.range(top_line + 1..).next() {
            let rel_mark = mark.saturating_sub(self.total_evicted_rows);
            let new_offset = self.scrollback.len().saturating_sub(rel_mark);
            if new_offset != self.viewport_offset {
                self.viewport_offset = new_offset;
                self.mark_visible_dirty();
            }
        } else {
            self.scroll_viewport_bottom();
        }
    }

    pub(crate) fn push_scrollback(&mut self, row: Row) {
        if self.max_scrollback == 0 {
            self.recycle_row(row);
            return;
        }
        if self.scrollback.len() >= self.max_scrollback
            && let Some(evicted) = self.evict_oldest_scrollback_row()
        {
            self.recycle_row(evicted);
        }
        self.scrollback.push_back(row);
        // If user is currently viewing history, keep the view anchored on the same lines
        if self.viewport_offset > 0 {
            self.viewport_offset = (self.viewport_offset + 1).min(self.scrollback.len());
            self.mark_visible_dirty();
        }
    }

    /// Scrolls the viewport up by `delta` lines to view older history.
    pub fn scroll_viewport_up(&mut self, delta: usize) {
        if self.is_alt_screen() {
            return;
        }
        let max_offset = self.scrollback.len();
        let new_offset = self.viewport_offset.saturating_add(delta).min(max_offset);
        if new_offset != self.viewport_offset {
            self.viewport_offset = new_offset;
            self.mark_visible_dirty();
        }
    }

    /// Scrolls the viewport down by `delta` lines toward the active screen.
    pub fn scroll_viewport_down(&mut self, delta: usize) {
        if self.is_alt_screen() || self.viewport_offset == 0 {
            return;
        }
        let new_offset = self.viewport_offset.saturating_sub(delta);
        if new_offset != self.viewport_offset {
            self.viewport_offset = new_offset;
            self.mark_visible_dirty();
        }
    }

    /// Jumps the viewport to the earliest line in the scrollback history.
    pub fn scroll_viewport_top(&mut self) {
        if self.is_alt_screen() {
            return;
        }
        let max_offset = self.scrollback.len();
        if self.viewport_offset != max_offset {
            self.viewport_offset = max_offset;
            self.mark_visible_dirty();
        }
    }

    /// Resets the viewport offset to 0 (bottom of active screen).
    pub fn scroll_viewport_bottom(&mut self) {
        if self.viewport_offset != 0 {
            self.viewport_offset = 0;
            self.mark_visible_dirty();
        }
    }

    #[must_use]
    pub fn viewport_offset(&self) -> usize {
        self.viewport_offset
    }

    /// Returns a reference to the row currently displayed at the given screen row.
    #[must_use]
    pub fn visible_line(&self, row: usize) -> &Row {
        let h = self.scrollback.len();
        let offset = self.viewport_offset.min(h);
        if offset == 0 || self.is_alt_screen() {
            return &self.lines[row];
        }
        let abs_idx = h + row - offset;
        if abs_idx < h {
            &self.scrollback[abs_idx]
        } else {
            &self.lines[abs_idx - h]
        }
    }

    /// Extracts all text currently displayed in the visible viewport.
    #[must_use]
    pub fn extract_visible_text(&self) -> String {
        let mut result = String::new();
        let mut line_str = String::with_capacity(self.cols);
        for r in 0..self.rows {
            let row = self.visible_line(r);
            line_str.clear();
            for (col, cell) in row.cells.iter().enumerate() {
                if !cell
                    .flags
                    .intersects(CellFlags::WIDE_CHAR_SPACER | CellFlags::WRAP_SPACER)
                {
                    if let Some(text) = row.cluster_text(col) {
                        line_str.push_str(text);
                    } else {
                        line_str.push(cell.c);
                    }
                }
            }
            if row.wrapped {
                result.push_str(&line_str);
            } else {
                result.push_str(line_str.trim_end());
                result.push('\n');
            }
        }
        result
    }

    /// Extracts all text from the start of scrollback history through visible screen lines.
    #[must_use]
    pub fn extract_scrollback_text(&self) -> String {
        let mut result = String::new();
        let mut line_str = String::with_capacity(self.cols);
        for row in self.scrollback.iter().chain(self.lines.iter()) {
            line_str.clear();
            for (col, cell) in row.cells.iter().enumerate() {
                if !cell
                    .flags
                    .intersects(CellFlags::WIDE_CHAR_SPACER | CellFlags::WRAP_SPACER)
                {
                    if let Some(text) = row.cluster_text(col) {
                        line_str.push_str(text);
                    } else {
                        line_str.push(cell.c);
                    }
                }
            }
            if row.wrapped {
                result.push_str(&line_str);
            } else {
                result.push_str(line_str.trim_end());
                result.push('\n');
            }
        }
        result
    }

    /// Sets the top and bottom scrolling margins (0-indexed).
    pub fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        let top = top.min(self.rows.saturating_sub(1));
        let bottom = bottom.min(self.rows.saturating_sub(1));
        if top < bottom {
            self.scroll_region_top = top;
            self.scroll_region_bottom = bottom;
        }
    }

    pub fn reset_scroll_region(&mut self) {
        self.scroll_region_top = 0;
        self.scroll_region_bottom = self.rows.saturating_sub(1);
    }

    /// Scrolls lines inside the active scroll region upward with default background.
    pub fn scroll_up(&mut self, count: usize) {
        self.scroll_up_with_bg(count, Color::DefaultBackground);
    }

    /// Scrolls lines inside the active scroll region upward with the specified background color (BCE).
    pub fn scroll_up_with_bg(&mut self, count: usize, bg: Color) {
        self.reset_width_state();
        let region_len = self
            .scroll_region_bottom
            .saturating_sub(self.scroll_region_top)
            + 1;
        let count = count.min(region_len);
        if count == 0 {
            return;
        }

        let is_full_screen = self.scroll_region_top == 0
            && self.scroll_region_bottom == self.rows.saturating_sub(1)
            && self.alt_lines.is_none();
        let can_push_scrollback = is_full_screen && self.max_scrollback > 0;

        if can_push_scrollback {
            for i in 0..count {
                if self.scrollback.len() >= self.max_scrollback {
                    if let Some(mut recycled) = self.evict_oldest_scrollback_row() {
                        if recycled.cells.len() != self.cols {
                            recycled.resize(self.cols);
                        }
                        recycled.reset_with_bg(bg);
                        let old = std::mem::replace(&mut self.lines[i], recycled);
                        self.scrollback.push_back(old);
                        if self.viewport_offset > 0 {
                            self.viewport_offset =
                                (self.viewport_offset + 1).min(self.scrollback.len());
                            self.mark_visible_dirty();
                        }
                    }
                } else {
                    let fresh = self.alloc_row_with_bg(self.cols, bg);
                    let old = std::mem::replace(&mut self.lines[i], fresh);
                    self.push_scrollback(old);
                }
            }
        } else {
            self.shift_region_marks_up(self.scroll_region_top, self.scroll_region_bottom, count);
            let top_line = self.scrollback.len() + self.scroll_region_top;
            let bottom_line = self.scrollback.len() + self.scroll_region_bottom;
            self.graphics.shift_region_up(top_line, bottom_line, count);
        }
        self.lines[self.scroll_region_top..=self.scroll_region_bottom].rotate_left(count);
        for row in
            &mut self.lines[self.scroll_region_bottom + 1 - count..=self.scroll_region_bottom]
        {
            row.reset_with_bg(bg);
        }
        for row in &self.lines[self.scroll_region_top..=self.scroll_region_bottom] {
            row.dirty.set(true);
        }
    }

    /// Scrolls lines inside the active scroll region downward with default background.
    pub fn scroll_down(&mut self, count: usize) {
        self.scroll_down_with_bg(count, Color::DefaultBackground);
    }

    /// Scrolls lines inside the active scroll region downward with the specified background color (BCE).
    pub fn scroll_down_with_bg(&mut self, count: usize, bg: Color) {
        self.reset_width_state();
        let region_len = self
            .scroll_region_bottom
            .saturating_sub(self.scroll_region_top)
            + 1;
        let count = count.min(region_len);
        if count == 0 {
            return;
        }

        self.shift_region_marks_down(self.scroll_region_top, self.scroll_region_bottom, count);
        let top_line = self.scrollback.len() + self.scroll_region_top;
        let bottom_line = self.scrollback.len() + self.scroll_region_bottom;
        self.graphics
            .shift_region_down(top_line, bottom_line, count);
        self.lines[self.scroll_region_top..=self.scroll_region_bottom].rotate_right(count);
        for row in &mut self.lines[self.scroll_region_top..self.scroll_region_top + count] {
            row.reset_with_bg(bg);
        }
        for row in &self.lines[self.scroll_region_top..=self.scroll_region_bottom] {
            row.dirty.set(true);
        }
    }

    /// Clears part or all of the active display screen with default background.
    pub fn clear_screen(&mut self, mode: ClearMode) {
        self.clear_screen_with_bg(mode, Color::DefaultBackground);
    }

    /// Clears part or all of the active display screen with the specified background color (BCE).
    pub fn clear_screen_with_bg(&mut self, mode: ClearMode, bg: Color) {
        self.reset_width_state();
        let blank_cell = Cell::blank(bg);
        match mode {
            ClearMode::Below => {
                if self.cursor.row < self.rows {
                    let col = self.cursor.col;
                    self.lines[self.cursor.row].cells[col..].fill(blank_cell);
                    self.lines[self.cursor.row].clear_clusters_range(col, self.cols);
                    for row in &mut self.lines[self.cursor.row + 1..] {
                        row.reset_with_bg(bg);
                    }
                    self.lines[self.cursor.row].dirty.set(true);
                }
            }
            ClearMode::Above => {
                if self.cursor.row < self.rows {
                    for row in &mut self.lines[..self.cursor.row] {
                        row.reset_with_bg(bg);
                    }
                    let col = (self.cursor.col + 1).min(self.cols);
                    self.lines[self.cursor.row].cells[..col].fill(blank_cell);
                    self.lines[self.cursor.row].clear_clusters_range(0, col);
                    self.lines[self.cursor.row].dirty.set(true);
                }
            }
            ClearMode::All => {
                let abs_screen_start = self.total_evicted_rows + self.scrollback.len();
                self.prompt_marks.retain(|&m| m < abs_screen_start);
                for row in &mut self.lines {
                    row.reset_with_bg(bg);
                }
                self.mark_all_dirty();
            }
            ClearMode::Saved => {
                let sb_len = self.scrollback.len();
                while let Some(row) = self.scrollback.pop_front() {
                    self.recycle_row(row);
                }
                self.viewport_offset = 0;
                self.total_evicted_rows = self.total_evicted_rows.saturating_add(sb_len);
                self.prompt_marks.retain(|&m| m >= self.total_evicted_rows);
                self.graphics.rebase_scrollback(sb_len);
                self.mark_all_dirty();
            }
        }
    }

    /// Clears hyperlink references from all visible lines, scrollback, and parked alternate lines.
    pub fn clear_all_hyperlinks(&mut self) {
        for line in &mut self.lines {
            for cell in &mut line.cells {
                cell.hyperlink_id = None;
            }
        }
        for line in &mut self.scrollback {
            for cell in &mut line.cells {
                cell.hyperlink_id = None;
            }
        }
        if let Some(alt) = &mut self.alt_lines {
            for line in alt {
                for cell in &mut line.cells {
                    cell.hyperlink_id = None;
                }
            }
        }
    }

    /// Clears part or all of the current cursor line with default background.
    pub fn clear_line(&mut self, mode: ClearMode) {
        self.clear_line_with_bg(mode, Color::DefaultBackground);
    }

    /// Clears part or all of the current cursor line with the specified background color (BCE).
    pub fn clear_line_with_bg(&mut self, mode: ClearMode, bg: Color) {
        if self.cursor.row >= self.rows {
            return;
        }
        self.reset_width_state();
        let blank_cell = Cell::blank(bg);
        let row = &mut self.lines[self.cursor.row];
        match mode {
            ClearMode::Below => {
                let mut start = self.cursor.col.min(self.cols);
                if start > 0 && row.cells[start].flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                    start -= 1;
                }
                row.cells[start..].fill(blank_cell);
                row.clear_clusters_range(start, self.cols);
            }
            ClearMode::Above => {
                let mut end = (self.cursor.col + 1).min(self.cols);
                if end < self.cols && row.cells[end - 1].flags.contains(CellFlags::WIDE_CHAR) {
                    end += 1;
                }
                row.cells[..end].fill(blank_cell);
                row.clear_clusters_range(0, end);
            }
            ClearMode::All | ClearMode::Saved => {
                row.reset_with_bg(bg);
            }
        }
        row.dirty.set(true);
    }

    /// Writes a character with the given styling attributes at the current cursor position.
    pub fn write_char(&mut self, c: char, fg: Color, bg: Color, flags: CellFlags) {
        self.write_char_styled(c, fg, bg, flags, Color::DefaultForeground, None);
    }

    /// Bulk writes a run of printable ASCII bytes with given styling attributes.
    pub fn write_ascii_run(
        &mut self,
        text: &[u8],
        fg: Color,
        bg: Color,
        flags: CellFlags,
        underline_color: Color,
        hyperlink_id: Option<u32>,
    ) {
        let hl_id = hyperlink_id.and_then(std::num::NonZeroU32::new);
        self.ensure_width_continuity();
        let mut rest = text;

        while !rest.is_empty() {
            if self.cursor.col >= self.cols {
                self.lines[self.cursor.row].wrapped = true;
                self.newline_with_bg(bg);
                self.cursor.col = 0;
                self.cluster_anchor = None;
            }

            let row = self.cursor.row;
            if row >= self.rows {
                self.cluster_anchor = None;
                break;
            }

            let col = self.cursor.col;
            let available = self.cols.saturating_sub(col);
            if available == 0 {
                continue;
            }
            let take = rest.len().min(available);
            let chunk = &rest[..take];
            rest = &rest[take..];

            // Following Contour: cleanly sever wide characters at both boundaries of the run
            let row_line = &mut self.lines[row];
            row_line.sever_wide_at(col);
            if col + take < self.cols {
                row_line.sever_wide_at(col + take);
            }

            let target_cells = &mut row_line.cells[col..col + take];
            for (cell, &byte) in target_cells.iter_mut().zip(chunk.iter()) {
                *cell = Cell {
                    c: byte as char,
                    fg,
                    bg,
                    underline_color,
                    flags,
                    hyperlink_id: hl_id,
                };
            }
            row_line.clear_clusters_range(col, col + take);
            if let Some(coords) = &mut row_line.placeholders {
                for c in col..col + take {
                    coords.remove(&c);
                }
                if coords.is_empty() {
                    row_line.placeholders = None;
                }
            }
            row_line.dirty.set(true);
            self.cursor.col += take;
            self.cluster_anchor = Some((row, col + take - 1));
        }
    }

    /// Selects the width model for East Asian Ambiguous characters (1 or 2 cells).
    pub fn set_ambiguous_width(&mut self, width: u8) {
        self.ambiguous_width = if width == AMBIGUOUS_WIDE {
            AMBIGUOUS_WIDE
        } else {
            crate::width::AMBIGUOUS_NARROW
        };
        self.cluster_anchor = None;
    }

    pub(crate) fn reset_width_state(&mut self) {
        self.cluster_anchor = None;
    }

    fn ensure_width_continuity(&mut self) {
        let Some((row, col)) = self.cluster_anchor else {
            return;
        };
        if row >= self.rows || col >= self.cols {
            self.cluster_anchor = None;
            return;
        }
        let width = if self.lines[row].cells[col]
            .flags
            .contains(CellFlags::WIDE_CHAR)
        {
            2
        } else {
            1
        };
        let end = col.saturating_add(width);
        let adjacent = self.cursor.row == row
            && (self.cursor.col == end || (self.cursor.col == self.cols && end >= self.cols));
        if !adjacent {
            self.cluster_anchor = None;
        }
    }

    // Styling fields are intentionally forwarded without an intermediate allocation.
    #[allow(clippy::too_many_arguments)]
    fn write_base_cluster(
        &mut self,
        c: char,
        width: usize,
        fg: Color,
        bg: Color,
        flags: CellFlags,
        underline_color: Color,
        hl_id: Option<std::num::NonZeroU32>,
    ) {
        let width = width.clamp(1, 2);
        if self.cursor.row >= self.rows {
            self.cluster_anchor = None;
            return;
        }
        if self.cursor.col >= self.cols || self.cursor.col + width > self.cols {
            self.lines[self.cursor.row].wrapped = true;
            self.newline_with_bg(bg);
            self.cursor.col = 0;
        }
        if self.cursor.row >= self.rows || self.cursor.col >= self.cols {
            self.cluster_anchor = None;
            return;
        }

        let row = self.cursor.row;
        let col = self.cursor.col;

        // Sever any wide character relationship crossing the cells we are writing over
        self.lines[row].sever_wide_at(col);
        if width == 2 && col + 1 < self.cols {
            self.lines[row].sever_wide_at(col + 1);
        }

        let mut cell_flags = flags;
        if width == 2 {
            cell_flags |= CellFlags::WIDE_CHAR;
        }
        self.lines[row].cells[col] = Cell {
            c,
            fg,
            bg,
            underline_color,
            flags: cell_flags,
            hyperlink_id: hl_id,
        };
        self.lines[row].clear_cluster(col);
        self.lines[row].dirty.set(true);

        if c == KITTY_PLACEHOLDER {
            let (img_row, img_col) = if col > 0
                && let Some(coords) = &self.lines[row].placeholders
                && let Some(&(left_row, left_col, _)) = coords.get(&(col - 1))
                && self.lines[row].cells[col - 1].fg == fg
            {
                (left_row, left_col + 1)
            } else {
                (0, 0)
            };
            self.lines[row]
                .placeholders
                .get_or_insert_with(|| Box::new(HashMap::new()))
                .insert(col, (img_row, img_col, 0));
        }

        if width == 2 && col + 1 < self.cols {
            self.lines[row].cells[col + 1] = Cell {
                c: ' ',
                fg,
                bg,
                underline_color,
                flags: flags | CellFlags::WIDE_CHAR_SPACER,
                hyperlink_id: hl_id,
            };
            self.lines[row].clear_cluster(col + 1);
        }

        self.cluster_anchor = Some((row, col));
        self.cursor.col += width;
        if self.cursor.col >= self.cols {
            self.cursor.col = self.cols;
        }
    }

    fn attach_cluster_char(&mut self, c: char) {
        let Some((row, col)) = self.cluster_anchor else {
            return;
        };
        if row >= self.rows || col >= self.cols {
            self.cluster_anchor = None;
            return;
        }
        self.lines[row].append_cluster_char(col, c);
        let cluster = self.lines[row]
            .cluster_text(col)
            .map(str::to_owned)
            .unwrap_or_else(|| self.lines[row].cells[col].c.to_string());
        let target_width = grapheme_display_width(&cluster, self.ambiguous_width);
        let old_width = if self.lines[row].cells[col]
            .flags
            .contains(CellFlags::WIDE_CHAR)
        {
            2
        } else {
            1
        };

        if target_width > old_width && old_width == 1 {
            if col + 2 <= self.cols {
                self.lines[row].sever_wide_at(col + 1);
                let base = self.lines[row].cells[col];
                self.lines[row].cells[col].flags |= CellFlags::WIDE_CHAR;
                self.lines[row].cells[col + 1] = Cell {
                    c: ' ',
                    fg: base.fg,
                    bg: base.bg,
                    underline_color: base.underline_color,
                    flags: (base.flags & !CellFlags::WIDE_CHAR) | CellFlags::WIDE_CHAR_SPACER,
                    hyperlink_id: base.hyperlink_id,
                };
                self.lines[row].clear_cluster(col + 1);
                if self.cursor.row == row && self.cursor.col == col + 1 {
                    self.cursor.col += 1;
                }
                self.lines[row].dirty.set(true);
            }
        } else if target_width < old_width && old_width == 2 {
            self.lines[row].cells[col]
                .flags
                .remove(CellFlags::WIDE_CHAR);
            if col + 1 < self.cols {
                self.lines[row].sever_wide_at(col + 1);
                self.lines[row].cells[col + 1] = Cell::blank(self.lines[row].cells[col].bg);
            }
            if self.cursor.row == row && self.cursor.col >= col + 2 {
                self.cursor.col = self.cursor.col.saturating_sub(1);
            }
            self.lines[row].dirty.set(true);
        }
    }

    fn try_placeholder_diacritic(&mut self, c: char) -> bool {
        let Some(idx) = diacritic_to_index(c) else {
            return false;
        };
        if self.cursor.row >= self.rows || self.cursor.col == 0 {
            return false;
        }
        let target_row = self.cursor.row;
        let target_col = self.cursor.col.min(self.cols).saturating_sub(1);
        if self.lines[target_row].cells[target_col].c != KITTY_PLACEHOLDER {
            return false;
        }
        if let Some(coords) = &mut self.lines[target_row].placeholders
            && let Some(coord) = coords.get_mut(&target_col)
        {
            match coord.2 {
                0 => coord.0 = idx,
                1 => coord.1 = idx,
                _ => {}
            }
            coord.2 = coord.2.saturating_add(1);
        }
        true
    }

    /// Writes a character with extended styling attributes including underline color and hyperlink id.
    pub fn write_char_styled(
        &mut self,
        c: char,
        fg: Color,
        bg: Color,
        flags: CellFlags,
        underline_color: Color,
        hyperlink_id: Option<u32>,
    ) {
        let hl_id = hyperlink_id.and_then(std::num::NonZeroU32::new);
        self.ensure_width_continuity();
        if self.try_placeholder_diacritic(c) {
            return;
        }

        // Check if `c` continues the grapheme cluster at the anchor cell
        if let Some((row, col)) = self.cluster_anchor
            && row < self.rows
            && col < self.cols
        {
            let cell_c = self.lines[row].cells[col].c;
            let continues = if let Some(cluster) = self.lines[row].cluster_text(col) {
                continues_grapheme(cluster, c, &mut self.scratch_cluster)
            } else {
                let mut s = String::with_capacity(8);
                s.push(cell_c);
                continues_grapheme(&s, c, &mut self.scratch_cluster)
            };
            if continues {
                self.attach_cluster_char(c);
                return;
            }
        }

        let w = wcwidth(c, self.ambiguous_width);
        if w < 0 {
            self.cluster_anchor = None;
            return;
        }
        if w == 0 {
            // Unattached zero-width / combining character
            if self.cluster_anchor.is_some() {
                self.attach_cluster_char(c);
            }
            return;
        }

        self.write_base_cluster(c, w as usize, fg, bg, flags, underline_color, hl_id);
    }

    /// Performs a newline (LF): moves cursor down, scrolling if necessary.
    pub fn newline(&mut self) {
        self.newline_with_bg(Color::DefaultBackground);
    }

    /// Performs a newline (LF) using the specified background color for scrolled lines (BCE).
    pub fn newline_with_bg(&mut self, bg: Color) {
        if self.cursor.row == self.scroll_region_bottom {
            self.scroll_up_with_bg(1, bg);
        } else if self.cursor.row < self.rows.saturating_sub(1) {
            self.cursor.row += 1;
        }
    }

    /// Performs a carriage return (CR): moves cursor to column 0.
    pub fn carriage_return(&mut self) {
        self.cursor.col = 0;
    }

    /// Performs a backspace (BS): moves cursor one position left.
    pub fn backspace(&mut self) {
        self.cursor.col = self.cursor.col.saturating_sub(1);
    }

    /// Moves cursor to next tab stop (multiples of 8).
    pub fn tab(&mut self) {
        let next_tab = (self.cursor.col + 8) & !7;
        self.cursor.col = next_tab.min(self.cols.saturating_sub(1));
    }

    /// Inserts blank characters at cursor, shifting remaining characters right with default background.
    pub fn insert_blank_chars(&mut self, count: usize) {
        self.insert_blank_chars_with_bg(count, Color::DefaultBackground);
    }

    /// Inserts blank characters at cursor with the specified background color (BCE).
    pub fn insert_blank_chars_with_bg(&mut self, count: usize, bg: Color) {
        if self.cursor.row >= self.rows || self.cursor.col >= self.cols {
            return;
        }
        self.reset_width_state();
        let row = &mut self.lines[self.cursor.row];
        let col = self.cursor.col;
        let count = count.min(self.cols - col);

        if row.cells[col].flags.contains(CellFlags::WIDE_CHAR_SPACER) {
            row.sever_wide_at(col);
        }
        if self.cols > count
            && row.cells[self.cols - count - 1]
                .flags
                .contains(CellFlags::WIDE_CHAR)
        {
            row.sever_wide_at(self.cols - count - 1);
        }

        for i in (col + count..self.cols).rev() {
            row.cells[i] = row.cells[i - count];
        }
        let blank_cell = Cell::blank(bg);
        for cell in &mut row.cells[col..col + count] {
            *cell = blank_cell;
        }
        row.shift_clusters_right(col, count);
        row.clear_clusters_range(col, col + count);
        row.dirty.set(true);
    }

    /// Deletes characters at cursor, shifting remaining characters left with default background.
    pub fn delete_chars(&mut self, count: usize) {
        self.delete_chars_with_bg(count, Color::DefaultBackground);
    }

    /// Deletes characters at cursor, shifting remaining characters left with the specified background color (BCE).
    pub fn delete_chars_with_bg(&mut self, count: usize, bg: Color) {
        if self.cursor.row >= self.rows || self.cursor.col >= self.cols {
            return;
        }
        self.reset_width_state();
        let row = &mut self.lines[self.cursor.row];
        let col = self.cursor.col;
        let count = count.min(self.cols - col);

        if row.cells[col].flags.contains(CellFlags::WIDE_CHAR_SPACER) {
            row.sever_wide_at(col);
        }
        if col + count < self.cols
            && row.cells[col + count]
                .flags
                .contains(CellFlags::WIDE_CHAR_SPACER)
        {
            row.sever_wide_at(col + count);
        }

        for i in col..self.cols - count {
            row.cells[i] = row.cells[i + count];
        }
        let blank_cell = Cell::blank(bg);
        for cell in &mut row.cells[self.cols - count..] {
            *cell = blank_cell;
        }
        row.shift_clusters_left(col, count);
        row.clear_clusters_range(self.cols - count, self.cols);
        row.dirty.set(true);
    }

    /// Erases `count` characters starting at cursor position with default background without moving the cursor.
    pub fn erase_chars(&mut self, count: usize) {
        self.erase_chars_with_bg(count, Color::DefaultBackground);
    }

    /// Erases `count` characters starting at cursor position with specified background color (BCE).
    pub fn erase_chars_with_bg(&mut self, count: usize, bg: Color) {
        if self.cursor.row >= self.rows || self.cursor.col >= self.cols {
            return;
        }
        self.reset_width_state();
        let row = &mut self.lines[self.cursor.row];
        let col = self.cursor.col;
        let count = count.min(self.cols - col);
        if count == 0 {
            return;
        }

        let mut start = col;
        let mut end = col + count;

        if start > 0 && row.cells[start].flags.contains(CellFlags::WIDE_CHAR_SPACER) {
            start -= 1;
        }
        if end < self.cols && row.cells[end - 1].flags.contains(CellFlags::WIDE_CHAR) {
            end += 1;
        }

        let blank_cell = Cell::blank(bg);
        for i in start..end {
            row.cells[i] = blank_cell;
            if let Some(coords) = &mut row.placeholders {
                coords.remove(&i);
            }
        }
        row.clear_clusters_range(start, end);
        if let Some(coords) = &mut row.placeholders
            && coords.is_empty()
        {
            row.placeholders = None;
        }
        row.dirty.set(true);
    }

    /// Inserts lines at cursor row, shifting lines down with default background.
    pub fn insert_lines(&mut self, count: usize) {
        self.insert_lines_with_bg(count, Color::DefaultBackground);
    }

    /// Inserts lines at cursor row, shifting lines down with specified background color (BCE).
    pub fn insert_lines_with_bg(&mut self, count: usize, bg: Color) {
        self.reset_width_state();
        if self.cursor.row < self.scroll_region_top || self.cursor.row > self.scroll_region_bottom {
            return;
        }
        let count = count.min(self.scroll_region_bottom - self.cursor.row + 1);
        self.shift_region_marks_down(self.cursor.row, self.scroll_region_bottom, count);
        for _ in 0..count {
            let removed = self.lines.remove(self.scroll_region_bottom);
            self.recycle_row(removed);
            let new_row = self.alloc_row_with_bg(self.cols, bg);
            self.lines.insert(self.cursor.row, new_row);
        }
        self.mark_all_dirty();
    }

    /// Deletes lines at cursor row, shifting lines up with default background.
    pub fn delete_lines(&mut self, count: usize) {
        self.delete_lines_with_bg(count, Color::DefaultBackground);
    }

    /// Deletes lines at cursor row, shifting lines up with specified background color (BCE).
    pub fn delete_lines_with_bg(&mut self, count: usize, bg: Color) {
        self.reset_width_state();
        if self.cursor.row < self.scroll_region_top || self.cursor.row > self.scroll_region_bottom {
            return;
        }
        let count = count.min(self.scroll_region_bottom - self.cursor.row + 1);
        self.shift_region_marks_up(self.cursor.row, self.scroll_region_bottom, count);
        for _ in 0..count {
            let removed = self.lines.remove(self.cursor.row);
            self.recycle_row(removed);
            let new_row = self.alloc_row_with_bg(self.cols, bg);
            self.lines.insert(self.scroll_region_bottom, new_row);
        }
        self.mark_all_dirty();
    }

    pub fn save_cursor(&mut self) {
        self.saved_cursor = self.cursor;
    }

    pub fn restore_cursor(&mut self) {
        self.cursor = self.saved_cursor;
        self.cursor.row = self.cursor.row.min(self.rows.saturating_sub(1));
        self.cursor.col = self.cursor.col.min(self.cols.saturating_sub(1));
    }
}

pub mod reflow;
pub use reflow::{LogicalLine, ReflowCell, reflow_primary_screen, shrink_rows};

impl Grid {
    /// Resizes the grid dimensions.
    ///
    /// Shrinking keeps the cursor's line on screen and discards rows below it first, so content
    /// above the cursor does not scroll away while the area beneath it is still empty.
    pub fn resize(&mut self, new_cols: usize, new_rows: usize) {
        self.reset_width_state();
        let new_cols = new_cols.max(1);
        let new_rows = new_rows.max(1);

        if self.alt_lines.is_none() && new_cols != self.cols {
            reflow::reflow_primary_screen(self, new_cols, new_rows);
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
                    let hidden_removed = reflow::shrink_rows(alt, to_remove, hidden_row, new_rows);
                    let hidden_from_top = hidden_removed.len();
                    if let Some(cursor) = &mut self.alt_cursor {
                        cursor.row = cursor.row.saturating_sub(hidden_from_top);
                    }
                    if let Some(saved) = &mut self.alt_saved_cursor {
                        saved.row = saved.row.saturating_sub(hidden_from_top);
                    }
                    self.graphics.shift_placements(hidden_from_top);
                }
            } else {
                // Primary screen with scrollback: trim below cursor first so active prompt/cursor survives.
                let active_retains = self.max_scrollback > 0;
                let removed_top =
                    reflow::shrink_rows(&mut self.lines, to_remove, self.cursor.row, new_rows);
                let from_top = removed_top.len();
                for row in removed_top {
                    self.push_scrollback(row);
                }
                self.cursor.row = self.cursor.row.saturating_sub(from_top);
                self.saved_cursor.row = self.saved_cursor.row.saturating_sub(from_top);
                if !active_retains {
                    self.graphics.shift_placements(from_top);
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
            self.graphics
                .retain_mut_placements(|p| p.line < bottom_line);
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
}

#[cfg(test)]
mod tests {


use crate::color::Color;
use crate::grid::{
    Cell, CellFlags, ClearMode, Cursor, CursorShape, Grid, MAX_ROW_POOL_CAPACITY, PlaceholderMap,
    Row,
};
use crate::kitty::model::{DeleteTarget, ImageData, ImagePlacement};
use crate::width::KITTY_PLACEHOLDER;

#[test]
fn test_grid_initialization() {
    let grid = Grid::new(80, 24, 100);
    assert_eq!(grid.cols, 80);
    assert_eq!(grid.rows, 24);
    assert_eq!(grid.lines.len(), 24);
    assert_eq!(grid.lines[0].cells.len(), 80);
    assert_eq!(grid.cursor.row, 0);
    assert_eq!(grid.cursor.col, 0);
}

#[test]
fn test_write_ascii_and_wide_char() {
    let mut grid = Grid::new(10, 2, 10);
    grid.write_char(
        'A',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    assert_eq!(grid.lines[0].cells[0].c, 'A');
    assert_eq!(grid.cursor.col, 1);

    grid.write_char(
        '中',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    assert_eq!(grid.lines[0].cells[1].c, '中');
    assert!(grid.lines[0].cells[1].flags.contains(CellFlags::WIDE_CHAR));
    assert!(
        grid.lines[0].cells[2]
            .flags
            .contains(CellFlags::WIDE_CHAR_SPACER)
    );
    assert_eq!(grid.cursor.col, 3);
}

#[test]
fn test_scrolling_and_scrollback() {
    let mut grid = Grid::new(10, 2, 5);
    grid.write_char(
        '1',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    grid.newline();
    grid.carriage_return();
    grid.write_char(
        '2',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    grid.newline(); // This triggers scroll up

    assert_eq!(grid.scrollback.len(), 1);
    assert_eq!(grid.scrollback[0].cells[0].c, '1');
    assert_eq!(grid.lines[0].cells[0].c, '2');
}

#[test]
fn test_alt_screen() {
    let mut grid = Grid::new(10, 2, 10);
    grid.write_char(
        'P',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    assert_eq!(grid.lines[0].cells[0].c, 'P');

    grid.enter_alt_screen();
    assert!(grid.is_alt_screen());
    assert_eq!(grid.lines[0].cells[0].c, ' ');

    grid.write_char(
        'A',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    assert_eq!(grid.lines[0].cells[0].c, 'A');

    grid.exit_alt_screen();
    assert!(!grid.is_alt_screen());
    assert_eq!(grid.lines[0].cells[0].c, 'P');
}

#[test]
fn test_viewport_scrolling_and_visible_lines() {
    let mut grid = Grid::new(10, 2, 100);
    // Write line 1
    grid.write_char(
        '1',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    grid.carriage_return();
    grid.newline();
    // Write line 2
    grid.write_char(
        '2',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    grid.carriage_return();
    grid.newline();
    // Write line 3
    grid.write_char(
        '3',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    grid.carriage_return();
    grid.newline();
    // Write line 4
    grid.write_char(
        '4',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );

    assert_eq!(grid.scrollback.len(), 2);
    assert_eq!(grid.viewport_offset(), 0);
    assert_eq!(grid.visible_line(0).cells[0].c, '3');
    assert_eq!(grid.visible_line(1).cells[0].c, '4');

    // Scroll up by 1
    grid.scroll_viewport_up(1);
    assert_eq!(grid.viewport_offset(), 1);
    assert_eq!(grid.visible_line(0).cells[0].c, '2');
    assert_eq!(grid.visible_line(1).cells[0].c, '3');

    // Scroll to top
    grid.scroll_viewport_top();
    assert_eq!(grid.viewport_offset(), 2);
    assert_eq!(grid.visible_line(0).cells[0].c, '1');
    assert_eq!(grid.visible_line(1).cells[0].c, '2');

    // Scroll down and to bottom
    grid.scroll_viewport_down(1);
    assert_eq!(grid.viewport_offset(), 1);
    grid.scroll_viewport_bottom();
    assert_eq!(grid.viewport_offset(), 0);
    assert_eq!(grid.visible_line(1).cells[0].c, '4');
}

#[test]
fn test_history_anchoring_on_new_output() {
    let mut grid = Grid::new(10, 2, 5);
    for c in ['1', '2', '3', '4'] {
        grid.write_char(
            c,
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
        grid.carriage_return();
        grid.newline();
    }

    // Viewport viewing '3' and '4'
    grid.scroll_viewport_up(1);
    assert_eq!(grid.visible_line(0).cells[0].c, '3');

    // New output arrives while scrolled back
    grid.write_char(
        '5',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    grid.carriage_return();
    grid.newline();

    // History anchor must keep visible lines identical
    assert_eq!(grid.visible_line(0).cells[0].c, '3');
}

#[test]
fn test_clear_saved_scrollback_resets_viewport_offset() {
    let mut grid = Grid::new(10, 2, 10);
    for c in ['1', '2', '3'] {
        grid.write_char(
            c,
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
        grid.carriage_return();
        grid.newline();
    }

    grid.scroll_viewport_up(1);
    assert_eq!(grid.viewport_offset(), 1);

    // CSI 3 J clears saved lines
    grid.clear_screen(ClearMode::Saved);
    assert_eq!(grid.viewport_offset(), 0);
    assert!(grid.scrollback.is_empty());
    // Must not panic on subsequent visible_line access
    assert_eq!(grid.visible_line(0).cells.len(), 10);
}

#[test]
fn test_clear_screen_marks_affected_rows_dirty() {
    let mut grid = Grid::new(10, 5, 10);
    // Clear all initial dirty flags
    for row in &grid.lines {
        row.dirty.set(false);
    }

    // 1. Clear Below at row 2
    grid.cursor.row = 2;
    grid.cursor.col = 3;
    grid.clear_screen(ClearMode::Below);
    assert!(!grid.lines[0].dirty.get());
    assert!(!grid.lines[1].dirty.get());
    assert!(grid.lines[2].dirty.get());
    assert!(grid.lines[3].dirty.get());
    assert!(grid.lines[4].dirty.get());

    // Reset dirty flags
    for row in &grid.lines {
        row.dirty.set(false);
    }

    // 2. Clear Above at row 2
    grid.clear_screen(ClearMode::Above);
    assert!(grid.lines[0].dirty.get());
    assert!(grid.lines[1].dirty.get());
    assert!(grid.lines[2].dirty.get());
    assert!(!grid.lines[3].dirty.get());
    assert!(!grid.lines[4].dirty.get());

    // Reset dirty flags
    for row in &grid.lines {
        row.dirty.set(false);
    }

    // 3. Clear Saved
    grid.clear_screen(ClearMode::Saved);
    for row in &grid.lines {
        assert!(row.dirty.get());
    }
}

#[test]
fn virtual_placement_images_are_preserved_across_evictions() {
    let mut grid = Grid::new(80, 24, 100);
    grid.graphics.insert_virtual_placement(100, 10, 10);
    grid.write_char(
        KITTY_PLACEHOLDER,
        Color::Rgb(0, 0, 100),
        Color::DefaultBackground,
        CellFlags::empty(),
    );

    for id in 1..=260 {
        grid.add_image(ImageData {
            id,
            width: 1,
            height: 1,
            rgba: Some(vec![0, 0, 0, 0]),
        });
    }

    assert!(
        grid.contains_image(100),
        "virtual image 100 must be preserved across eviction"
    );
}

#[test]
fn scrollback_virtual_placement_images_are_preserved_across_evictions() {
    let mut grid = Grid::new(80, 24, 100);
    grid.graphics.insert_virtual_placement(100, 10, 10);
    grid.write_char(
        KITTY_PLACEHOLDER,
        Color::Rgb(0, 0, 100),
        Color::DefaultBackground,
        CellFlags::empty(),
    );

    // Scroll lines up so the placeholder moves into scrollback history
    grid.scroll_up(5);
    assert_eq!(grid.scrollback.len(), 5);

    for id in 1..=260 {
        grid.add_image(ImageData {
            id,
            width: 1,
            height: 1,
            rgba: Some(vec![0, 0, 0, 0]),
        });
    }

    assert!(
        grid.contains_image(100),
        "virtual image 100 in scrollback must be preserved across eviction"
    );
}

/// Writes `count` rows labelled `R0`, `R1`, … and leaves the cursor on the last one.
fn fill_rows(grid: &mut Grid, count: usize) {
    for row in 0..count {
        for c in format!("R{row}").chars() {
            grid.write_char(
                c,
                Color::DefaultForeground,
                Color::DefaultBackground,
                CellFlags::empty(),
            );
        }
        grid.carriage_return();
        if row + 1 < count {
            grid.newline();
        }
    }
}

#[test]
fn shrink_below_cursor_discards_bottom_rows_and_keeps_content() {
    let mut grid = Grid::new(10, 10, 100);
    fill_rows(&mut grid, 3);
    grid.cursor.row = 2;
    grid.cursor.col = 0;

    grid.resize(10, 8);

    assert_eq!(grid.rows, 8);
    assert_eq!(grid.lines.len(), 8);
    assert_eq!(
        grid.cursor.row, 2,
        "cursor must not move when nothing above it is cut"
    );
    assert_eq!(grid.visible_line(0).cells[0].c, 'R');
    assert_eq!(grid.visible_line(0).cells[1].c, '0');
    assert_eq!(grid.visible_line(2).cells[1].c, '2');
    assert!(
        grid.scrollback.is_empty(),
        "top content must not scroll away"
    );
}

#[test]
fn shrink_above_cursor_scrolls_top_rows_and_keeps_cursor_line() {
    let mut grid = Grid::new(10, 4, 100);
    fill_rows(&mut grid, 4);
    grid.cursor.row = 3;
    grid.cursor.col = 1;

    grid.resize(10, 2);

    assert_eq!(grid.rows, 2);
    assert_eq!(grid.cursor.row, 1, "cursor must follow its content up");
    assert!(grid.cursor.row < grid.rows);
    // The cursor's own row (`R3`) is the last surviving line.
    assert_eq!(grid.visible_line(1).cells[1].c, '3');
    assert_eq!(grid.scrollback.len(), 2, "two top rows move into history");
    assert_eq!(grid.scrollback[0].cells[1].c, '0');
}

#[test]
fn shrink_without_scrollback_discards_top_rows() {
    let mut grid = Grid::new(10, 4, 0);
    fill_rows(&mut grid, 4);
    grid.cursor.row = 3;

    grid.resize(10, 2);

    assert_eq!(grid.rows, 2);
    assert_eq!(grid.cursor.row, 1);
    assert!(grid.scrollback.is_empty());
    assert_eq!(grid.visible_line(1).cells[1].c, '3');
}

#[test]
fn shrink_on_alternate_screen_keeps_cursor_visible() {
    let mut grid = Grid::new(10, 4, 100);
    fill_rows(&mut grid, 2);
    grid.enter_alt_screen();
    fill_rows(&mut grid, 4);
    grid.cursor.row = 3;

    grid.resize(10, 2);

    assert!(grid.is_alt_screen());
    assert_eq!(grid.rows, 2);
    assert_eq!(grid.cursor.row, 1);
    assert!(
        grid.scrollback.is_empty(),
        "the alternate screen has no history"
    );
    // The primary screen is restored with its own cursor intact.
    grid.exit_alt_screen();
    assert_eq!(grid.rows, 2);
    assert!(grid.cursor.row < grid.rows);
}

#[test]
fn shrink_shifts_saved_cursor() {
    let mut grid = Grid::new(10, 4, 100);
    fill_rows(&mut grid, 4);
    grid.cursor.row = 3;
    grid.cursor.col = 2;
    grid.save_cursor();

    grid.resize(10, 2);

    assert_eq!(grid.cursor.row, 1);
    assert_eq!(
        grid.saved_cursor.row, 1,
        "the saved cursor follows the screen"
    );
}

#[test]
fn shrink_evicts_placements_below_the_new_bottom() {
    let mut grid = Grid::new(10, 4, 100);
    fill_rows(&mut grid, 4);
    // Keep the cursor near the top so the bottom row is what gets discarded.
    grid.cursor.row = 1;

    let scrollback_len = grid.scrollback.len();
    grid.add_placement(ImagePlacement {
        image_id: 1,
        placement_id: 0,
        line: scrollback_len + 3,
        col: 0,
        cols: 1,
        rows: 1,
        offset_x: 0,
        offset_y: 0,
        z_index: 0,
        ..Default::default()
    });

    grid.resize(10, 2);

    assert_eq!(grid.cursor.row, 1);
    assert!(
        grid.graphics.placements().is_empty(),
        "a placement anchored below the new bottom row is evicted"
    );
}

#[test]
fn shrink_on_alternate_screen_evicts_truncated_placements() {
    let mut grid = Grid::new(10, 4, 100);
    grid.enter_alt_screen();
    fill_rows(&mut grid, 4);

    // Placement on row 1 (survives) and row 3 (truncated)
    grid.add_placement(ImagePlacement {
        image_id: 1,
        placement_id: 0,
        line: grid.scrollback.len() + 1,
        col: 0,
        cols: 1,
        rows: 1,
        offset_x: 0,
        offset_y: 0,
        z_index: 0,
        ..Default::default()
    });
    grid.add_placement(ImagePlacement {
        image_id: 2,
        placement_id: 0,
        line: grid.scrollback.len() + 3,
        col: 0,
        cols: 1,
        rows: 1,
        offset_x: 0,
        offset_y: 0,
        z_index: 0,
        ..Default::default()
    });

    grid.resize(10, 2);

    assert_eq!(
        grid.graphics.placements().len(),
        1,
        "only the surviving row placement is retained"
    );
    assert_eq!(
        grid.graphics.placements()[0].line,
        grid.scrollback.len() + 1,
        "row 1 maintains its absolute row placement"
    );
}

#[test]
fn saved_cursor_is_scoped_to_its_screen_across_resize() {
    let mut grid = Grid::new(10, 6, 100);
    grid.cursor.row = 1;
    grid.cursor.col = 3;
    grid.save_cursor();

    // The alternate screen trims more top rows than the hidden primary would.
    grid.enter_alt_screen();
    grid.cursor.row = 5;
    grid.resize(10, 2);
    grid.exit_alt_screen();

    assert_eq!(
        grid.saved_cursor.row, 1,
        "the primary keeps its own saved cursor"
    );
    assert_eq!(grid.saved_cursor.col, 3);
}

#[test]
fn primary_placements_are_parked_while_the_alternate_screen_is_active() {
    let mut grid = Grid::new(10, 4, 100);
    grid.add_placement(ImagePlacement {
        image_id: 7,
        placement_id: 0,
        line: 2,
        col: 0,
        cols: 1,
        rows: 1,
        offset_x: 0,
        offset_y: 0,
        z_index: 0,
        ..Default::default()
    });
    assert_eq!(grid.graphics.placements().len(), 1);

    grid.enter_alt_screen();
    assert!(
        grid.graphics.placements().is_empty(),
        "primary placements must not bleed onto the alternate screen"
    );

    grid.exit_alt_screen();
    assert_eq!(grid.graphics.placements().len(), 1);
    assert_eq!(grid.graphics.placements()[0].line, 2);
}

#[test]
fn hidden_primary_images_survive_cache_eviction() {
    let mut grid = Grid::new(10, 4, 100);
    let image = ImageData {
        id: 42,
        width: 1,
        height: 1,
        rgba: Some(vec![0, 0, 0, 0]),
    };
    grid.add_image(image);
    grid.add_placement(ImagePlacement {
        image_id: 42,
        placement_id: 0,
        line: 1,
        col: 0,
        cols: 1,
        rows: 1,
        offset_x: 0,
        offset_y: 0,
        z_index: 0,
        ..Default::default()
    });

    // The primary's placement is parked while the alternate screen churns the cache.
    grid.enter_alt_screen();
    for id in 1000..1300 {
        grid.add_image(ImageData {
            id,
            width: 1,
            height: 1,
            rgba: Some(vec![0, 0, 0, 0]),
        });
    }
    grid.exit_alt_screen();

    assert!(
        grid.contains_image(42),
        "an image referenced by the hidden primary must not be evicted"
    );
    assert_eq!(grid.graphics.placements().len(), 1);
}

#[test]
fn clear_saved_history_rebases_hidden_primary_placements() {
    let mut grid = Grid::new(10, 2, 10);
    fill_rows(&mut grid, 4);
    assert_eq!(grid.scrollback.len(), 2);

    grid.add_placement(ImagePlacement {
        image_id: 5,
        placement_id: 0,
        line: grid.scrollback.len() + 1,
        col: 0,
        cols: 1,
        rows: 1,
        offset_x: 0,
        offset_y: 0,
        z_index: 0,
        ..Default::default()
    });

    grid.enter_alt_screen();
    grid.clear_screen(ClearMode::Saved);
    grid.exit_alt_screen();

    assert_eq!(grid.scrollback.len(), 0);
    assert_eq!(
        grid.graphics.placements()[0].line,
        1,
        "the parked placement follows the cleared history"
    );
}

#[test]
fn hidden_saved_cursor_is_clamped_on_shrink() {
    let mut grid = Grid::new(10, 6, 100);
    grid.cursor.row = 1;
    grid.saved_cursor.row = 5;
    grid.enter_alt_screen();

    // Shrinking drops the hidden primary's bottom rows, so row 5 no longer exists.
    grid.resize(10, 2);
    // Growing back must not resurrect the discarded coordinate.
    grid.resize(10, 6);
    grid.exit_alt_screen();

    assert_eq!(
        grid.saved_cursor.row, 1,
        "a restored saved cursor must be clamped to the surviving screen"
    );
}

#[test]
fn grow_appends_rows_and_keeps_cursor_and_scrollback() {
    let mut grid = Grid::new(10, 2, 100);
    fill_rows(&mut grid, 2);
    grid.cursor.row = 1;
    grid.cursor.col = 1;

    grid.resize(12, 5);

    assert_eq!(grid.rows, 5);
    assert_eq!(grid.cols, 12);
    assert_eq!(grid.lines.len(), 5);
    assert_eq!(grid.cursor.row, 1);
    assert_eq!(grid.visible_line(1).cells[1].c, '1');
    assert_eq!(grid.lines[1].cells.len(), 12);
    assert!(grid.scrollback.is_empty());
}

#[test]
fn test_shrink_and_grow_restores_scrollback_text() {
    let mut grid = Grid::new(10, 4, 100);
    fill_rows(&mut grid, 4);
    grid.cursor.row = 3;
    grid.cursor.col = 1;

    // Shrink from 4 to 2 rows: top 2 rows move to scrollback
    grid.resize(10, 2);
    assert_eq!(grid.rows, 2);
    assert_eq!(grid.cursor.row, 1);
    assert_eq!(grid.scrollback.len(), 2);

    // Grow back from 2 to 4 rows: top 2 rows must be pulled back from scrollback!
    grid.resize(10, 4);
    assert_eq!(grid.rows, 4);
    assert_eq!(grid.cursor.row, 3);
    assert_eq!(grid.scrollback.len(), 0);
    assert_eq!(grid.visible_line(0).cells[1].c, '0');
    assert_eq!(grid.visible_line(1).cells[1].c, '1');
    assert_eq!(grid.visible_line(2).cells[1].c, '2');
    assert_eq!(grid.visible_line(3).cells[1].c, '3');
}

#[test]
fn test_horizontal_shrink_and_grow_pads_with_default_cells() {
    let mut grid = Grid::new(20, 2, 100);
    let text = "Hello World 12345";
    for c in text.chars() {
        grid.write_char(
            c,
            crate::color::Color::DefaultForeground,
            crate::color::Color::DefaultBackground,
            CellFlags::empty(),
        );
    }
    assert_eq!(grid.visible_line(0).cells[16].c, '5');

    // Shrink horizontally to 5 columns: line reflows and wraps across rows
    grid.resize(5, 2);
    assert_eq!(grid.cols, 5);
    assert_eq!(grid.visible_line(0).cells.len(), 5);

    // Grow horizontally back to 20 columns: wrapped rows unwrap and restore the full string
    grid.resize(20, 2);
    assert_eq!(grid.cols, 20);
    assert_eq!(grid.visible_line(0).cells.len(), 20);
    let full: String = grid.visible_line(0).cells[..17]
        .iter()
        .map(|c| c.c)
        .collect();
    assert_eq!(full, "Hello World 12345");
    for cell in &grid.visible_line(0).cells[17..] {
        assert_eq!(cell.c, ' ');
    }
}

#[test]
fn test_horizontal_shrink_clears_split_wide_character() {
    let mut grid = Grid::new(10, 1, 100);
    grid.cursor.col = 4;
    // Write 2-column wide character '你' at col 4 and 5
    grid.write_char(
        '你',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    assert!(
        grid.visible_line(0).cells[4]
            .flags
            .contains(CellFlags::WIDE_CHAR)
    );
    assert!(
        grid.visible_line(0).cells[5]
            .flags
            .contains(CellFlags::WIDE_CHAR_SPACER)
    );

    // Shrink to 5 columns: wraps rather than splitting the 2-column wide character
    grid.resize(5, 1);
    assert_eq!(grid.cols, 5);

    // Grow back to 10 columns: unwraps and preserves the wide character intact
    grid.resize(10, 1);
    assert_eq!(grid.visible_line(0).cells[4].c, '你');
    assert!(
        grid.visible_line(0).cells[4]
            .flags
            .contains(CellFlags::WIDE_CHAR)
    );
    assert!(
        grid.visible_line(0).cells[5]
            .flags
            .contains(CellFlags::WIDE_CHAR_SPACER)
    );
}

#[test]
fn test_alt_screen_resize_preserves_absolute_row_coordinates() {
    let mut grid = Grid::new(20, 5, 100);
    grid.enter_alt_screen();

    for r in 0..5 {
        grid.cursor.row = r;
        grid.cursor.col = 0;
        let c = char::from_digit(r as u32, 10).unwrap();
        grid.write_char(
            c,
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
    }

    // Shrink to 3 rows on alternate screen: row 0, 1, 2 stay intact (never drained from top)
    grid.resize(20, 3);
    assert_eq!(grid.rows, 3);
    assert_eq!(grid.lines.len(), 3);
    assert_eq!(grid.visible_line(0).cells[0].c, '0');
    assert_eq!(grid.visible_line(1).cells[0].c, '1');
    assert_eq!(grid.visible_line(2).cells[0].c, '2');

    // Grow back to 5 rows on alternate screen: bottom rows appended as clean blanks
    grid.resize(20, 5);
    assert_eq!(grid.rows, 5);
    assert_eq!(grid.lines.len(), 5);
    assert_eq!(grid.visible_line(0).cells[0].c, '0');
    assert_eq!(grid.visible_line(1).cells[0].c, '1');
    assert_eq!(grid.visible_line(2).cells[0].c, '2');
    assert_eq!(grid.visible_line(3).cells[0].c, ' ');
    assert_eq!(grid.visible_line(4).cells[0].c, ' ');
}

#[test]
fn test_image_placements_pool_has_bounded_capacity() {
    let mut grid = Grid::new(80, 24, 100);
    for i in 0..1100 {
        grid.add_placement(ImagePlacement {
            image_id: i as u32,
            placement_id: 0,
            line: 0,
            col: 0,
            cols: 1,
            rows: 1,
            offset_x: 0,
            offset_y: 0,
            z_index: 0,
            ..Default::default()
        });
    }
    assert_eq!(grid.graphics.placements().len(), 1024);
    // Oldest placements 0..76 were evicted by FIFO
    assert_eq!(grid.graphics.placements()[0].image_id, 76);
}

#[test]
fn test_high_volume_scrolling_recycles_rows_and_bounds_dirty_scan() {
    let mut grid = Grid::new(80, 24, 100);
    // Write 5000 lines to exhaust scrollback and force repeated steady-state recycling
    for i in 0..5000 {
        grid.write_char(
            'X',
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
        grid.carriage_return();
        grid.newline();
        // Visible lines must remain within active screen bounds
        assert_eq!(grid.lines.len(), 24);
        assert!(grid.scrollback.len() <= 100);
        // Only active screen lines are marked dirty, not the entire scrollback
        assert!(grid.lines[23].dirty.get());
        if i % 500 == 0 {
            grid.mark_all_dirty();
        }
    }
    assert_eq!(grid.scrollback.len(), 100);
    assert_eq!(grid.lines.len(), 24);
}

#[test]
fn test_saturated_row_recycling_rebases_image_placements() {
    let mut grid = Grid::new(80, 2, 5);
    // Fill screen and scrollback to capacity (5 scrollback lines)
    for _ in 0..7 {
        grid.newline();
    }
    assert_eq!(grid.scrollback.len(), 5);

    // Add placement at line 0 (oldest line, should be evicted on next scroll)
    grid.add_placement(ImagePlacement {
        image_id: 1,
        placement_id: 0,
        line: 0,
        col: 0,
        cols: 1,
        rows: 1,
        offset_x: 0,
        offset_y: 0,
        z_index: 0,
        ..Default::default()
    });

    // Add placement at line 3 (should be decremented to line 2 on next scroll)
    grid.add_placement(ImagePlacement {
        image_id: 2,
        placement_id: 0,
        line: 3,
        col: 0,
        cols: 1,
        rows: 1,
        offset_x: 0,
        offset_y: 0,
        z_index: 0,
        ..Default::default()
    });

    // Trigger saturated row recycling
    grid.scroll_up(1);

    // Placement 1 on line 0 must be evicted
    assert!(!grid.graphics.placements().iter().any(|p| p.image_id == 1));

    // Placement 2 originally on line 3 must be decremented to line 2
    let p2 = grid
        .graphics
        .placements()
        .iter()
        .find(|p| p.image_id == 2)
        .unwrap();
    assert_eq!(p2.line, 2);
}

#[test]
fn test_vectorized_clear_line_and_reset_clears_cells() {
    let mut grid = Grid::new(20, 4, 10);
    for r in 0..4 {
        for c in 0..20 {
            grid.cursor.row = r;
            grid.cursor.col = c;
            grid.write_char(
                'Z',
                Color::Indexed(1),
                Color::Indexed(2),
                CellFlags::BOLD | CellFlags::UNDERLINE,
            );
        }
    }

    // Clear Below on line 1 from col 5
    grid.cursor.row = 1;
    grid.cursor.col = 5;
    grid.clear_line(ClearMode::Below);
    assert_eq!(grid.lines[1].cells[4].c, 'Z');
    assert_eq!(grid.lines[1].cells[5].c, ' ');
    assert_eq!(grid.lines[1].cells[19].c, ' ');

    // Clear Above on line 2 up to col 10
    grid.cursor.row = 2;
    grid.cursor.col = 10;
    grid.clear_line(ClearMode::Above);
    assert_eq!(grid.lines[2].cells[0].c, ' ');
    assert_eq!(grid.lines[2].cells[10].c, ' ');
    assert_eq!(grid.lines[2].cells[11].c, 'Z');

    // Reset row 3
    grid.lines[3].reset();
    assert!(grid.lines[3].cells.iter().all(|c| c.c == ' '));
    assert!(grid.lines[3].dirty.get());
}

#[test]
fn test_auto_scroll_viewport_bottom_bounds_dirty_marking() {
    let mut grid = Grid::new(80, 24, 100);
    // Initially at bottom (viewport_offset == 0)
    for row in &grid.lines {
        row.dirty.set(false);
    }
    // Calling scroll_viewport_bottom when already at bottom must be a no-op and preserve clean state
    grid.scroll_viewport_bottom();
    assert_eq!(grid.viewport_offset(), 0);
    assert!(!grid.lines[0].dirty.get());

    // Scroll up into history
    grid.scrollback.push_back(Row::new(80));
    grid.scroll_viewport_up(1);
    assert_eq!(grid.viewport_offset(), 1);

    // Resetting viewport to bottom marks lines dirty
    grid.scroll_viewport_bottom();
    assert_eq!(grid.viewport_offset(), 0);
    assert!(grid.lines[0].dirty.get());
}

#[test]
fn test_ascii_fast_path_writing_and_wrapping() {
    let mut grid = Grid::new(10, 3, 10);
    let s = "0123456789ABC";
    for c in s.chars() {
        grid.write_char(c, Color::Indexed(7), Color::Indexed(0), CellFlags::empty());
    }
    // "0123456789" fits on row 0, then "ABC" wraps to row 1
    assert_eq!(grid.lines[0].cells[0].c, '0');
    assert_eq!(grid.lines[0].cells[9].c, '9');
    assert_eq!(grid.lines[1].cells[0].c, 'A');
    assert_eq!(grid.lines[1].cells[1].c, 'B');
    assert_eq!(grid.lines[1].cells[2].c, 'C');
    assert_eq!(grid.cursor.row, 1);
    assert_eq!(grid.cursor.col, 3);
}

#[test]
fn test_cell_memory_footprint() {
    assert!(std::mem::size_of::<crate::grid::Cell>() <= 24);
    // One extra pointer tracks lazily allocated grapheme-cluster text for complex rows.
    assert!(std::mem::size_of::<crate::grid::Row>() <= 48);
}

#[test]
fn test_prompt_marks_navigation_and_rebasing() {
    let mut grid = Grid::new(20, 5, 10);
    grid.add_prompt_mark(0);
    grid.add_prompt_mark(2);

    grid.scroll_up(3);
    assert_eq!(grid.scrollback.len(), 3);
    assert_eq!(grid.viewport_offset, 0);

    grid.scroll_to_prompt_prev();
    assert!(grid.viewport_offset > 0);

    grid.scroll_to_prompt_next();
    assert_eq!(grid.viewport_offset, 0);
}

#[test]
fn test_clear_screen_rebases_and_clears_prompt_marks() {
    let mut grid = Grid::new(20, 5, 10);
    grid.add_prompt_mark(0);
    grid.add_prompt_mark(5);
    grid.scroll_up(3);

    grid.clear_screen(ClearMode::All);
    assert!(
        grid.prompt_marks
            .iter()
            .all(|&m| m < grid.total_evicted_rows + grid.scrollback.len())
    );

    let active_mark = grid.scrollback.len() + 1;
    grid.add_prompt_mark(active_mark);
    let old_sb = grid.scrollback.len();
    grid.clear_screen(ClearMode::Saved);
    assert_eq!(grid.scrollback.len(), 0);
    assert!(grid.has_prompt_mark_at(active_mark - old_sb));
}

#[test]
fn test_o1_prompt_marks_eviction_high_volume() {
    let mut grid = Grid::new(80, 24, 100);
    for _ in 0..500 {
        grid.add_prompt_mark(grid.scrollback.len() + grid.cursor.row);
        grid.scroll_up(1);
    }
    assert!(grid.total_evicted_rows > 0);
    assert!(grid.prompt_marks.len() <= 1024);
    assert!(
        grid.prompt_marks
            .iter()
            .all(|&m| m >= grid.total_evicted_rows)
    );

    // Navigating with prompt marks across large history works cleanly
    grid.scroll_to_prompt_prev();
    assert!(grid.viewport_offset > 0);
    grid.scroll_to_prompt_next();
    assert_eq!(grid.viewport_offset, 0);
}

#[test]
fn test_shrink_and_expand_preserves_long_lines_without_truncation() {
    let mut grid = Grid::new(80, 5, 100);
    let line1 = "Permissions Size User Date Modified Name";
    let line2 = "drwxr-xr-x     - xifan 14 Sep 02:31 Code";

    for c in line1.chars() {
        grid.write_char(
            c,
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
    }
    grid.newline();
    grid.cursor.col = 0;

    for c in line2.chars() {
        grid.write_char(
            c,
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
    }

    // Shrink horizontally to 25 columns (as in user screenshot)
    grid.resize(25, 10);
    assert_eq!(grid.cols, 25);

    // Expand back to 80 columns: text must unwrap and be 100% preserved without any truncation
    grid.resize(80, 5);
    assert_eq!(grid.cols, 80);

    let read_l1: String = grid.visible_line(0).cells[..line1.len()]
        .iter()
        .map(|c| c.c)
        .collect();
    assert_eq!(read_l1, line1);

    let read_l2: String = grid.visible_line(1).cells[..line2.len()]
        .iter()
        .map(|c| c.c)
        .collect();
    assert_eq!(read_l2, line2);
}

#[test]
fn test_reflow_shrink_to_one_column_preserves_wide_characters() {
    let mut grid = Grid::new(10, 2, 100);
    grid.write_char(
        '你',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    grid.write_char(
        '好',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );

    // Shrink to 1 column: wide characters cannot fit on a single column, but must not be deleted
    grid.resize(1, 10);
    assert_eq!(grid.cols, 1);

    // Expand back to 10 columns: both wide characters must be fully restored
    grid.resize(10, 2);
    assert_eq!(grid.cols, 10);
    assert_eq!(grid.visible_line(0).cells[0].c, '你');
    assert_eq!(grid.visible_line(0).cells[2].c, '好');
}

#[test]
fn test_reflow_exact_width_cursor_tracking() {
    let mut grid = Grid::new(5, 2, 100);
    for c in "12345".chars() {
        grid.write_char(
            c,
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
    }
    // Cursor is right at the boundary (deferred wrap at col 5)
    assert_eq!(grid.cursor.col, 5);

    // Resize to 10 columns: cursor must follow the logical line ending rather than jumping
    grid.resize(10, 2);
    assert_eq!(grid.cols, 10);
    assert_eq!(grid.cursor.row, 0);
    assert_eq!(grid.cursor.col, 5);
}

#[test]
fn test_reflow_combined_resize_preserves_cursor_row_on_screen() {
    let mut grid = Grid::new(20, 10, 100);
    for r in 0..8 {
        grid.cursor.row = r;
        grid.cursor.col = 0;
        grid.write_char(
            'A',
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
    }
    // Place cursor in middle of content (row 4)
    grid.cursor.row = 4;
    grid.cursor.col = 5;

    // Simultaneously shrink width and height: cursor row must remain on the visible screen
    grid.resize(10, 4);
    assert_eq!(grid.cols, 10);
    assert_eq!(grid.rows, 4);
    assert!(
        grid.cursor.row < 4,
        "cursor row {} must be within new screen height 4",
        grid.cursor.row
    );
}

#[test]
fn test_reflow_preserves_image_placeholders() {
    let mut grid = Grid::new(20, 2, 100);
    grid.write_char(
        KITTY_PLACEHOLDER,
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    grid.lines[0].placeholders = Some(Box::new(std::collections::HashMap::from([(
        0,
        (42, 99, 1),
    )])));

    // Resize narrower
    grid.resize(10, 2);
    assert_eq!(grid.visible_line(0).cells[0].c, KITTY_PLACEHOLDER);
    assert_eq!(
        grid.visible_line(0).placeholders.as_ref().unwrap().get(&0),
        Some(&(42, 99, 1))
    );

    // Resize wider
    grid.resize(25, 2);
    assert_eq!(grid.visible_line(0).cells[0].c, KITTY_PLACEHOLDER);
    assert_eq!(
        grid.visible_line(0).placeholders.as_ref().unwrap().get(&0),
        Some(&(42, 99, 1))
    );
}

#[test]
fn test_row_pool_recycling_on_continuous_scrolling() {
    let mut grid = Grid::new(80, 24, 100);

    // Scroll 100,000 lines: exceeds max_scrollback by a factor of 1000x
    for i in 0..100_000 {
        grid.write_char(
            'X',
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
        grid.scroll_up(1);
        if i % 1000 == 0 {
            // Pool capacity must remain bounded to at most rows.max(64)
            assert!(grid.row_pool.len() <= grid.rows.max(64));
        }
    }

    assert_eq!(grid.scrollback.len(), 100);
    // Incoming rows taken from pool must be cleanly reset
    for line in &grid.lines {
        for cell in &line.cells {
            if cell.c != 'X' {
                assert_eq!(*cell, Cell::default());
            }
        }
    }
}

#[test]
fn test_row_pool_alloc_and_resize() {
    let mut grid = Grid::new(80, 24, 100);

    let mut dirty_row = Row::new(80);
    dirty_row.cells[0].c = 'Z';
    dirty_row.wrapped = true;
    dirty_row.placeholders = Some(Box::new(std::collections::HashMap::from([(0, (1, 2, 3))])));
    grid.recycle_row(dirty_row);

    assert_eq!(grid.row_pool.len(), 1);

    // Allocate row with different column size (e.g. 120)
    let allocated = grid.alloc_row(120);
    assert_eq!(grid.row_pool.len(), 0);
    assert_eq!(allocated.cells.len(), 120);
    assert!(!allocated.wrapped);
    assert!(allocated.placeholders.is_none());
    assert_eq!(allocated.cells[0], Cell::default());
    assert!(allocated.dirty.get());
}

#[test]
fn test_clear_saved_scrollback_populates_row_pool() {
    let mut grid = Grid::new(80, 10, 50);
    for _ in 0..30 {
        grid.scroll_up(1);
    }
    assert_eq!(grid.scrollback.len(), 30);

    grid.clear_screen(ClearMode::Saved);
    assert_eq!(grid.scrollback.len(), 0);
    // Rows evicted by ClearMode::Saved must populate row_pool up to capacity
    assert!(grid.row_pool.len() >= 30);
    assert!(grid.row_pool.len() <= MAX_ROW_POOL_CAPACITY);
}

#[test]
fn test_row_pool_bounded_on_tall_grid_shrink_without_scrollback() {
    let mut grid = Grid::new(80, 1000, 0);
    grid.cursor.row = 999;

    // Shrink from 1000 rows to 5 rows with zero scrollback
    grid.resize(80, 5);
    assert_eq!(grid.rows, 5);
    assert_eq!(grid.lines.len(), 5);
    assert!(
        grid.row_pool.len() <= MAX_ROW_POOL_CAPACITY,
        "pool length {} must not exceed MAX_ROW_POOL_CAPACITY {}",
        grid.row_pool.len(),
        MAX_ROW_POOL_CAPACITY
    );
}

#[test]
fn test_image_data_byte_size_and_rgba_take() {
    let mut image = ImageData::new(1, 100, 100, vec![0; 40000]);
    assert_eq!(image.byte_size(), 40000);
    assert!(image.rgba.is_some());
    let taken = image.rgba.take();
    assert_eq!(taken.unwrap().len(), 40000);
    assert!(image.rgba.is_none());
    assert_eq!(image.byte_size(), 40000);
}

#[test]
fn test_stored_images_byte_budget_eviction() {
    let mut grid = Grid::new(80, 24, 100);

    // Each image is 2048 x 2048 x 4 bytes = 16 MB
    // With a 64 MB cap, at most 4 images can be stored simultaneously.
    for id in 1..=6 {
        grid.add_image(ImageData {
            id,
            width: 2048,
            height: 2048,
            rgba: None,
        });
    }

    assert!(
        grid.total_image_bytes() <= crate::kitty::MAX_STORED_IMAGE_BYTES,
        "total bytes {} must not exceed MAX_STORED_IMAGE_BYTES {}",
        grid.total_image_bytes(),
        crate::kitty::MAX_STORED_IMAGE_BYTES
    );
    // Oldest images (1 and 2) must have been evicted by LRU
    assert!(!grid.contains_image(1));
    assert!(!grid.contains_image(2));
    assert!(grid.contains_image(5));
    assert!(grid.contains_image(6));
}

#[test]
fn test_image_bytes_counter_tracks_add_replace_and_remove() {
    let mut grid = Grid::new(80, 24, 100);
    let size = |w: u32, h: u32| (w as usize) * (h as usize) * 4;

    grid.add_image(ImageData {
        id: 1,
        width: 10,
        height: 10,
        rgba: None,
    });
    grid.add_image(ImageData {
        id: 2,
        width: 20,
        height: 20,
        rgba: None,
    });
    assert_eq!(grid.total_image_bytes(), size(10, 10) + size(20, 20));

    // Replacing an existing id must not double count the old pixels.
    grid.add_image(ImageData {
        id: 1,
        width: 30,
        height: 5,
        rgba: None,
    });
    assert_eq!(grid.total_image_bytes(), size(30, 5) + size(20, 20));

    // Unloading the CPU buffer after GPU upload must not affect the decoded byte budget.
    grid.graphics.sync_gpu_textures(|id, _, _, _, _| id == 2);
    assert_eq!(grid.total_image_bytes(), size(30, 5) + size(20, 20));

    grid.delete_images(DeleteTarget::ByIdAndFree(1));
    assert_eq!(grid.total_image_bytes(), size(20, 20));

    grid.delete_images(DeleteTarget::AllAndFree);
    assert_eq!(grid.total_image_bytes(), 0);
    assert!(grid.images().is_empty());
}

#[test]
fn test_single_oversized_image_rejected_without_exceeding_budget() {
    let mut grid = Grid::new(80, 24, 100);
    // 5000 x 5000 x 4 bytes = 100 MB > 64 MB cap
    grid.add_image(ImageData {
        id: 999,
        width: 5000,
        height: 5000,
        rgba: None,
    });
    assert!(!grid.contains_image(999));
    assert_eq!(grid.total_image_bytes(), 0);
}

#[test]
fn test_extract_visible_and_scrollback_text() {
    let mut grid = Grid::new(80, 5, 100);
    for c in "Line 1 in history".chars() {
        grid.write_char(
            c,
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
    }
    grid.scroll_up(1);

    for c in "Line 2 visible".chars() {
        grid.write_char(
            c,
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
    }

    let visible = grid.extract_visible_text();
    assert!(visible.contains("Line 2 visible"));
    assert!(!visible.contains("Line 1 in history"));

    let scrollback = grid.extract_scrollback_text();
    assert!(scrollback.contains("Line 1 in history"));
    assert!(scrollback.contains("Line 2 visible"));
}

#[test]
fn test_viewport_scroll_marks_visible_rows_dirty_without_full_scrollback_scan() {
    let mut grid = Grid::new(80, 5, 100);
    for i in 0..20 {
        for c in format!("line {i}").chars() {
            grid.write_char(
                c,
                Color::DefaultForeground,
                Color::DefaultBackground,
                CellFlags::empty(),
            );
        }
        grid.scroll_up(1);
    }

    // Clear all dirty flags across lines and scrollback
    for row in &grid.lines {
        row.dirty.set(false);
    }
    for row in &grid.scrollback {
        row.dirty.set(false);
    }

    // Scroll up by 3 lines into scrollback
    grid.scroll_viewport_up(3);
    assert_eq!(grid.viewport_offset(), 3);

    // Visible lines must now be dirty
    for r in 0..grid.rows {
        assert!(grid.visible_line(r).dirty.get());
    }

    // Deep scrollback row that is outside the visible viewport should NOT be marked dirty
    // scrollback has 20 rows, visible covers 12..=16 when offset=3
    assert!(!grid.scrollback[0].dirty.get());
}

#[test]
fn test_image_accessors_expose_read_only_view_with_consistent_accounting() {
    let mut grid = Grid::new(80, 24, 100);
    grid.add_image(ImageData {
        id: 7,
        width: 12,
        height: 8,
        rgba: None,
    });

    assert!(grid.contains_image(7));
    assert!(!grid.contains_image(8));
    let view = grid.images();
    assert_eq!(view.len(), 1);
    assert_eq!(view.get(&7).map(|img| img.width), Some(12));
    // The cached budget must always match the read-only view of stored images.
    let recomputed: usize = grid.images().values().map(ImageData::byte_size).sum();
    assert_eq!(grid.total_image_bytes(), recomputed);
}

#[test]
fn test_grid_cell_and_row_constructors() {
    let _map: PlaceholderMap = std::collections::HashMap::new();
    let row = Row::new(80);
    assert_eq!(row.cells.len(), 80);
    let _cell = Cell::default();
    let _flags = CellFlags::empty();
    let _mode = ClearMode::All;
    let _cur = Cursor::default();
    let _shape = CursorShape::Block;
}

#[test]
fn test_grid_bce_methods() {
    let mut grid = Grid::new(40, 10, 20);
    let bg = Color::Indexed(3);

    // clear_screen_with_bg
    grid.clear_screen_with_bg(ClearMode::All, bg);
    assert_eq!(grid.lines[0].cells[0].bg, bg);
    assert_eq!(grid.lines[9].cells[39].bg, bg);

    // clear_line_with_bg
    let red_bg = Color::Indexed(1);
    grid.cursor.row = 2;
    grid.cursor.col = 10;
    grid.clear_line_with_bg(ClearMode::Below, red_bg);
    assert_eq!(grid.lines[2].cells[0].bg, bg);
    assert_eq!(grid.lines[2].cells[10].bg, red_bg);
    assert_eq!(grid.lines[2].cells[39].bg, red_bg);

    // erase_chars_with_bg
    let green_bg = Color::Indexed(2);
    grid.cursor.row = 4;
    grid.cursor.col = 5;
    grid.erase_chars_with_bg(10, green_bg);
    assert_eq!(grid.lines[4].cells[4].bg, bg);
    assert_eq!(grid.lines[4].cells[5].bg, green_bg);
    assert_eq!(grid.lines[4].cells[14].bg, green_bg);
    assert_eq!(grid.lines[4].cells[15].bg, bg);
}

#[test]
fn test_bce_erase_wide_character_boundaries() {
    let mut grid = Grid::new(20, 5, 10);
    // Write wide char '中' at col 4 (takes col 4 and 5)
    grid.cursor.row = 1;
    grid.cursor.col = 4;
    grid.write_char(
        '中',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    assert!(grid.lines[1].cells[4].flags.contains(CellFlags::WIDE_CHAR));
    assert!(
        grid.lines[1].cells[5]
            .flags
            .contains(CellFlags::WIDE_CHAR_SPACER)
    );

    // Erasing only col 5 (the spacer) must reconcile and clear leading cell at col 4 as well
    let bg = Color::Indexed(4);
    grid.cursor.row = 1;
    grid.cursor.col = 5;
    grid.erase_chars_with_bg(1, bg);
    assert_eq!(grid.lines[1].cells[4].c, ' ');
    assert!(!grid.lines[1].cells[4].flags.contains(CellFlags::WIDE_CHAR));
    assert_eq!(grid.lines[1].cells[4].bg, bg);
    assert_eq!(grid.lines[1].cells[5].c, ' ');
    assert!(
        !grid.lines[1].cells[5]
            .flags
            .contains(CellFlags::WIDE_CHAR_SPACER)
    );
    assert_eq!(grid.lines[1].cells[5].bg, bg);

    // Write wide char '文' at col 8 (takes col 8 and 9)
    grid.cursor.row = 1;
    grid.cursor.col = 8;
    grid.write_char(
        '文',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    assert!(grid.lines[1].cells[8].flags.contains(CellFlags::WIDE_CHAR));
    assert!(
        grid.lines[1].cells[9]
            .flags
            .contains(CellFlags::WIDE_CHAR_SPACER)
    );

    // Erasing up to col 8 (touching only the WIDE_CHAR leading cell) must also clear col 9 (the spacer)
    let red_bg = Color::Indexed(1);
    grid.cursor.row = 1;
    grid.cursor.col = 7;
    grid.erase_chars_with_bg(2, red_bg); // touches 7 and 8
    assert_eq!(grid.lines[1].cells[8].c, ' ');
    assert!(!grid.lines[1].cells[8].flags.contains(CellFlags::WIDE_CHAR));
    assert_eq!(grid.lines[1].cells[8].bg, red_bg);
    assert_eq!(grid.lines[1].cells[9].c, ' ');
    assert!(
        !grid.lines[1].cells[9]
            .flags
            .contains(CellFlags::WIDE_CHAR_SPACER)
    );
    assert_eq!(grid.lines[1].cells[9].bg, red_bg);
}

#[test]
fn test_grapheme_cluster_widths_follow_python_wcswidth() {
    let fg = Color::DefaultForeground;
    let bg = Color::DefaultBackground;

    // Family ZWJ sequence: two cells, one cluster, cursor advances by two.
    let mut grid = Grid::new(20, 2, 0);
    for c in "👨\u{200d}👩\u{200d}👧\u{200d}👦".chars() {
        grid.write_char(c, fg, bg, CellFlags::empty());
    }
    assert_eq!(grid.cursor.col, 2);
    assert!(grid.lines[0].cells[0].flags.contains(CellFlags::WIDE_CHAR));
    assert!(
        grid.lines[0].cells[1]
            .flags
            .contains(CellFlags::WIDE_CHAR_SPACER)
    );
    assert_eq!(
        grid.lines[0].cluster_text(0),
        Some("👨\u{200d}👩\u{200d}👧\u{200d}👦")
    );

    // VS16 widens a narrow base and narrows matching state.
    let mut grid = Grid::new(10, 1, 0);
    for c in "❤\u{fe0f}".chars() {
        grid.write_char(c, fg, bg, CellFlags::empty());
    }
    assert_eq!(grid.cursor.col, 2);
    assert_eq!(grid.lines[0].cluster_text(0), Some("❤\u{fe0f}"));

    let mut grid = Grid::new(10, 1, 0);
    for c in "⌚\u{fe0e}".chars() {
        grid.write_char(c, fg, bg, CellFlags::empty());
    }
    assert_eq!(grid.cursor.col, 1);
    assert!(!grid.lines[0].cells[0].flags.contains(CellFlags::WIDE_CHAR));

    // Regional-indicator flags and Fitzpatrick modifiers stay in one double cell.
    let mut grid = Grid::new(10, 1, 0);
    for c in "🇺🇸".chars() {
        grid.write_char(c, fg, bg, CellFlags::empty());
    }
    assert_eq!(grid.cursor.col, 2);
    assert_eq!(grid.lines[0].cluster_text(0), Some("🇺🇸"));

    let mut grid = Grid::new(10, 1, 0);
    for c in "👍🏽".chars() {
        grid.write_char(c, fg, bg, CellFlags::empty());
    }
    assert_eq!(grid.cursor.col, 2);
    assert_eq!(grid.lines[0].cluster_text(0), Some("👍🏽"));

    // Spacing marks cap the cluster at two cells instead of disappearing.
    let mut grid = Grid::new(10, 1, 0);
    for c in "कि".chars() {
        grid.write_char(c, fg, bg, CellFlags::empty());
    }
    assert_eq!(grid.cursor.col, 2);
    assert_eq!(grid.lines[0].cluster_text(0), Some("कि"));

    // Virama conjuncts also cap at two cells.
    let mut grid = Grid::new(10, 1, 0);
    for c in "क्न".chars() {
        grid.write_char(c, fg, bg, CellFlags::empty());
    }
    assert_eq!(grid.cursor.col, 2);
    assert_eq!(grid.lines[0].cluster_text(0), Some("क्न"));
}

#[test]
fn test_ambiguous_width_is_configurable() {
    let fg = Color::DefaultForeground;
    let bg = Color::DefaultBackground;
    let mut grid = Grid::new(10, 1, 0);
    grid.write_char('¡', fg, bg, CellFlags::empty());
    assert_eq!(grid.cursor.col, 1);
    grid.set_ambiguous_width(2);
    grid.write_char('¡', fg, bg, CellFlags::empty());
    assert_eq!(grid.cursor.col, 3);
    assert!(grid.lines[0].cells[1].flags.contains(CellFlags::WIDE_CHAR));
}

#[test]
fn test_grapheme_clusters_survive_edits_reflow_and_extraction() {
    let fg = Color::DefaultForeground;
    let bg = Color::DefaultBackground;
    let family = "👨\u{200d}👩\u{200d}👧\u{200d}👦";

    let mut grid = Grid::new(10, 2, 100);
    for c in family.chars() {
        grid.write_char(c, fg, bg, CellFlags::empty());
    }
    grid.write_char('X', fg, bg, CellFlags::empty());
    assert_eq!(grid.extract_visible_text(), format!("{family}X\n\n"));

    // Insert/delete/erase move the cluster text with its root cell.
    grid.cursor.col = 0;
    grid.insert_blank_chars(1);
    assert_eq!(grid.lines[0].cluster_text(1), Some(family));
    grid.delete_chars(1);
    assert_eq!(grid.lines[0].cluster_text(0), Some(family));
    grid.cursor.col = 0;
    grid.erase_chars(2);
    assert_eq!(grid.lines[0].cluster_text(0), None);
    assert_eq!(grid.lines[0].cells[0].c, ' ');

    // Reflow keeps cluster text attached while wrapping and growing back.
    let mut grid = Grid::new(10, 2, 100);
    for c in family.chars() {
        grid.write_char(c, fg, bg, CellFlags::empty());
    }
    grid.resize(1, 2);
    grid.resize(10, 2);
    assert_eq!(grid.visible_line(0).cluster_text(0), Some(family));
    assert_eq!(grid.cursor.col, 2);
}

#[test]
fn test_ascii_run_then_combining_mark_stays_in_one_cluster() {
    let fg = Color::DefaultForeground;
    let bg = Color::DefaultBackground;
    let mut grid = Grid::new(10, 1, 0);
    grid.write_ascii_run(
        b"e",
        fg,
        bg,
        CellFlags::empty(),
        Color::DefaultForeground,
        None,
    );
    grid.write_char('\u{0301}', fg, bg, CellFlags::empty());
    assert_eq!(grid.cursor.col, 1);
    assert_eq!(grid.lines[0].cluster_text(0), Some("e\u{0301}"));
}

#[test]
fn test_ascii_run_preserves_adjacent_cell_and_cleans_spacer_boundary() {
    let fg = Color::DefaultForeground;
    let bg = Color::Rgb(37, 65, 49);
    let mut grid = Grid::new(20, 1, 0);

    // Initial state: wide char '中' at col 5 (takes 5 and 6)
    grid.cursor.col = 5;
    grid.write_char('中', fg, bg, CellFlags::empty());
    assert_eq!(grid.lines[0].cells[5].c, '中');
    assert!(
        grid.lines[0].cells[6]
            .flags
            .contains(CellFlags::WIDE_CHAR_SPACER)
    );

    // Next redraw: ASCII run writes 0..=5 ending with '(' at col 5
    grid.cursor.col = 0;
    grid.write_ascii_run(
        b"12345(",
        fg,
        bg,
        CellFlags::empty(),
        Color::DefaultForeground,
        None,
    );
    assert_eq!(grid.cursor.col, 6);
    assert_eq!(grid.lines[0].cells[5].c, '(');
    assert_eq!(grid.lines[0].cells[5].bg, bg);
    assert!(
        !grid.lines[0].cells[6]
            .flags
            .contains(CellFlags::WIDE_CHAR_SPACER)
    );

    // Next char '微' written at col 6: must NOT wipe out col 5 '('
    grid.write_char('微', fg, bg, CellFlags::empty());
    assert_eq!(grid.lines[0].cells[5].c, '(');
    assert_eq!(grid.lines[0].cells[5].bg, bg);
    assert_eq!(grid.lines[0].cells[6].c, '微');
    assert_eq!(grid.lines[0].cells[6].bg, bg);
}

#[test]
fn test_overwriting_wide_char_with_narrow_preserves_background_color() {
    let fg = Color::DefaultForeground;
    let bg = Color::Rgb(37, 65, 49);
    let mut grid = Grid::new(20, 1, 0);

    grid.clear_line_with_bg(ClearMode::All, bg);
    grid.cursor.col = 0;
    grid.write_char('中', fg, bg, CellFlags::empty());
    assert_eq!(grid.lines[0].cells[1].flags, CellFlags::WIDE_CHAR_SPACER);
    assert_eq!(grid.lines[0].cells[1].bg, bg);

    // Overwrite col 0 with narrow 'A', keeping the same background
    grid.cursor.col = 0;
    grid.write_char('A', fg, bg, CellFlags::empty());
    assert_eq!(grid.lines[0].cells[0].c, 'A');
    assert_eq!(grid.lines[0].cells[0].bg, bg);
    assert_eq!(grid.lines[0].cells[1].c, ' ');
    assert_eq!(grid.lines[0].cells[1].bg, bg);
    assert_eq!(grid.lines[0].cells[1].flags, CellFlags::empty());
}

#[test]
fn test_row_resize_truncating_wide_char_preserves_background_color() {
    let fg = Color::DefaultForeground;
    let bg = Color::Rgb(37, 65, 49);
    let mut row = Row::blank(10, bg);
    row.cells[8] = Cell {
        c: '中',
        fg,
        bg,
        underline_color: fg,
        flags: CellFlags::WIDE_CHAR,
        hyperlink_id: None,
    };
    row.cells[9] = Cell {
        c: ' ',
        fg,
        bg,
        underline_color: fg,
        flags: CellFlags::WIDE_CHAR_SPACER,
        hyperlink_id: None,
    };

    // Resize truncates col 9 (the spacer). Col 8 leader must decouple to ' ' keeping bg
    row.resize(9);
    assert_eq!(row.cells.len(), 9);
    assert_eq!(row.cells[8].c, ' ');
    assert_eq!(row.cells[8].bg, bg);
    assert_eq!(row.cells[8].flags, CellFlags::empty());
}

mod reflow_module_tests {
    use crate::color::Color;
    use crate::grid::reflow::{reflow_primary_screen, shrink_rows};
    use crate::grid::{CellFlags, Grid, Row};

    #[test]
    fn test_shrink_rows_preserves_cursor_line() {
        let mut lines = vec![
            Row::new(10), // Row 0
            Row::new(10), // Row 1 (Cursor)
            Row::new(10), // Row 2
            Row::new(10), // Row 3
        ];

        // Cursor at row 1. Remove 2 rows.
        // Below cursor = 3 - 1 = 2 rows (Row 2 and Row 3).
        // Since from_bottom = min(2, 2) = 2, from_top = 0.
        let removed = shrink_rows(&mut lines, 2, 1, 2);
        assert!(removed.is_empty(), "bottom rows discarded first");
        assert_eq!(lines.len(), 2);

        // Now with cursor at row 0, remove 2 rows.
        // Below cursor = 1 - 0 = 1 row (Row 1).
        // from_bottom = min(2, 1) = 1, from_top = 1.
        let mut lines2 = vec![Row::new(10), Row::new(10)];
        let removed_top = shrink_rows(&mut lines2, 1, 0, 1);
        assert!(removed_top.is_empty());
        assert_eq!(lines2.len(), 1);
    }

    #[test]
    fn test_reflow_primary_screen_direct_invocation() {
        let mut grid = Grid::new(20, 4, 100);
        // Write long line that will wrap when width shrinks
        for c in "first line of text that wraps nicely".chars() {
            grid.write_char(
                c,
                Color::DefaultForeground,
                Color::DefaultBackground,
                CellFlags::empty(),
            );
        }

        // Direct reflow to width 10
        reflow_primary_screen(&mut grid, 10, 8);

        assert_eq!(grid.cols, 10);
        assert_eq!(grid.rows, 8);
        assert_eq!(grid.lines[0].cells.len(), 10);
        assert!(grid.lines[0].wrapped);
    }
}
}
