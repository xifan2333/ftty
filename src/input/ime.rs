//! `text-input-v3` session state, pre-edit staging, and PTY range-edit encoding.

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

/// Backspace escape emitted when erasing text before the cursor for an IME commit.
///
/// DEL (0x7f) matches what the Backspace key sends, which is the byte the kernel's default
/// `VERASE` expects; BS (0x08) would be kept as an ordinary byte by line-editing programs.
const IME_ERASE_BEFORE: &[u8] = b"\x7f";

/// Forward-delete escape emitted when erasing text after the cursor for an IME commit.
const IME_ERASE_AFTER: &[u8] = b"\x1b[3~";

/// Upper bound on characters erased per IME commit, so a hostile `DeleteSurroundingText` length
/// cannot force a multi-gigabyte allocation.
pub const MAX_IME_ERASE_CHARS: u32 = 4096;

/// Encodes an IME completion batch as the PTY input a shell expects: one backspace per erased
/// character before the cursor, one forward delete per erased character after, then the text.
#[must_use]
pub fn encode_ime_edits(delete: Option<(u32, u32)>, commit: Option<&str>) -> Vec<u8> {
    let (before, after) = delete.map_or((0, 0), |(before, after)| {
        (
            before.min(MAX_IME_ERASE_CHARS),
            after.min(MAX_IME_ERASE_CHARS),
        )
    });
    let commit_len = commit.map_or(0, str::len);
    let mut bytes = Vec::with_capacity(
        before as usize * IME_ERASE_BEFORE.len()
            + after as usize * IME_ERASE_AFTER.len()
            + commit_len,
    );
    for _ in 0..before {
        bytes.extend_from_slice(IME_ERASE_BEFORE);
    }
    for _ in 0..after {
        bytes.extend_from_slice(IME_ERASE_AFTER);
    }
    if let Some(text) = commit {
        bytes.extend_from_slice(text.as_bytes());
    }
    bytes
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
