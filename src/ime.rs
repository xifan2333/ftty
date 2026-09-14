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

/// Tracks the active IME session state and pre-edit text.
#[derive(Debug, Clone, Default)]
pub struct ImeState {
    pub active: bool,
    pub preedit: Option<Preedit>,
}

impl ImeState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Updates or clears pre-edit text received from the compositor.
    pub fn set_preedit(&mut self, text: Option<String>, cursor_begin: i32, cursor_end: i32) {
        match text {
            Some(t) if !t.is_empty() => {
                self.preedit = Some(Preedit {
                    text: t,
                    cursor_begin,
                    cursor_end,
                });
            }
            _ => {
                self.preedit = None;
            }
        }
    }

    /// Clears any pending pre-edit text upon commit or focus loss.
    pub fn clear_preedit(&mut self) {
        self.preedit = None;
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
    let col = grid.cursor.col.min(grid.cols.saturating_sub(1));

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

    let width = if is_wide { cw * 2 } else { cw };
    let x = pad_x + (col as i32) * cw;
    let y = pad_y + (row as i32) * ch;

    (x, y, width, ch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ime_state_preedit_lifecycle() {
        let mut ime = ImeState::new();
        assert!(!ime.active);
        assert!(ime.preedit.is_none());

        ime.set_preedit(Some("nihao".to_string()), 0, 5);
        assert_eq!(
            ime.preedit,
            Some(Preedit {
                text: "nihao".to_string(),
                cursor_begin: 0,
                cursor_end: 5,
            })
        );

        ime.clear_preedit();
        assert!(ime.preedit.is_none());

        ime.set_preedit(Some(String::new()), 0, 0);
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
    fn test_calculate_cursor_rect_wide_char() {
        let mut grid = Grid::new(80, 24, 0);
        grid.cursor.row = 2;
        grid.cursor.col = 4;
        grid.lines[2].cells[4].flags = CellFlags::WIDE_CHAR;

        let metrics = CellMetrics {
            cell_width: 9,
            cell_height: 18,
            ascent: 14,
        };

        let (x, y, w, h) = calculate_cursor_rect(&grid, metrics, [5, 5]);
        assert_eq!(x, 5 + 4 * 9);
        assert_eq!(y, 5 + 2 * 18);
        assert_eq!(w, 18); // Wide char spans 2 cells
        assert_eq!(h, 18);
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
