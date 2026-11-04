//! Terminal cell grid, cursor management, and scrollback ring buffer.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::num::NonZeroU32;

use crate::color::Color;
use crate::kitty::{DeleteTarget, ImageData, ImagePlacement};
use crate::width::{
    AMBIGUOUS_NARROW, AMBIGUOUS_WIDE, continues_grapheme, grapheme_display_width, wcwidth,
};

// --- Cell & Row ---

pub(crate) use std::cell::Cell as DirtyCell;

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

/// Sparse map of column indexes to (image_row, image_col, diacritic_count) coordinates.
pub type PlaceholderMap = HashMap<usize, (u16, u16, u8)>;

/// A horizontal row of cells in the terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub placeholders: Option<Box<PlaceholderMap>>,
    /// Full grapheme-cluster text for cells that contain more than one code point.
    pub clusters: Option<Box<HashMap<usize, String>>>,
    pub wrapped: bool,
    pub dirty: DirtyCell<bool>,
}

impl Row {
    #[must_use]
    pub fn new(cols: usize) -> Self {
        Self {
            cells: vec![Cell::default(); cols],
            placeholders: None,
            clusters: None,
            wrapped: false,
            dirty: DirtyCell::new(true),
        }
    }

    pub fn resize(&mut self, new_cols: usize) {
        if new_cols < self.cells.len() {
            // Prevent splitting a wide character across the truncation boundary.
            if new_cols > 0 {
                self.sever_wide_at(new_cols - 1);
            }
            self.cells.truncate(new_cols);
            if let Some(coords) = &mut self.placeholders {
                coords.retain(|&col, _| col < new_cols);
                if coords.is_empty() {
                    self.placeholders = None;
                }
            }
            if let Some(clusters) = &mut self.clusters {
                clusters.retain(|&col, _| col < new_cols);
                if clusters.is_empty() {
                    self.clusters = None;
                }
            }
            self.dirty.set(true);
        } else if new_cols > self.cells.len() {
            self.cells.resize(new_cols, Cell::default());
            self.dirty.set(true);
        }
    }

    #[must_use]
    pub fn blank(cols: usize, bg: Color) -> Self {
        Self {
            cells: vec![Cell::blank(bg); cols],
            placeholders: None,
            clusters: None,
            wrapped: false,
            dirty: DirtyCell::new(true),
        }
    }

    pub fn reset(&mut self) {
        self.reset_with_bg(Color::DefaultBackground);
    }

    pub fn reset_with_bg(&mut self, bg: Color) {
        self.cells.fill(Cell::blank(bg));
        self.placeholders = None;
        self.clusters = None;
        self.wrapped = false;
        self.dirty.set(true);
    }

    /// Decouples any wide character relationship crossing `col`.
    ///
    /// Following Contour's multi-cell overwriting design: when either half of a wide
    /// character is touched, its partner is cleanly decoupled to an ordinary blank space
    /// while strictly preserving its existing background color and attributes (no default
    /// background destruction).
    pub fn sever_wide_at(&mut self, col: usize) {
        if col >= self.cells.len() {
            return;
        }
        let flags = self.cells[col].flags;
        if flags.contains(CellFlags::WIDE_CHAR) {
            self.cells[col].flags.remove(CellFlags::WIDE_CHAR);
            self.cells[col].c = ' ';
            self.clear_cluster(col);
            if col + 1 < self.cells.len() {
                self.cells[col + 1]
                    .flags
                    .remove(CellFlags::WIDE_CHAR_SPACER);
                self.cells[col + 1].c = ' ';
                self.clear_cluster(col + 1);
            }
            self.dirty.set(true);
        } else if flags.contains(CellFlags::WIDE_CHAR_SPACER) {
            self.cells[col].flags.remove(CellFlags::WIDE_CHAR_SPACER);
            self.cells[col].c = ' ';
            self.clear_cluster(col);
            if col > 0 {
                self.cells[col - 1].flags.remove(CellFlags::WIDE_CHAR);
                self.cells[col - 1].c = ' ';
                self.clear_cluster(col - 1);
            }
            self.dirty.set(true);
        }
    }

    /// Returns the full grapheme-cluster text for `col` when it spans multiple code points.
    #[must_use]
    pub fn cluster_text(&self, col: usize) -> Option<&str> {
        self.clusters
            .as_ref()
            .and_then(|clusters| clusters.get(&col))
            .map(String::as_str)
    }

    /// Appends a code point to the cluster rooted at `col`, creating storage if needed.
    pub fn append_cluster_char(&mut self, col: usize, c: char) {
        if col >= self.cells.len() {
            return;
        }
        let clusters = self
            .clusters
            .get_or_insert_with(|| Box::new(HashMap::new()));
        let text = clusters
            .entry(col)
            .or_insert_with(|| self.cells[col].c.to_string());
        text.push(c);
        self.dirty.set(true);
    }

    /// Stores complete cluster text for a cell (used by reflow).
    pub fn set_cluster_text(&mut self, col: usize, text: String) {
        if col >= self.cells.len() || text.chars().count() <= 1 {
            return;
        }
        self.clusters
            .get_or_insert_with(|| Box::new(HashMap::new()))
            .insert(col, text);
        self.dirty.set(true);
    }

    pub fn clear_cluster(&mut self, col: usize) {
        if let Some(clusters) = &mut self.clusters {
            clusters.remove(&col);
            if clusters.is_empty() {
                self.clusters = None;
            }
        }
    }

    pub fn clear_clusters_range(&mut self, start: usize, end: usize) {
        if let Some(clusters) = &mut self.clusters {
            clusters.retain(|&col, _| col < start || col >= end);
            if clusters.is_empty() {
                self.clusters = None;
            }
        }
    }

    fn shift_clusters(&mut self, start: usize, end: usize, delta: isize) {
        let Some(clusters) = &mut self.clusters else {
            return;
        };
        let mut shifted = HashMap::with_capacity(clusters.len());
        for (&col, text) in clusters.iter() {
            let target = if col >= start && col < end {
                if delta >= 0 {
                    col + delta as usize
                } else {
                    match col.checked_sub(delta.unsigned_abs()) {
                        Some(target) => target,
                        None => continue,
                    }
                }
            } else {
                col
            };
            if target < self.cells.len() {
                shifted.insert(target, text.clone());
            }
        }
        if shifted.is_empty() {
            self.clusters = None;
        } else {
            self.clusters = Some(Box::new(shifted));
        }
        self.dirty.set(true);
    }

    pub fn shift_clusters_right(&mut self, start: usize, count: usize) {
        self.shift_clusters(start, self.cells.len(), count as isize);
    }

    pub fn shift_clusters_left(&mut self, start: usize, count: usize) {
        self.shift_clusters(start, self.cells.len(), -(count as isize));
    }
}

// --- Diacritics ---

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
    DIACRITICS.binary_search(&c).ok().map(|p| p as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diacritic_to_index_binary_search() {
        assert_eq!(diacritic_to_index('\u{305}'), Some(0));
        assert_eq!(diacritic_to_index('\u{1D244}'), Some(296));
        assert_eq!(diacritic_to_index('\u{483}'), Some(30));
        assert_eq!(diacritic_to_index('A'), None);
        assert_eq!(diacritic_to_index(KITTY_PLACEHOLDER), None);
    }
}

// --- Grid Core ---

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

// --- Resize ---

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

pub mod diacritics {
    pub use super::{KITTY_PLACEHOLDER, diacritic_to_index};
}

pub mod row {
    pub use super::{Cell, CellFlags, ClearMode, Cursor, CursorShape, PlaceholderMap, Row};
}

pub mod resize {
    pub use super::reflow::*;
}
