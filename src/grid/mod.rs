//! Terminal cell grid, cursor management, and scrollback ring buffer.

pub mod diacritics;
pub mod resize;
pub mod row;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};
use unicode_width::UnicodeWidthChar;

use crate::color::Color;
use crate::kitty::{DeleteTarget, ImageData, ImagePlacement};

pub use diacritics::{KITTY_PLACEHOLDER, diacritic_to_index};
pub use row::{Cell, CellFlags, ClearMode, Cursor, CursorShape, Row};

pub(crate) const MAX_PLACEMENTS: usize = 1024;
pub(crate) const MAX_STORED_IMAGES: usize = 256;

/// 2D Screen grid with scrollback history and alternate screen support.
#[derive(Debug, Clone)]
pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    pub max_scrollback: usize,

    pub lines: Vec<Row>,
    pub scrollback: VecDeque<Row>,
    pub viewport_offset: usize,

    pub images: HashMap<u32, ImageData>,
    pub image_versions: HashMap<u32, u64>,
    pub placements: Vec<ImagePlacement>,
    pub virtual_placements: HashMap<u32, (usize, usize)>,

    pub cursor: Cursor,
    pub saved_cursor: Cursor,

    pub scroll_region_top: usize,
    pub scroll_region_bottom: usize,

    // The hidden primary screen while the alternate screen is active. Each screen owns its saved
    // cursor and placements so a resize on one cannot shift the other's coordinates.
    pub(crate) alt_lines: Option<Vec<Row>>,
    pub(crate) alt_cursor: Option<Cursor>,
    pub(crate) alt_saved_cursor: Option<Cursor>,
    pub(crate) alt_placements: Vec<ImagePlacement>,
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
            images: HashMap::new(),
            image_versions: HashMap::new(),
            placements: Vec::new(),
            virtual_placements: HashMap::new(),
            cursor: Cursor::default(),
            saved_cursor: Cursor::default(),
            scroll_region_top: 0,
            scroll_region_bottom: actual_rows.saturating_sub(1),
            alt_lines: None,
            alt_cursor: None,
            alt_saved_cursor: None,
            alt_placements: Vec::new(),
        }
    }

    #[must_use]
    pub fn is_alt_screen(&self) -> bool {
        self.alt_lines.is_some()
    }

    pub fn add_image(&mut self, image: ImageData) {
        let id = image.id;
        self.images.insert(id, image);
        let ver = self.image_versions.entry(id).or_insert(0);
        *ver = ver.wrapping_add(1);

        // Cap stored images to MAX_STORED_IMAGES by removing unplaced images
        if self.images.len() > MAX_STORED_IMAGES {
            let mut active_ids: std::collections::HashSet<u32> = self
                .placements
                .iter()
                .chain(self.alt_placements.iter())
                .map(|p| p.image_id)
                .collect();
            // Virtual image IDs stay alive while a placeholder on either screen references them.
            for row in self.lines.iter().chain(self.alt_lines.iter().flatten()) {
                for cell in &row.cells {
                    if cell.c != KITTY_PLACEHOLDER {
                        continue;
                    }
                    let id_low24 = match cell.fg {
                        Color::Rgb(r, g, b) => ((r as u32) << 16) | ((g as u32) << 8) | (b as u32),
                        Color::Indexed(idx) => idx as u32,
                        _ => 0,
                    } & 0x00FF_FFFF;
                    if id_low24 == 0 {
                        continue;
                    }
                    active_ids.insert(id_low24);
                    if let Some(&real_id) = self
                        .virtual_placements
                        .keys()
                        .find(|&&k| (k & 0x00FF_FFFF) == id_low24)
                    {
                        active_ids.insert(real_id);
                    }
                }
            }
            self.virtual_placements
                .retain(|id, _| active_ids.contains(id));
            self.images
                .retain(|img_id, _| active_ids.contains(img_id) || *img_id == id);
            self.image_versions
                .retain(|img_id, _| self.images.contains_key(img_id));

            // If active placements exceed capacity, evict oldest unreferenced images
            if self.images.len() > MAX_STORED_IMAGES {
                let to_evict = self.images.len() - MAX_STORED_IMAGES;
                let evict_keys: Vec<u32> = self
                    .images
                    .keys()
                    .copied()
                    .filter(|&k| k != id)
                    .take(to_evict)
                    .collect();
                for k in evict_keys {
                    self.images.remove(&k);
                    self.image_versions.remove(&k);
                    self.placements.retain(|p| p.image_id != k);
                    self.alt_placements.retain(|p| p.image_id != k);
                    self.virtual_placements.remove(&k);
                }
            }
        }
    }

    /// Adds an image placement instance anchored to grid cells with FIFO eviction.
    pub fn add_placement(&mut self, placement: ImagePlacement) {
        if self.placements.len() >= MAX_PLACEMENTS {
            self.placements.remove(0);
        }
        self.placements.push(placement);
    }

    /// Deletes images and/or placements matching the given delete target.
    pub fn delete_images(&mut self, target: DeleteTarget) {
        match target {
            DeleteTarget::All => {
                self.images.clear();
                self.image_versions.clear();
                self.placements.clear();
                self.alt_placements.clear();
                self.virtual_placements.clear();
            }
            DeleteTarget::ById(id) => {
                self.images.remove(&id);
                self.image_versions.remove(&id);
                self.placements.retain(|p| p.image_id != id);
                self.alt_placements.retain(|p| p.image_id != id);
                self.virtual_placements.remove(&id);
            }
            DeleteTarget::ByPlacement(p_id) => {
                self.placements.retain(|p| p.placement_id != p_id);
                self.alt_placements.retain(|p| p.placement_id != p_id);
            }
            DeleteTarget::AtCursor => {
                let cursor_row = self.cursor.row;
                let cursor_col = self.cursor.col;
                let abs_line = self.scrollback.len() + cursor_row;
                self.placements.retain(|p| {
                    !(p.line == abs_line && cursor_col >= p.col && cursor_col < p.col + p.cols)
                });
            }
        }
    }

    pub fn enter_alt_screen(&mut self) {
        if self.alt_lines.is_none() {
            let alt = (0..self.rows).map(|_| Row::new(self.cols)).collect();
            self.alt_lines = Some(std::mem::replace(&mut self.lines, alt));
            self.alt_cursor = Some(self.cursor);
            self.alt_saved_cursor = Some(self.saved_cursor);
            self.cursor = Cursor::default();
            self.saved_cursor = Cursor::default();
            // Each screen owns its placements; the primary's are parked until it is restored.
            self.alt_placements = std::mem::take(&mut self.placements);
            self.viewport_offset = 0;
        }
    }

    /// Restores the primary screen buffer.
    pub fn exit_alt_screen(&mut self) {
        if let Some(primary) = self.alt_lines.take() {
            self.lines = primary;
            if let Some(cursor) = self.alt_cursor.take() {
                self.cursor = cursor;
            }
            if let Some(saved) = self.alt_saved_cursor.take() {
                self.saved_cursor = saved;
            }
            self.placements = std::mem::take(&mut self.alt_placements);
            self.viewport_offset = 0;
            self.mark_all_dirty();
        }
    }

    /// Marks all rows in the visible screen, alternate screen, and scrollback as dirty.
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
            for row in &self.scrollback {
                row.dirty.set(true);
            }
        }
    }

    pub(crate) fn evict_oldest_scrollback_row(&mut self) -> Option<Row> {
        let row = self.scrollback.pop_front()?;
        if !self.placements.is_empty() {
            self.placements.retain_mut(|p| {
                if p.line == 0 {
                    false
                } else {
                    p.line -= 1;
                    true
                }
            });
        }
        Some(row)
    }

    pub(crate) fn push_scrollback(&mut self, row: Row) {
        if self.max_scrollback == 0 {
            return;
        }
        if self.scrollback.len() >= self.max_scrollback {
            self.evict_oldest_scrollback_row();
        }
        self.scrollback.push_back(row);
        // If user is currently viewing history, keep the view anchored on the same lines
        if self.viewport_offset > 0 {
            self.viewport_offset = (self.viewport_offset + 1).min(self.scrollback.len());
            self.mark_all_dirty();
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
            self.mark_all_dirty();
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
            self.mark_all_dirty();
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
            self.mark_all_dirty();
        }
    }

    /// Resets the viewport offset to 0 (bottom of active screen).
    pub fn scroll_viewport_bottom(&mut self) {
        if self.viewport_offset != 0 {
            self.viewport_offset = 0;
            self.mark_all_dirty();
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

    /// Scrolls lines inside the active scroll region upward.
    pub fn scroll_up(&mut self, count: usize) {
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

        if is_full_screen && self.max_scrollback > 0 {
            for i in 0..count {
                if self.scrollback.len() >= self.max_scrollback {
                    if let Some(recycled) = self.evict_oldest_scrollback_row() {
                        let old = std::mem::replace(&mut self.lines[i], recycled);
                        self.scrollback.push_back(old);
                        if self.viewport_offset > 0 {
                            self.viewport_offset =
                                (self.viewport_offset + 1).min(self.scrollback.len());
                            self.mark_all_dirty();
                        }
                    }
                } else {
                    let fresh = Row::new(self.cols);
                    let old = std::mem::replace(&mut self.lines[i], fresh);
                    self.push_scrollback(old);
                }
            }
        }

        self.lines[self.scroll_region_top..=self.scroll_region_bottom].rotate_left(count);
        for row in
            &mut self.lines[self.scroll_region_bottom + 1 - count..=self.scroll_region_bottom]
        {
            row.reset();
        }
        for row in &self.lines[self.scroll_region_top..=self.scroll_region_bottom] {
            row.dirty.set(true);
        }
    }

    /// Scrolls lines inside the active scroll region downward.
    pub fn scroll_down(&mut self, count: usize) {
        let region_len = self
            .scroll_region_bottom
            .saturating_sub(self.scroll_region_top)
            + 1;
        let count = count.min(region_len);
        if count == 0 {
            return;
        }

        self.lines[self.scroll_region_top..=self.scroll_region_bottom].rotate_right(count);
        for row in &mut self.lines[self.scroll_region_top..self.scroll_region_top + count] {
            row.reset();
        }
        for row in &self.lines[self.scroll_region_top..=self.scroll_region_bottom] {
            row.dirty.set(true);
        }
    }

    /// Clears part or all of the active display screen.
    pub fn clear_screen(&mut self, mode: ClearMode) {
        match mode {
            ClearMode::Below => {
                if self.cursor.row < self.rows {
                    let col = self.cursor.col;
                    self.lines[self.cursor.row].cells[col..].fill(Cell::default());
                    for row in &mut self.lines[self.cursor.row + 1..] {
                        row.reset();
                    }
                    self.lines[self.cursor.row].dirty.set(true);
                }
            }
            ClearMode::Above => {
                if self.cursor.row < self.rows {
                    for row in &mut self.lines[..self.cursor.row] {
                        row.reset();
                    }
                    let col = (self.cursor.col + 1).min(self.cols);
                    self.lines[self.cursor.row].cells[..col].fill(Cell::default());
                    self.lines[self.cursor.row].dirty.set(true);
                }
            }
            ClearMode::All => {
                for row in &mut self.lines {
                    row.reset();
                }
                self.mark_all_dirty();
            }
            ClearMode::Saved => {
                let sb_len = self.scrollback.len();
                self.scrollback.clear();
                self.viewport_offset = 0;
                // Both screens share the scrollback base, so the hidden primary's placements need
                // the same rebase as the active screen's.
                for placements in [&mut self.placements, &mut self.alt_placements] {
                    placements.retain_mut(|p| {
                        if p.line < sb_len {
                            false
                        } else {
                            p.line -= sb_len;
                            true
                        }
                    });
                }
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

    /// Clears part or all of the current cursor line.
    pub fn clear_line(&mut self, mode: ClearMode) {
        if self.cursor.row >= self.rows {
            return;
        }
        let row = &mut self.lines[self.cursor.row];
        match mode {
            ClearMode::Below => {
                let start = self.cursor.col.min(self.cols);
                row.cells[start..].fill(Cell::default());
            }
            ClearMode::Above => {
                let end = (self.cursor.col + 1).min(self.cols);
                row.cells[..end].fill(Cell::default());
            }
            ClearMode::All | ClearMode::Saved => {
                row.reset();
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
        let mut rest = text;
        while !rest.is_empty() {
            if self.cursor.col >= self.cols {
                self.lines[self.cursor.row].wrapped = true;
                self.newline();
                self.cursor.col = 0;
            }

            let row = self.cursor.row;
            if row >= self.rows {
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

            let row_line = &mut self.lines[row];
            for (idx, &byte) in chunk.iter().enumerate() {
                row_line.cells[col + idx] = Cell {
                    c: byte as char,
                    fg,
                    bg,
                    underline_color,
                    flags,
                    hyperlink_id: hl_id,
                };
            }
            if let Some(coords) = &mut row_line.placeholders {
                for c in col..col + take {
                    coords.remove(&c);
                }
            }
            row_line.dirty.set(true);
            self.cursor.col += take;
        }
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
        if c.is_ascii() && c >= ' ' {
            let col = self.cursor.col;
            let row = self.cursor.row;
            if col < self.cols && row < self.rows {
                self.lines[row].cells[col] = Cell {
                    c,
                    fg,
                    bg,
                    underline_color,
                    flags,
                    hyperlink_id: hl_id,
                };
                if !self.lines[row].dirty.get() {
                    self.lines[row].dirty.set(true);
                }
                if let Some(coords) = &mut self.lines[row].placeholders {
                    coords.remove(&col);
                }
                self.cursor.col += 1;
                return;
            }
        }

        let width = if c.is_ascii() && c >= ' ' {
            1
        } else {
            c.width().unwrap_or(1)
        };
        if width == 0 {
            // Combining character: decode Kitty Unicode placeholder diacritics
            if let Some(idx) = diacritic_to_index(c)
                && self.cursor.col > 0
                && self.cursor.row < self.rows
            {
                let target_col = self.cursor.col - 1;
                let target_row = self.cursor.row;
                if self.lines[target_row].cells[target_col].c == KITTY_PLACEHOLDER
                    && let Some(coords) = &mut self.lines[target_row].placeholders
                    && let Some(coord) = coords.get_mut(&target_col)
                {
                    match coord.2 {
                        0 => coord.0 = idx,
                        1 => coord.1 = idx,
                        _ => {}
                    }
                    coord.2 = coord.2.saturating_add(1);
                }
            }
            return;
        }

        // Line wrap if wide char doesn't fit or col reached end
        if self.cursor.col + width > self.cols {
            self.lines[self.cursor.row].wrapped = true;
            self.newline();
            self.cursor.col = 0;
        }

        if self.cursor.row >= self.rows || self.cursor.col >= self.cols {
            return;
        }

        let mut cell_flags = flags;
        if width == 2 {
            cell_flags |= CellFlags::WIDE_CHAR;
        }

        let col = self.cursor.col;
        let row = self.cursor.row;
        self.lines[row].cells[col] = Cell {
            c,
            fg,
            bg,
            underline_color,
            flags: cell_flags,
            hyperlink_id: hl_id,
        };
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
                .get_or_insert_with(HashMap::new)
                .insert(col, (img_row, img_col, 0));
        } else if let Some(coords) = &mut self.lines[row].placeholders {
            coords.remove(&col);
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
        }

        self.cursor.col += width;
        if self.cursor.col >= self.cols {
            // Defer wrapping until next printable char or explicit move
            self.cursor.col = self.cols;
        }
    }

    /// Performs a newline (LF): moves cursor down, scrolling if necessary.
    pub fn newline(&mut self) {
        if self.cursor.row == self.scroll_region_bottom {
            self.scroll_up(1);
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

    /// Inserts blank characters at cursor, shifting remaining characters right.
    pub fn insert_blank_chars(&mut self, count: usize) {
        if self.cursor.row >= self.rows || self.cursor.col >= self.cols {
            return;
        }
        let row = &mut self.lines[self.cursor.row];
        let col = self.cursor.col;
        let count = count.min(self.cols - col);

        for i in (col + count..self.cols).rev() {
            row.cells[i] = row.cells[i - count];
        }
        for cell in &mut row.cells[col..col + count] {
            cell.reset();
        }
        row.dirty.set(true);
    }

    /// Deletes characters at cursor, shifting remaining characters left.
    pub fn delete_chars(&mut self, count: usize) {
        if self.cursor.row >= self.rows || self.cursor.col >= self.cols {
            return;
        }
        let row = &mut self.lines[self.cursor.row];
        let col = self.cursor.col;
        let count = count.min(self.cols - col);

        for i in col..self.cols - count {
            row.cells[i] = row.cells[i + count];
        }
        for cell in &mut row.cells[self.cols - count..] {
            cell.reset();
        }
        row.dirty.set(true);
    }

    /// Inserts lines at cursor row, shifting lines down.
    pub fn insert_lines(&mut self, count: usize) {
        if self.cursor.row < self.scroll_region_top || self.cursor.row > self.scroll_region_bottom {
            return;
        }
        let count = count.min(self.scroll_region_bottom - self.cursor.row + 1);
        for _ in 0..count {
            self.lines.remove(self.scroll_region_bottom);
            self.lines.insert(self.cursor.row, Row::new(self.cols));
        }
        self.mark_all_dirty();
    }

    /// Deletes lines at cursor row, shifting lines up.
    pub fn delete_lines(&mut self, count: usize) {
        if self.cursor.row < self.scroll_region_top || self.cursor.row > self.scroll_region_bottom {
            return;
        }
        let count = count.min(self.scroll_region_bottom - self.cursor.row + 1);
        for _ in 0..count {
            self.lines.remove(self.cursor.row);
            self.lines
                .insert(self.scroll_region_bottom, Row::new(self.cols));
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
