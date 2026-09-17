//! Terminal cell grid, cursor management, and scrollback ring buffer.

use std::cell::Cell as DirtyCell;
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

pub const KITTY_PLACEHOLDER: char = '\u{10EEEE}';

/// Maps a Kitty Unicode placeholder diacritic combining character to its index.
#[must_use]
pub fn diacritic_to_index(c: char) -> Option<u16> {
    const DIACRITICS: [char; 297] = [
        '\u{305}',
        '\u{30D}',
        '\u{30E}',
        '\u{310}',
        '\u{312}',
        '\u{33D}',
        '\u{33E}',
        '\u{33F}',
        '\u{346}',
        '\u{34A}',
        '\u{34B}',
        '\u{34C}',
        '\u{350}',
        '\u{351}',
        '\u{352}',
        '\u{357}',
        '\u{35B}',
        '\u{363}',
        '\u{364}',
        '\u{365}',
        '\u{366}',
        '\u{367}',
        '\u{368}',
        '\u{369}',
        '\u{36A}',
        '\u{36B}',
        '\u{36C}',
        '\u{36D}',
        '\u{36E}',
        '\u{36F}',
        '\u{483}',
        '\u{484}',
        '\u{485}',
        '\u{486}',
        '\u{487}',
        '\u{592}',
        '\u{593}',
        '\u{594}',
        '\u{595}',
        '\u{597}',
        '\u{598}',
        '\u{599}',
        '\u{59C}',
        '\u{59D}',
        '\u{59E}',
        '\u{59F}',
        '\u{5A0}',
        '\u{5A1}',
        '\u{5A8}',
        '\u{5A9}',
        '\u{5AB}',
        '\u{5AC}',
        '\u{5AF}',
        '\u{5C4}',
        '\u{610}',
        '\u{611}',
        '\u{612}',
        '\u{613}',
        '\u{614}',
        '\u{615}',
        '\u{616}',
        '\u{617}',
        '\u{657}',
        '\u{658}',
        '\u{659}',
        '\u{65A}',
        '\u{65B}',
        '\u{65D}',
        '\u{65E}',
        '\u{6D6}',
        '\u{6D7}',
        '\u{6D8}',
        '\u{6D9}',
        '\u{6DA}',
        '\u{6DB}',
        '\u{6DC}',
        '\u{6DF}',
        '\u{6E0}',
        '\u{6E1}',
        '\u{6E2}',
        '\u{6E4}',
        '\u{6E7}',
        '\u{6E8}',
        '\u{6EB}',
        '\u{6EC}',
        '\u{730}',
        '\u{732}',
        '\u{733}',
        '\u{735}',
        '\u{736}',
        '\u{73A}',
        '\u{73D}',
        '\u{73F}',
        '\u{740}',
        '\u{741}',
        '\u{743}',
        '\u{745}',
        '\u{747}',
        '\u{749}',
        '\u{74A}',
        '\u{7EB}',
        '\u{7EC}',
        '\u{7ED}',
        '\u{7EE}',
        '\u{7EF}',
        '\u{7F0}',
        '\u{7F1}',
        '\u{7F3}',
        '\u{816}',
        '\u{817}',
        '\u{818}',
        '\u{819}',
        '\u{81B}',
        '\u{81C}',
        '\u{81D}',
        '\u{81E}',
        '\u{81F}',
        '\u{820}',
        '\u{821}',
        '\u{822}',
        '\u{823}',
        '\u{825}',
        '\u{826}',
        '\u{827}',
        '\u{829}',
        '\u{82A}',
        '\u{82B}',
        '\u{82C}',
        '\u{82D}',
        '\u{951}',
        '\u{953}',
        '\u{954}',
        '\u{F82}',
        '\u{F83}',
        '\u{F86}',
        '\u{F87}',
        '\u{135D}',
        '\u{135E}',
        '\u{135F}',
        '\u{17DD}',
        '\u{193A}',
        '\u{1A17}',
        '\u{1A75}',
        '\u{1A76}',
        '\u{1A77}',
        '\u{1A78}',
        '\u{1A79}',
        '\u{1A7A}',
        '\u{1A7B}',
        '\u{1A7C}',
        '\u{1B6B}',
        '\u{1B6D}',
        '\u{1B6E}',
        '\u{1B6F}',
        '\u{1B70}',
        '\u{1B71}',
        '\u{1B72}',
        '\u{1B73}',
        '\u{1CD0}',
        '\u{1CD1}',
        '\u{1CD2}',
        '\u{1CDA}',
        '\u{1CDB}',
        '\u{1CE0}',
        '\u{1DC0}',
        '\u{1DC1}',
        '\u{1DC3}',
        '\u{1DC4}',
        '\u{1DC5}',
        '\u{1DC6}',
        '\u{1DC7}',
        '\u{1DC8}',
        '\u{1DC9}',
        '\u{1DCB}',
        '\u{1DCC}',
        '\u{1DD1}',
        '\u{1DD2}',
        '\u{1DD3}',
        '\u{1DD4}',
        '\u{1DD5}',
        '\u{1DD6}',
        '\u{1DD7}',
        '\u{1DD8}',
        '\u{1DD9}',
        '\u{1DDA}',
        '\u{1DDB}',
        '\u{1DDC}',
        '\u{1DDD}',
        '\u{1DDE}',
        '\u{1DDF}',
        '\u{1DE0}',
        '\u{1DE1}',
        '\u{1DE2}',
        '\u{1DE3}',
        '\u{1DE4}',
        '\u{1DE5}',
        '\u{1DE6}',
        '\u{1DFE}',
        '\u{20D0}',
        '\u{20D1}',
        '\u{20D4}',
        '\u{20D5}',
        '\u{20D6}',
        '\u{20D7}',
        '\u{20DB}',
        '\u{20DC}',
        '\u{20E1}',
        '\u{20E7}',
        '\u{20E9}',
        '\u{20F0}',
        '\u{2CEF}',
        '\u{2CF0}',
        '\u{2CF1}',
        '\u{2DE0}',
        '\u{2DE1}',
        '\u{2DE2}',
        '\u{2DE3}',
        '\u{2DE4}',
        '\u{2DE5}',
        '\u{2DE6}',
        '\u{2DE7}',
        '\u{2DE8}',
        '\u{2DE9}',
        '\u{2DEA}',
        '\u{2DEB}',
        '\u{2DEC}',
        '\u{2DED}',
        '\u{2DEE}',
        '\u{2DEF}',
        '\u{2DF0}',
        '\u{2DF1}',
        '\u{2DF2}',
        '\u{2DF3}',
        '\u{2DF4}',
        '\u{2DF5}',
        '\u{2DF6}',
        '\u{2DF7}',
        '\u{2DF8}',
        '\u{2DF9}',
        '\u{2DFA}',
        '\u{2DFB}',
        '\u{2DFC}',
        '\u{2DFD}',
        '\u{2DFE}',
        '\u{2DFF}',
        '\u{A66F}',
        '\u{A67C}',
        '\u{A67D}',
        '\u{A6F0}',
        '\u{A6F1}',
        '\u{A8E0}',
        '\u{A8E1}',
        '\u{A8E2}',
        '\u{A8E3}',
        '\u{A8E4}',
        '\u{A8E5}',
        '\u{A8E6}',
        '\u{A8E7}',
        '\u{A8E8}',
        '\u{A8E9}',
        '\u{A8EA}',
        '\u{A8EB}',
        '\u{A8EC}',
        '\u{A8ED}',
        '\u{A8EE}',
        '\u{A8EF}',
        '\u{A8F0}',
        '\u{A8F1}',
        '\u{AAB0}',
        '\u{AAB2}',
        '\u{AAB3}',
        '\u{AAB7}',
        '\u{AAB8}',
        '\u{AABE}',
        '\u{AABF}',
        '\u{AAC1}',
        '\u{FE20}',
        '\u{FE21}',
        '\u{FE22}',
        '\u{FE23}',
        '\u{FE24}',
        '\u{FE25}',
        '\u{FE26}',
        '\u{10A0F}',
        '\u{10A38}',
        '\u{1D185}',
        '\u{1D186}',
        '\u{1D187}',
        '\u{1D188}',
        '\u{1D189}',
        '\u{1D1AA}',
        '\u{1D1AB}',
        '\u{1D1AC}',
        '\u{1D1AD}',
        '\u{1D242}',
        '\u{1D243}',
        '\u{1D244}',
    ];
    DIACRITICS.iter().position(|&d| d == c).map(|p| p as u16)
}

const MAX_ROW_OVERFLOW: usize = 256;

/// A horizontal row of cells in the terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
    pub placeholders: Option<HashMap<usize, (u16, u16, u8)>>,
    pub overflow: Vec<Cell>,
    pub overflow_placeholders: Vec<(usize, (u16, u16, u8))>,
    pub dirty: DirtyCell<bool>,
}

impl Row {
    #[must_use]
    pub fn new(cols: usize) -> Self {
        Self {
            cells: vec![Cell::default(); cols],
            wrapped: false,
            placeholders: None,
            overflow: Vec::new(),
            overflow_placeholders: Vec::new(),
            dirty: DirtyCell::new(true),
        }
    }

    pub fn resize(&mut self, new_cols: usize) {
        if new_cols < self.cells.len() {
            let current_len = self.cells.len();
            let excess_len = current_len - new_cols;

            let mut newly_overflowed_ph = Vec::new();
            if let Some(coords) = &mut self.placeholders {
                coords.retain(|&col, &mut data| {
                    if col >= new_cols {
                        newly_overflowed_ph.push((col - new_cols, data));
                        false
                    } else {
                        true
                    }
                });
            }

            for (offset, _) in &mut self.overflow_placeholders {
                *offset = offset.saturating_add(excess_len);
            }
            newly_overflowed_ph.append(&mut self.overflow_placeholders);
            self.overflow_placeholders = newly_overflowed_ph;

            let excess: Vec<Cell> = self.cells.drain(new_cols..).collect();
            let mut new_overflow = excess;
            new_overflow.append(&mut self.overflow);
            self.overflow = new_overflow;

            if self.overflow.len() > MAX_ROW_OVERFLOW {
                self.overflow.truncate(MAX_ROW_OVERFLOW);
                self.overflow_placeholders
                    .retain(|&(offset, _)| offset < MAX_ROW_OVERFLOW);
            }
            self.dirty.set(true);
        } else if new_cols > self.cells.len() {
            let current_len = self.cells.len();
            let needed = new_cols - current_len;
            let from_overflow = needed.min(self.overflow.len());

            for cell in self.overflow.drain(0..from_overflow) {
                self.cells.push(cell);
            }

            let mut remaining_ph = Vec::new();
            for (offset, data) in self.overflow_placeholders.drain(..) {
                if offset < from_overflow {
                    let col = current_len + offset;
                    self.placeholders
                        .get_or_insert_with(HashMap::new)
                        .insert(col, data);
                } else {
                    remaining_ph.push((offset - from_overflow, data));
                }
            }
            self.overflow_placeholders = remaining_ph;

            if self.cells.len() < new_cols {
                self.cells.resize(new_cols, Cell::default());
            }
            self.dirty.set(true);
        }
    }

    pub fn reset(&mut self) {
        for cell in &mut self.cells {
            cell.reset();
        }
        self.wrapped = false;
        self.placeholders = None;
        self.overflow.clear();
        self.overflow_placeholders.clear();
        self.dirty.set(true);
    }
}

/// Drops `to_remove` rows from `lines`, discarding rows below `cursor_row` first so the
/// cursor's line survives. Returns the rows removed from the top, in order.
fn shrink_rows(
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
fn shift_placements(placements: &mut Vec<ImagePlacement>, amount: usize) {
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
    alt_lines: Option<Vec<Row>>,
    alt_cursor: Option<Cursor>,
    alt_saved_cursor: Option<Cursor>,
    alt_placements: Vec<ImagePlacement>,
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

    /// Adds a decoded image to the grid's image store and bumps its version.
    pub fn add_image(&mut self, image: ImageData) {
        let id = image.id;
        self.images.insert(id, image);
        let ver = self.image_versions.entry(id).or_insert(0);
        *ver = ver.wrapping_add(1);

        // Cap stored images to 256 by removing unplaced images
        if self.images.len() > 256 {
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
        }
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
        for row in &self.scrollback {
            row.dirty.set(true);
        }
    }

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

            // The alternate screen keeps no history, so only the primary screen with scrollback
            // enabled preserves the absolute line of its surviving rows.
            let active_on_alt = self.alt_lines.is_some();
            let active_retains = !active_on_alt && self.max_scrollback > 0;

            // Active screen: trim below the cursor first so its line is always retained.
            let removed_top = shrink_rows(&mut self.lines, to_remove, self.cursor.row, new_rows);
            let from_top = removed_top.len();
            if !active_on_alt {
                for row in removed_top {
                    self.push_scrollback(row);
                }
            }
            self.cursor.row = self.cursor.row.saturating_sub(from_top);
            self.saved_cursor.row = self.saved_cursor.row.saturating_sub(from_top);
            if !active_retains {
                shift_placements(&mut self.placements, from_top);
            }

            // Hidden primary screen while the alternate screen is active. Its rows are dropped
            // rather than saved, so its own placements shift by its own removal count.
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

    fn push_scrollback(&mut self, row: Row) {
        if self.max_scrollback == 0 {
            return;
        }
        if self.scrollback.len() >= self.max_scrollback {
            self.scrollback.pop_front();
            self.placements.retain_mut(|p| {
                if p.line == 0 {
                    false
                } else {
                    p.line -= 1;
                    true
                }
            });
        }
        self.scrollback.push_back(row);
        // If user is currently viewing history, keep the view anchored on the same lines
        if self.viewport_offset > 0 {
            self.viewport_offset = (self.viewport_offset + 1).min(self.scrollback.len());
        }
        self.mark_all_dirty();
    }

    /// Scrolls the viewport up by `delta` lines to view older history.
    pub fn scroll_viewport_up(&mut self, delta: usize) {
        if self.is_alt_screen() {
            return;
        }
        let max_offset = self.scrollback.len();
        self.viewport_offset = self.viewport_offset.saturating_add(delta).min(max_offset);
        self.mark_all_dirty();
    }

    /// Scrolls the viewport down by `delta` lines toward the active screen.
    pub fn scroll_viewport_down(&mut self, delta: usize) {
        if self.is_alt_screen() {
            return;
        }
        self.viewport_offset = self.viewport_offset.saturating_sub(delta);
        self.mark_all_dirty();
    }

    /// Jumps the viewport to the earliest line in the scrollback history.
    pub fn scroll_viewport_top(&mut self) {
        if self.is_alt_screen() {
            return;
        }
        self.viewport_offset = self.scrollback.len();
        self.mark_all_dirty();
    }

    /// Resets the viewport offset to 0 (bottom of active screen).
    pub fn scroll_viewport_bottom(&mut self) {
        self.viewport_offset = 0;
        self.mark_all_dirty();
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

        if self.scroll_region_top == 0
            && self.scroll_region_bottom == self.rows.saturating_sub(1)
            && self.alt_lines.is_none()
        {
            for i in 0..count {
                self.push_scrollback(self.lines[i].clone());
            }
        }

        self.lines[self.scroll_region_top..=self.scroll_region_bottom].rotate_left(count);
        for row in
            &mut self.lines[self.scroll_region_bottom + 1 - count..=self.scroll_region_bottom]
        {
            row.reset();
        }
        self.mark_all_dirty();
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
        self.mark_all_dirty();
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
                    self.lines[self.cursor.row].dirty.set(true);
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
        row.dirty.set(true);
    }

    /// Writes a character with the given styling attributes at the current cursor position.
    pub fn write_char(&mut self, c: char, fg: Color, bg: Color, flags: CellFlags) {
        self.write_char_styled(c, fg, bg, flags, Color::DefaultForeground, None);
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
        let width = c.width().unwrap_or(1);
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
            hyperlink_id,
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
                hyperlink_id,
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
        grid.virtual_placements.insert(100, (10, 10));
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
                rgba: vec![0, 0, 0, 0],
            });
        }

        assert!(
            grid.images.contains_key(&100),
            "virtual image 100 must be preserved across eviction"
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
        });

        grid.resize(10, 2);

        assert_eq!(grid.cursor.row, 1);
        assert!(
            grid.placements.is_empty(),
            "a placement anchored below the new bottom row is evicted"
        );
    }

    #[test]
    fn shrink_on_alternate_screen_shifts_placements() {
        let mut grid = Grid::new(10, 4, 100);
        grid.enter_alt_screen();
        fill_rows(&mut grid, 4);
        grid.cursor.row = 3;

        // The alternate screen has no history, so its content shifts up on shrink.
        grid.add_placement(ImagePlacement {
            image_id: 1,
            placement_id: 0,
            line: grid.scrollback.len() + 3,
            col: 0,
            cols: 1,
            rows: 1,
            offset_x: 0,
            offset_y: 0,
            z_index: 0,
        });

        grid.resize(10, 2);

        assert_eq!(
            grid.placements.len(),
            1,
            "the placement must not be dropped"
        );
        assert_eq!(
            grid.placements[0].line,
            grid.scrollback.len() + 1,
            "it follows its row up the alternate screen"
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
        });
        assert_eq!(grid.placements.len(), 1);

        grid.enter_alt_screen();
        assert!(
            grid.placements.is_empty(),
            "primary placements must not bleed onto the alternate screen"
        );

        grid.exit_alt_screen();
        assert_eq!(grid.placements.len(), 1);
        assert_eq!(grid.placements[0].line, 2);
    }

    #[test]
    fn hidden_primary_images_survive_cache_eviction() {
        let mut grid = Grid::new(10, 4, 100);
        let image = ImageData {
            id: 42,
            width: 1,
            height: 1,
            rgba: vec![0, 0, 0, 0],
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
        });

        // The primary's placement is parked while the alternate screen churns the cache.
        grid.enter_alt_screen();
        for id in 1000..1300 {
            grid.add_image(ImageData {
                id,
                width: 1,
                height: 1,
                rgba: vec![0, 0, 0, 0],
            });
        }
        grid.exit_alt_screen();

        assert!(
            grid.images.contains_key(&42),
            "an image referenced by the hidden primary must not be evicted"
        );
        assert_eq!(grid.placements.len(), 1);
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
        });

        grid.enter_alt_screen();
        grid.clear_screen(ClearMode::Saved);
        grid.exit_alt_screen();

        assert_eq!(grid.scrollback.len(), 0);
        assert_eq!(
            grid.placements[0].line, 1,
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
    fn test_horizontal_shrink_and_grow_preserves_overflow_cells() {
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

        // Shrink horizontally to 5 columns
        grid.resize(5, 2);
        assert_eq!(grid.cols, 5);
        assert_eq!(grid.visible_line(0).cells[0].c, 'H');
        assert_eq!(grid.visible_line(0).cells[4].c, 'o');

        // Grow horizontally back to 20 columns: overflow cells restored!
        grid.resize(20, 2);
        assert_eq!(grid.cols, 20);
        let restored: String = grid.visible_line(0).cells[..17]
            .iter()
            .map(|c| c.c)
            .collect();
        assert_eq!(restored, "Hello World 12345");
    }

    #[test]
    fn test_shrink_and_grow_restores_placeholder_coordinates_and_bounds_overflow() {
        let mut row = Row::new(300);
        row.placeholders = Some(HashMap::from([(50, (1, 2, 3))]));

        // Shrink to 40 columns (excess = 260)
        row.resize(40);
        assert_eq!(row.cells.len(), 40);
        // Placeholder at 50 is beyond 40, so it's stashed in overflow_placeholders at offset 10
        assert!(row.placeholders.as_ref().unwrap().is_empty());
        assert_eq!(row.overflow.len(), MAX_ROW_OVERFLOW); // Bounded to 256!
        assert_eq!(row.overflow_placeholders, vec![(10, (1, 2, 3))]);

        // Grow back to 60 columns: restores 20 overflow cells
        row.resize(60);
        assert_eq!(row.cells.len(), 60);
        // Offset 10 is restored to column 40 + 10 = 50!
        assert_eq!(
            row.placeholders.as_ref().unwrap().get(&50),
            Some(&(1, 2, 3))
        );
    }
}
