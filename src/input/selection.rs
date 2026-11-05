//! Pointer selection state machine over grid coordinates.

use crate::grid::{CellFlags, Grid, Row};

/// A point in the terminal grid history or active screen buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SelectionPoint {
    pub line: usize,
    pub col: usize,
}

impl SelectionPoint {
    #[must_use]
    pub const fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

/// The mode of mouse selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectionType {
    #[default]
    Simple,
    Word,
    Line,
}

/// Represents an active or completed text selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub start: SelectionPoint,
    pub end: SelectionPoint,
    pub kind: SelectionType,
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            start: SelectionPoint::new(0, 0),
            end: SelectionPoint::new(0, 0),
            kind: SelectionType::Simple,
        }
    }
}

impl Selection {
    #[must_use]
    pub fn new(start: SelectionPoint, end: SelectionPoint, kind: SelectionType) -> Self {
        Self { start, end, kind }
    }

    /// Normalizes the selection range into `(start, end)` where `start <= end` in reading order.
    #[must_use]
    pub fn normalized(&self) -> (SelectionPoint, SelectionPoint) {
        if self.start <= self.end {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }

    /// Clears the selection by resetting start and end to (0, 0).
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Checks whether the selection spans zero characters.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.start == self.end && self.kind == SelectionType::Simple
    }

    /// Returns `true` if the cell at absolute line and column is contained within the selection.
    #[must_use]
    pub fn contains(&self, line: usize, col: usize) -> bool {
        if self.is_empty() {
            return false;
        }
        let (start, end) = self.normalized();
        let point = SelectionPoint::new(line, col);

        match self.kind {
            SelectionType::Simple | SelectionType::Word => point >= start && point <= end,
            SelectionType::Line => line >= start.line && line <= end.line,
        }
    }

    /// Returns true if this selection spans across the given line.
    #[must_use]
    pub fn spans_line(&self, line: usize) -> bool {
        if self.is_empty() {
            return false;
        }
        let (start, end) = self.normalized();
        line >= start.line && line <= end.line
    }

    /// Returns the column span `Some((start_col, end_col))` selected on the given line, if any.
    #[must_use]
    pub fn line_span(&self, line: usize, cols: usize) -> Option<(usize, usize)> {
        if self.is_empty() || cols == 0 {
            return None;
        }
        let (start, end) = self.normalized();
        if line < start.line || line > end.line {
            return None;
        }
        let max_col = cols.saturating_sub(1);
        let start_col = if line == start.line && self.kind != SelectionType::Line {
            if start.col > max_col {
                return None;
            }
            start.col
        } else {
            0
        };
        let end_col = if line == end.line && self.kind != SelectionType::Line {
            end.col.min(max_col)
        } else {
            max_col
        };
        if start_col <= end_col {
            Some((start_col, end_col))
        } else {
            None
        }
    }

    /// Extracts clean UTF-8 text from the grid within this selection range.
    ///
    /// Respects wrapped lines (omits newline) and trims trailing spaces from rows.
    #[must_use]
    pub fn extract_text(&self, grid: &Grid) -> String {
        if self.is_empty() {
            return String::new();
        }

        let (start, end) = self.normalized();
        let total_lines = grid.scrollback.len() + grid.lines.len();
        let mut result = String::new();
        let mut line_str = String::with_capacity(grid.cols);

        let get_row = |idx: usize| -> Option<&Row> {
            if idx < grid.scrollback.len() {
                grid.scrollback.get(idx)
            } else {
                grid.lines.get(idx - grid.scrollback.len())
            }
        };

        for line_idx in start.line..=end.line.min(total_lines.saturating_sub(1)) {
            let Some(row) = get_row(line_idx) else {
                break;
            };

            let start_col = if line_idx == start.line && self.kind != SelectionType::Line {
                start.col.min(row.cells.len())
            } else {
                0
            };

            let end_col = if line_idx == end.line && self.kind != SelectionType::Line {
                (end.col + 1).min(row.cells.len())
            } else {
                row.cells.len()
            };

            if start_col >= end_col {
                continue;
            }

            line_str.clear();
            for (offset, cell) in row.cells[start_col..end_col].iter().enumerate() {
                if !cell
                    .flags
                    .intersects(CellFlags::WIDE_CHAR_SPACER | CellFlags::WRAP_SPACER)
                {
                    if let Some(text) = row.cluster_text(start_col + offset) {
                        line_str.push_str(text);
                    } else {
                        line_str.push(cell.c);
                    }
                }
            }

            // Only trim trailing whitespace if this is the last line or an unwrapped line
            if line_idx == end.line || !row.wrapped {
                let trimmed_len = line_str.trim_end_matches(' ').len();
                line_str.truncate(trimmed_len);
            }

            result.push_str(&line_str);

            // Add newline if unwrapped and not the final line
            if !row.wrapped && line_idx < end.line {
                result.push('\n');
            }
        }

        result
    }
}

/// Identifies word boundaries around a given column index in a row.
#[must_use]
pub fn find_word_boundaries(row: &Row, col: usize) -> (usize, usize) {
    if row.cells.is_empty() {
        return (0, 0);
    }
    let col = col.min(row.cells.len().saturating_sub(1));
    let target_char = row.cells[col].c;

    let is_word_char = |c: char| c.is_alphanumeric() || c == '_';
    let target_is_word = is_word_char(target_char);

    // Expand left
    let mut start = col;
    while start > 0 {
        let prev = row.cells[start - 1].c;
        if is_word_char(prev) == target_is_word && !prev.is_whitespace() {
            start -= 1;
        } else {
            break;
        }
    }

    // Expand right
    let mut end = col;
    while end + 1 < row.cells.len() {
        let next = row.cells[end + 1].c;
        if is_word_char(next) == target_is_word && !next.is_whitespace() {
            end += 1;
        } else {
            break;
        }
    }

    (start, end)
}
