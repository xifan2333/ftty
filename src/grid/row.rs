//! Terminal cell and row representation, cell flags, and cursor state.

pub(crate) use std::cell::Cell as DirtyCell;
use std::collections::HashMap;

use crate::color::Color;

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
        const UNDERLINE_DOUBLE = 1 << 9;
        const UNDERLINE_CURLY = 1 << 10;
        const UNDERLINE_DOTTED = 1 << 11;
        const UNDERLINE_DASHED = 1 << 12;
    }
}

impl CellFlags {
    pub const ALL_UNDERLINES: CellFlags = Self::UNDERLINE
        .union(Self::UNDERLINE_DOUBLE)
        .union(Self::UNDERLINE_CURLY)
        .union(Self::UNDERLINE_DOTTED)
        .union(Self::UNDERLINE_DASHED);
}

/// A single character cell within the terminal grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub underline_color: Color,
    pub flags: CellFlags,
    pub hyperlink_id: Option<u32>,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            c: ' ',
            fg: Color::DefaultForeground,
            bg: Color::DefaultBackground,
            underline_color: Color::DefaultForeground,
            flags: CellFlags::empty(),
            hyperlink_id: None,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
    pub placeholders: Option<HashMap<usize, (u16, u16, u8)>>,
    pub dirty: DirtyCell<bool>,
}

impl Row {
    #[must_use]
    pub fn new(cols: usize) -> Self {
        Self {
            cells: vec![Cell::default(); cols],
            wrapped: false,
            placeholders: None,
            dirty: DirtyCell::new(true),
        }
    }

    pub fn resize(&mut self, new_cols: usize) {
        if new_cols < self.cells.len() {
            self.cells.truncate(new_cols);
            if let Some(coords) = &mut self.placeholders {
                coords.retain(|&col, _| col < new_cols);
            }
            self.dirty.set(true);
        } else if new_cols > self.cells.len() {
            self.cells.resize(new_cols, Cell::default());
            self.dirty.set(true);
        }
    }

    pub fn reset(&mut self) {
        self.cells.fill(Cell::default());
        self.wrapped = false;
        if let Some(coords) = &mut self.placeholders {
            coords.clear();
        }
        self.dirty.set(true);
    }
}
