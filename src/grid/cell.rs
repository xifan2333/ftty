//! Cell attributes and the terminal cell payload.

use std::num::NonZeroU32;

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
        const WRAP_SPACER = 1 << 13;
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
    pub hyperlink_id: Option<NonZeroU32>,
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
    #[inline]
    #[must_use]
    pub fn blank(bg: Color) -> Self {
        Self {
            c: ' ',
            fg: Color::DefaultForeground,
            bg,
            underline_color: Color::DefaultForeground,
            flags: CellFlags::empty(),
            hyperlink_id: None,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    #[inline]
    pub fn reset_with_bg(&mut self, bg: Color) {
        *self = Self::blank(bg);
    }

    #[inline]
    #[must_use]
    pub fn hyperlink_id(&self) -> Option<u32> {
        self.hyperlink_id.map(NonZeroU32::get)
    }

    #[inline]
    pub fn set_hyperlink_id(&mut self, id: Option<u32>) {
        self.hyperlink_id = id.and_then(NonZeroU32::new);
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
