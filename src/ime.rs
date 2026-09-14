//! Fcitx5 and Wayland `text-input-v3` input method state and cursor tracking.

use crate::font::CellMetrics;
use crate::grid::{CellFlags, Grid};

/// Active pre-edit text and cursor range from the input method.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Preedit {
    pub text: String,
    pub cursor_begin: i32,
    pub cursor_end: i32,
}

/// Double-buffered pending events for `text-input-v3` batches committed upon `Done`.
#[derive(Debug, Clone, Default)]
pub struct PendingImeEvents {
    pub delete_surrounding: Option<(u32, u32)>,
    pub commit_text: Option<String>,
    pub preedit: Option<Option<Preedit>>,
}

/// Tracks the active IME session state, pre-edit text, and double-buffered batches.
#[derive(Debug, Clone, Default)]
pub struct ImeState {
    pub active: bool,
    pub preedit: Option<Preedit>,
    pub pending: PendingImeEvents,
}

impl ImeState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stages a surrounding text deletion event into the pending batch.
    pub fn stage_delete(&mut self, before_length: u32, after_length: u32) {
        self.pending.delete_surrounding = Some((before_length, after_length));
    }

    /// Stages committed text into the pending batch.
    pub fn stage_commit(&mut self, text: Option<String>) {
        self.pending.commit_text = text;
    }

    /// Stages pre-edit string updates into the pending batch.
    pub fn stage_preedit(&mut self, text: Option<String>, cursor_begin: i32, cursor_end: i32) {
        match text {
            Some(t) if !t.is_empty() => {
                self.pending.preedit = Some(Some(Preedit {
                    text: t,
                    cursor_begin,
                    cursor_end,
                }));
            }
            _ => {
                self.pending.preedit = Some(None);
            }
        }
    }

    /// Atomically applies the pending batch upon `zwp_text_input_v3.done`.
    ///
    /// Returns `(delete_surrounding, commit_text)` ordered so deletion precedes commit.
    pub fn apply_done(&mut self) -> (Option<(u32, u32)>, Option<String>) {
        let delete = self.pending.delete_surrounding.take();
        let commit = self.pending.commit_text.take();

        if let Some(preedit_update) = self.pending.preedit.take() {
            self.preedit = preedit_update;
        } else if commit.is_some() {
            self.preedit = None;
        }

        (delete, commit)
    }

    /// Clears any active composition and pending batches upon focus loss or reset.
    pub fn clear(&mut self) {
        self.active = false;
        self.preedit = None;
        self.pending = PendingImeEvents::default();
    }
}

/// Computes the pixel-accurate bounding rectangle of the terminal cursor for IME popup positioning.
///
/// Returns `(x, y, width, height)` in surface-local pixels.
#[must_use]
pub fn calculate_cursor_rect(
    grid: &Grid,
    metrics: CellMetrics,
    padding: [u16; 2],
) -> (i32, i32, i32, i32) {
    let cw = metrics.cell_width as i32;
    let ch = metrics.cell_height as i32;
    let pad_x = i32::from(padding[0]);
    let pad_y = i32::from(padding[1]);

    let row = grid.cursor.row.min(grid.rows.saturating_sub(1));
    let mut col = grid.cursor.col.min(grid.cols.saturating_sub(1));

    // If placed on a wide character spacer, anchor to the leading wide character cell
    if row < grid.lines.len() {
        let line = &grid.lines[row];
        if col > 0
            && col < line.cells.len()
            && line.cells[col].flags.contains(CellFlags::WIDE_CHAR_SPACER)
        {
            col -= 1;
        }
    }

    let is_wide = if row < grid.lines.len() {
        let line = &grid.lines[row];
        if col < line.cells.len() {
            line.cells[col].flags.contains(CellFlags::WIDE_CHAR)
        } else {
            false
        }
    } else {
        false
    };

    let width = if is_wide {
        2.min(grid.cols.saturating_sub(col)) as i32 * cw
    } else {
        cw
    };
    let x = pad_x + (col as i32) * cw;
    let y = pad_y + (row as i32) * ch;

    (x, y, width, ch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ime_batch_application_order() {
        let mut ime = ImeState::new();

        // Stage delete, commit, and preedit out of order
        ime.stage_commit(Some("你好".to_string()));
        ime.stage_delete(2, 0);
        ime.stage_preedit(Some("test".to_string()), 0, 4);

        let (delete, commit) = ime.apply_done();
        assert_eq!(delete, Some((2, 0)));
        assert_eq!(commit, Some("你好".to_string()));
        assert_eq!(
            ime.preedit,
            Some(Preedit {
                text: "test".to_string(),
                cursor_begin: 0,
                cursor_end: 4,
            })
        );

        // Subsequent commit without preedit update clears preedit
        ime.stage_commit(Some("世界".to_string()));
        let (_, commit) = ime.apply_done();
        assert_eq!(commit, Some("世界".to_string()));
        assert!(ime.preedit.is_none());
    }

    #[test]
    fn test_calculate_cursor_rect_with_padding() {
        let mut grid = Grid::new(80, 24, 0);
        grid.cursor.row = 5;
        grid.cursor.col = 10;

        let metrics = CellMetrics {
            cell_width: 10,
            cell_height: 20,
            ascent: 15,
        };

        // Without padding
        let (x, y, w, h) = calculate_cursor_rect(&grid, metrics, [0, 0]);
        assert_eq!((x, y, w, h), (100, 100, 10, 20));

        // With padding [15, 25]
        let (x, y, w, h) = calculate_cursor_rect(&grid, metrics, [15, 25]);
        assert_eq!((x, y, w, h), (115, 125, 10, 20));
    }

    #[test]
    fn test_calculate_cursor_rect_wide_char_and_spacer() {
        let mut grid = Grid::new(80, 24, 0);
        grid.cursor.row = 2;
        grid.cursor.col = 4;
        grid.lines[2].cells[4].flags = CellFlags::WIDE_CHAR;
        grid.lines[2].cells[5].flags = CellFlags::WIDE_CHAR_SPACER;

        let metrics = CellMetrics {
            cell_width: 9,
            cell_height: 18,
            ascent: 14,
        };

        // Directly on leading wide char
        let (x, y, w, h) = calculate_cursor_rect(&grid, metrics, [5, 5]);
        assert_eq!(x, 5 + 4 * 9);
        assert_eq!(y, 5 + 2 * 18);
        assert_eq!(w, 18);
        assert_eq!(h, 18);

        // Cursor positioned on the spacer cell (index 5) must anchor back to index 4
        grid.cursor.col = 5;
        let (sx, sy, sw, sh) = calculate_cursor_rect(&grid, metrics, [5, 5]);
        assert_eq!(sx, 5 + 4 * 9);
        assert_eq!(sy, 5 + 2 * 18);
        assert_eq!(sw, 18);
        assert_eq!(sh, 18);
    }

    #[test]
    fn test_calculate_cursor_rect_clamping() {
        let mut grid = Grid::new(80, 24, 0);
        grid.cursor.row = 999;
        grid.cursor.col = 999;

        let metrics = CellMetrics {
            cell_width: 10,
            cell_height: 20,
            ascent: 15,
        };

        let (x, y, w, h) = calculate_cursor_rect(&grid, metrics, [0, 0]);
        // Should clamp to (79, 23)
        assert_eq!(x, 79 * 10);
        assert_eq!(y, 23 * 20);
        assert_eq!(w, 10);
        assert_eq!(h, 20);
    }
}
