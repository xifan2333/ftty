//! Terminal cell grid, cursor management, and scrollback ring buffer.

use std::collections::{HashMap, VecDeque};
use unicode_width::UnicodeWidthChar;

use crate::color::Color;
use crate::kitty::{DeleteTarget, ImageData, ImagePlacement};

bitflags::bitflags! {
    /// Visual and semantic attributes attached to a terminal cell.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct CellFlags: u16 {
        const BOLD = 1 << 0;
        const DIM = 1 << 1;
        const ITALIC = 1 << 2;
        const UNDERLINE = 1 << 3;
        const REVERSE = 1 << 4;
        const HIDDEN = 1 << 5;
        const STRIKETHROUGH = 1 << 6;
        const WIDE_CHAR = 1 << 7;
        const WIDE_CHAR_SPACER = 1 << 8;
    }
}

/// A single character cell within the terminal grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: CellFlags,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            c: ' ',
            fg: Color::DefaultForeground,
            bg: Color::DefaultBackground,
            flags: CellFlags::empty(),
        }
    }
}

impl Cell {
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// The visual style of the text cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorShape {
    #[default]
    Block,
    Beam,
    Underline,
}

/// Cursor coordinate and visibility state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub visible: bool,
    pub shape: CursorShape,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            row: 0,
            col: 0,
            visible: true,
            shape: CursorShape::Block,
        }
    }
}

/// Clear direction mode for ED (Erase in Display) and EL (Erase in Line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearMode {
    Below,
    Above,
    All,
    Saved,
}

/// A horizontal row of cells in the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
}

impl Row {
    #[must_use]
    pub fn new(cols: usize) -> Self {
        Self {
            cells: vec![Cell::default(); cols],
            wrapped: false,
        }
    }

    pub fn resize(&mut self, new_cols: usize) {
        self.cells.resize(new_cols, Cell::default());
    }

    pub fn reset(&mut self) {
        for cell in &mut self.cells {
            cell.reset();
        }
        self.wrapped = false;
    }
}

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
    pub placements: Vec<ImagePlacement>,

    pub cursor: Cursor,
    pub saved_cursor: Cursor,

    pub scroll_region_top: usize,
    pub scroll_region_bottom: usize,

    alt_lines: Option<Vec<Row>>,
    alt_cursor: Option<Cursor>,
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
            placements: Vec::new(),
            cursor: Cursor::default(),
            saved_cursor: Cursor::default(),
            scroll_region_top: 0,
            scroll_region_bottom: actual_rows.saturating_sub(1),
            alt_lines: None,
            alt_cursor: None,
        }
    }

    #[must_use]
    pub fn is_alt_screen(&self) -> bool {
        self.alt_lines.is_some()
    }

    /// Adds a decoded image to the grid's image store.
    pub fn add_image(&mut self, image: ImageData) {
        self.images.insert(image.id, image);
    }

    /// Adds an image placement instance anchored to grid cells.
    pub fn add_placement(&mut self, placement: ImagePlacement) {
        self.placements.push(placement);
    }

    /// Deletes images and/or placements matching the given delete target.
    pub fn delete_images(&mut self, target: DeleteTarget) {
        match target {
            DeleteTarget::All => {
                self.images.clear();
                self.placements.clear();
            }
            DeleteTarget::ById(id) => {
                self.images.remove(&id);
                self.placements.retain(|p| p.image_id != id);
            }
            DeleteTarget::ByPlacement(p_id) => {
                self.placements.retain(|p| p.placement_id != p_id);
            }
            DeleteTarget::AtCursor => {
                let cursor_row = self.cursor.row;
                let cursor_col = self.cursor.col;
                let abs_line = self.scrollback.len() + cursor_row - self.viewport_offset;
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
            self.cursor = Cursor::default();
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
            self.viewport_offset = 0;
        }
    }

    /// Resizes the grid dimensions.
    pub fn resize(&mut self, new_cols: usize, new_rows: usize) {
        let new_cols = new_cols.max(1);
        let new_rows = new_rows.max(1);

        for row in &mut self.lines {
            row.resize(new_cols);
        }

        if new_rows > self.rows {
            for _ in self.rows..new_rows {
                self.lines.push(Row::new(new_cols));
            }
        } else if new_rows < self.rows {
            // Push truncated lines to scrollback if in primary screen
            if self.alt_lines.is_none() {
                let to_remove = self.rows - new_rows;
                for _ in 0..to_remove {
                    let removed = self.lines.remove(0);
                    self.push_scrollback(removed);
                }
            } else {
                self.lines.truncate(new_rows);
            }
        }

        if let Some(alt) = &mut self.alt_lines {
            for row in alt.iter_mut() {
                row.resize(new_cols);
            }
            if new_rows > self.rows {
                for _ in self.rows..new_rows {
                    alt.push(Row::new(new_cols));
                }
            } else {
                alt.truncate(new_rows);
            }
        }

        self.cols = new_cols;
        self.rows = new_rows;
        self.scroll_region_top = 0;
        self.scroll_region_bottom = new_rows.saturating_sub(1);
        self.cursor.row = self.cursor.row.min(new_rows.saturating_sub(1));
        self.cursor.col = self.cursor.col.min(new_cols.saturating_sub(1));
    }

    fn push_scrollback(&mut self, row: Row) {
        if self.max_scrollback == 0 {
            return;
        }
        if self.scrollback.len() >= self.max_scrollback {
            self.scrollback.pop_front();
        }
        self.scrollback.push_back(row);
        // If user is currently viewing history, keep the view anchored on the same lines
        if self.viewport_offset > 0 {
            self.viewport_offset = (self.viewport_offset + 1).min(self.scrollback.len());
        }
    }

    /// Scrolls the viewport up by `delta` lines to view older history.
    pub fn scroll_viewport_up(&mut self, delta: usize) {
        if self.is_alt_screen() {
            return;
        }
        let max_offset = self.scrollback.len();
        self.viewport_offset = self.viewport_offset.saturating_add(delta).min(max_offset);
    }

    /// Scrolls the viewport down by `delta` lines toward the active screen.
    pub fn scroll_viewport_down(&mut self, delta: usize) {
        if self.is_alt_screen() {
            return;
        }
        self.viewport_offset = self.viewport_offset.saturating_sub(delta);
    }

    /// Jumps the viewport to the earliest line in the scrollback history.
    pub fn scroll_viewport_top(&mut self) {
        if self.is_alt_screen() {
            return;
        }
        self.viewport_offset = self.scrollback.len();
    }

    /// Resets the viewport offset to 0 (bottom of active screen).
    pub fn scroll_viewport_bottom(&mut self) {
        self.viewport_offset = 0;
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
        let count = count.min(self.scroll_region_bottom - self.scroll_region_top + 1);
        for _ in 0..count {
            let removed = self.lines.remove(self.scroll_region_top);
            if self.scroll_region_top == 0
                && self.scroll_region_bottom == self.rows.saturating_sub(1)
                && self.alt_lines.is_none()
            {
                self.push_scrollback(removed);
            }
            self.lines
                .insert(self.scroll_region_bottom, Row::new(self.cols));
        }
    }

    /// Scrolls lines inside the active scroll region downward.
    pub fn scroll_down(&mut self, count: usize) {
        let count = count.min(self.scroll_region_bottom - self.scroll_region_top + 1);
        for _ in 0..count {
            self.lines.remove(self.scroll_region_bottom);
            self.lines
                .insert(self.scroll_region_top, Row::new(self.cols));
        }
    }

    /// Clears part or all of the active display screen.
    pub fn clear_screen(&mut self, mode: ClearMode) {
        match mode {
            ClearMode::Below => {
                if self.cursor.row < self.rows {
                    let col = self.cursor.col;
                    for cell in &mut self.lines[self.cursor.row].cells[col..] {
                        cell.reset();
                    }
                    for row in &mut self.lines[self.cursor.row + 1..] {
                        row.reset();
                    }
                }
            }
            ClearMode::Above => {
                if self.cursor.row < self.rows {
                    for row in &mut self.lines[..self.cursor.row] {
                        row.reset();
                    }
                    let col = (self.cursor.col + 1).min(self.cols);
                    for cell in &mut self.lines[self.cursor.row].cells[..col] {
                        cell.reset();
                    }
                }
            }
            ClearMode::All => {
                for row in &mut self.lines {
                    row.reset();
                }
            }
            ClearMode::Saved => {
                self.scrollback.clear();
                self.viewport_offset = 0;
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
                for cell in &mut row.cells[start..] {
                    cell.reset();
                }
            }
            ClearMode::Above => {
                let end = (self.cursor.col + 1).min(self.cols);
                for cell in &mut row.cells[..end] {
                    cell.reset();
                }
            }
            ClearMode::All | ClearMode::Saved => {
                row.reset();
            }
        }
    }

    /// Writes a character with the given styling attributes at the current cursor position.
    pub fn write_char(&mut self, c: char, fg: Color, bg: Color, flags: CellFlags) {
        let width = c.width().unwrap_or(1);
        if width == 0 {
            // Combining character: optionally attach to previous cell if possible
            if self.cursor.col > 0 && self.cursor.row < self.rows {
                // Keep base char for simplicity in minimal terminal
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
            flags: cell_flags,
        };

        if width == 2 && col + 1 < self.cols {
            self.lines[row].cells[col + 1] = Cell {
                c: ' ',
                fg,
                bg,
                flags: flags | CellFlags::WIDE_CHAR_SPACER,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
