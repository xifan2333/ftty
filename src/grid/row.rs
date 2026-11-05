//! One horizontal row of cells, including wide-character and placeholder bookkeeping.

use std::collections::HashMap;

use super::{Cell, CellFlags, DirtyCell};
use crate::color::Color;

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
