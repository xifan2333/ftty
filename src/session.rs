//! Deep module orchestrating terminal emulation, PTY I/O, escape parsing, and protocol timers.

use std::io::{self, Read};
use std::os::fd::{AsFd, OwnedFd};
use std::time::{Duration, Instant};

use nix::errno::Errno;

use crate::kitty::{GraphicContext, KittyParser, KittyStreamChunk, collect_placeholder_ids};
use crate::parser::{Terminal, VtParser};
use crate::pty::Pty;

/// Maximum duration (150ms) before Mode 2026 synchronized output is automatically disabled.
pub const SYNC_OUTPUT_TIMEOUT: Duration = Duration::from_millis(150);

/// Maximum clipboard payload accepted for a single paste (10 MiB), bounding memory use.
pub const MAX_PASTE_BYTES: usize = 10 * 1024 * 1024;

/// Time budget for a single interactive input write before bytes are dropped.
const INPUT_WRITE_BUDGET: Duration = Duration::from_millis(100);

/// Time budget for draining a pasted payload to the PTY.
const PASTE_WRITE_BUDGET: Duration = Duration::from_secs(5);

/// Payloads at or below this size are written synchronously on the event loop.
const SYNC_PASTE_THRESHOLD: usize = 4096;

/// Poll readiness slice used while waiting for PTY write capacity.
const POLL_SLICE_MS: u16 = 20;

/// Bracketed paste mode (DEC 2004) opening marker.
const PASTE_OPEN: &[u8] = b"\x1b[200~";

/// Bracketed paste mode (DEC 2004) closing marker.
const PASTE_CLOSE: &[u8] = b"\x1b[201~";

/// Wraps `bytes` in bracketed-paste markers when `enabled`.
#[must_use]
pub fn wrap_bracketed_paste(bytes: &[u8], enabled: bool) -> Vec<u8> {
    if !enabled {
        return bytes.to_vec();
    }
    let mut wrapped = Vec::with_capacity(bytes.len() + PASTE_OPEN.len() + PASTE_CLOSE.len());
    wrapped.extend_from_slice(PASTE_OPEN);
    wrapped.extend_from_slice(bytes);
    wrapped.extend_from_slice(PASTE_CLOSE);
    wrapped
}

/// Writes `bytes` to a PTY master, retrying on readiness until `budget` is exhausted.
///
/// This is the single write path for every producer (keystrokes, protocol responses, pastes,
/// IME commits), so truncation and timeout behaviour cannot diverge between callers.
pub fn write_all_to_pty(fd: impl AsFd, mut bytes: &[u8], budget: Duration) {
    let start = Instant::now();
    while !bytes.is_empty() && start.elapsed() < budget {
        let borrowed = fd.as_fd();
        match nix::unistd::write(borrowed, bytes) {
            Ok(0) => break,
            Ok(written) => bytes = &bytes[written..],
            Err(Errno::EINTR) => continue,
            Err(Errno::EAGAIN) => {
                let mut fds = [nix::poll::PollFd::new(
                    borrowed,
                    nix::poll::PollFlags::POLLOUT,
                )];
                if !nix::poll::poll(&mut fds, POLL_SLICE_MS).is_ok_and(|ready| ready > 0) {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// Semantic actions and notifications emitted after processing terminal or user input.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SessionOutcome {
    /// Whether the terminal screen content or cursor changed and requires a redraw.
    pub needs_redraw: bool,
    /// Updated window title if set via OSC escape sequences.
    pub title_update: Option<String>,
    /// Clipboard update requested via OSC 52: `Some(Some(text))` for set, `Some(None)` for clear.
    pub clipboard_update: Option<Option<String>>,
    /// Whether the terminal color palette or default colors were modified.
    pub palette_changed: bool,
    /// Updated Kitty keyboard protocol flags (for keyboard input translation).
    pub kitty_keyboard_flags: u8,
    /// Whether the session process has terminated (EOF on PTY read).
    pub process_exited: bool,
}

/// The stateful orchestrator managing terminal emulation, PTY communication, and parser routing.
pub struct TerminalSession {
    pub terminal: Terminal,
    pub pty: Pty,
    vt_parser: VtParser,
    kitty_parser: KittyParser,
    sync_output_start: Option<Instant>,
    last_sync_gen: u64,
}

impl TerminalSession {
    /// Creates a new `TerminalSession` combining the terminal model, PTY channel, and parsers.
    #[must_use]
    pub fn new(terminal: Terminal, pty: Pty) -> Self {
        Self {
            terminal,
            pty,
            vt_parser: VtParser::new(),
            kitty_parser: KittyParser::new(),
            sync_output_start: None,
            last_sync_gen: 0,
        }
    }

    /// Read-only reference to the underlying terminal model.
    #[must_use]
    pub fn terminal(&self) -> &Terminal {
        &self.terminal
    }

    /// Mutable reference to the underlying terminal model.
    pub fn terminal_mut(&mut self) -> &mut Terminal {
        &mut self.terminal
    }

    /// Read-only reference to the underlying PTY handle.
    #[must_use]
    pub fn pty(&self) -> &Pty {
        &self.pty
    }

    /// Mutable reference to the underlying PTY handle.
    pub fn pty_mut(&mut self) -> &mut Pty {
        &mut self.pty
    }

    /// Writes user input bytes directly to the PTY master.
    pub fn write_input_bytes(&mut self, bytes: &[u8]) {
        write_all_to_pty(self.pty.as_fd(), bytes, INPUT_WRITE_BUDGET);
    }

    /// Wraps clipboard bytes in bracketed-paste markers when DEC mode 2004 is active.
    #[must_use]
    pub fn encode_paste(&self, bytes: &[u8]) -> Vec<u8> {
        wrap_bracketed_paste(bytes, self.terminal.bracketed_paste)
    }

    /// Pastes in-memory clipboard text, handing oversized payloads to a background writer.
    pub fn paste(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let payload = self.encode_paste(bytes);
        if payload.len() <= SYNC_PASTE_THRESHOLD {
            self.write_input_bytes(&payload);
            return;
        }
        match self.pty.try_clone_master() {
            Ok(fd) => {
                std::thread::spawn(move || write_all_to_pty(&fd, &payload, PASTE_WRITE_BUDGET));
            }
            Err(_) => self.write_input_bytes(&payload[..SYNC_PASTE_THRESHOLD]),
        }
    }

    /// Pastes clipboard text read from `fd`, draining at most [`MAX_PASTE_BYTES`] off the loop.
    ///
    /// The read and PTY write happen on a background thread so a slow clipboard owner or a
    /// backed-up shell cannot stall the event loop.
    pub fn paste_from_fd(&mut self, fd: OwnedFd) {
        let bracketed = self.terminal.bracketed_paste;
        let Ok(pty_fd) = self.pty.try_clone_master() else {
            return;
        };
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let read = std::fs::File::from(fd)
                .take(MAX_PASTE_BYTES as u64)
                .read_to_end(&mut bytes);
            if read.is_err() || bytes.is_empty() {
                return;
            }
            write_all_to_pty(
                &pty_fd,
                &wrap_bracketed_paste(&bytes, bracketed),
                PASTE_WRITE_BUDGET,
            );
        });
    }

    /// Applies an IME completion batch: surrounding-text erasure, then the committed text.
    pub fn ime_commit(&mut self, delete: Option<(u32, u32)>, commit: Option<String>) {
        let bytes = crate::input::ime::encode_ime_edits(delete, commit.as_deref());
        if !bytes.is_empty() {
            self.write_input_bytes(&bytes);
        }
    }

    /// Atomically updates terminal dimensions and the PTY master window size.
    ///
    /// # Errors
    /// Returns [`io::Error`] if the kernel ioctl `TIOCSWINSZ` fails.
    pub fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_pixels: [u16; 2],
        viewport_pixels: [u16; 2],
    ) -> io::Result<()> {
        self.pty
            .resize(cols, rows, viewport_pixels[0], viewport_pixels[1])?;
        let (c, r) = (cols as usize, rows as usize);
        if (self.terminal.grid.cols, self.terminal.grid.rows) != (c, r) {
            self.terminal.grid.resize(c, r);
        }
        self.terminal.set_geometry(cell_pixels, viewport_pixels);

        if !self.terminal.responses.is_empty() {
            for response in self.terminal.take_responses() {
                self.write_input_bytes(&response);
            }
        }
        Ok(())
    }

    /// Returns `true` if Mode 2026 synchronized output is active and has not timed out.
    pub fn is_synchronized_output_active(&mut self) -> bool {
        if self.terminal.sync_output_gen != self.last_sync_gen {
            self.last_sync_gen = self.terminal.sync_output_gen;
            self.sync_output_start = Some(Instant::now());
        }

        if self.terminal.synchronized_output {
            let start = *self.sync_output_start.get_or_insert_with(Instant::now);
            if start.elapsed() > SYNC_OUTPUT_TIMEOUT {
                self.terminal.synchronized_output = false;
                self.sync_output_start = None;
                false
            } else {
                true
            }
        } else {
            self.sync_output_start = None;
            false
        }
    }

    /// Processes an incoming raw slice of bytes from the PTY, demuxing Kitty and VT sequences.
    pub fn process_incoming_bytes(
        &mut self,
        incoming: &[u8],
        cell_width: f32,
        cell_height: f32,
        auto_scroll: bool,
    ) -> SessionOutcome {
        if incoming.is_empty() {
            return SessionOutcome::default();
        }

        let mut outcome = SessionOutcome {
            needs_redraw: true,
            ..Default::default()
        };

        if self.kitty_parser.is_fast_path(incoming) {
            self.advance_vt_bytes(incoming, auto_scroll, &mut outcome);
        } else {
            let mut parser = std::mem::take(&mut self.kitty_parser);

            parser.process_stream(incoming, |chunk| match chunk {
                KittyStreamChunk::Text(text) => {
                    self.advance_vt_bytes(text, auto_scroll, &mut outcome);
                }
                KittyStreamChunk::Event(event) => {
                    let active = collect_placeholder_ids(
                        self.terminal
                            .grid
                            .lines
                            .iter()
                            .chain(self.terminal.grid.alt_lines.iter().flatten())
                            .chain(self.terminal.grid.scrollback.iter()),
                    );
                    let ctx = GraphicContext {
                        cell_width,
                        cell_height,
                        cursor_row: self.terminal.grid.cursor.row,
                        cursor_col: self.terminal.grid.cursor.col,
                        scrollback_len: self.terminal.grid.scrollback.len(),
                    };
                    let graphic_outcome = self
                        .terminal
                        .grid
                        .graphics
                        .handle_event(*event, ctx, &active);

                    if let Some((cols, rows, do_not_move)) = graphic_outcome.cursor_advance {
                        self.advance_cursor_for_image(cols, rows, do_not_move);
                    }
                    if let Some(resp) = graphic_outcome.pty_response {
                        self.write_input_bytes(&resp);
                    }
                    if graphic_outcome.needs_redraw {
                        outcome.needs_redraw = true;
                        if auto_scroll && !self.terminal.grid.is_alt_screen() {
                            self.terminal.grid.scroll_viewport_bottom();
                        }
                    }
                }
            });

            self.kitty_parser = parser;
        }

        outcome.kitty_keyboard_flags = self.terminal.kitty_keyboard_flags;
        outcome
    }

    /// Reads up to 64 KiB from the PTY and processes all available terminal data.
    ///
    /// # Errors
    /// Returns [`io::Error`] if reading from the PTY encounters an unrecoverable failure.
    pub fn read_and_process_pty(
        &mut self,
        cell_width: f32,
        cell_height: f32,
        auto_scroll: bool,
    ) -> io::Result<SessionOutcome> {
        let mut buf = [0u8; 65536];
        let mut total_read = 0;
        let mut aggregated = SessionOutcome::default();

        loop {
            match self.pty.read(&mut buf) {
                Ok(n) if n > 0 => {
                    total_read += n;
                    let incoming = &buf[..n];
                    let step_outcome =
                        self.process_incoming_bytes(incoming, cell_width, cell_height, auto_scroll);

                    if step_outcome.needs_redraw {
                        aggregated.needs_redraw = true;
                    }
                    if let Some(title) = step_outcome.title_update {
                        aggregated.title_update = Some(title);
                    }
                    if let Some(clip) = step_outcome.clipboard_update {
                        aggregated.clipboard_update = Some(clip);
                    }
                    if step_outcome.palette_changed {
                        aggregated.palette_changed = true;
                    }
                    aggregated.kitty_keyboard_flags = step_outcome.kitty_keyboard_flags;

                    if total_read >= 65536 {
                        break;
                    }
                }
                Ok(_) => {
                    aggregated.process_exited = true;
                    return Ok(aggregated);
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    aggregated.process_exited = true;
                    return Err(e);
                }
            }
        }

        if total_read > 0 {
            aggregated.needs_redraw = true;
        }
        Ok(aggregated)
    }

    fn advance_vt_bytes(&mut self, text: &[u8], auto_scroll: bool, outcome: &mut SessionOutcome) {
        if text.is_empty() {
            return;
        }

        self.terminal.advance_bytes(&mut self.vt_parser, text);

        if self.terminal.sync_output_gen != self.last_sync_gen {
            self.last_sync_gen = self.terminal.sync_output_gen;
            self.sync_output_start = Some(Instant::now());
        }

        if !self.terminal.responses.is_empty() {
            for response in self.terminal.take_responses() {
                self.write_input_bytes(&response);
            }
        }

        if let Some(pending) = self.terminal.take_pending_clipboard() {
            outcome.clipboard_update = Some(pending);
        }

        if self.terminal.palette_dirty {
            self.terminal.palette_dirty = false;
            outcome.palette_changed = true;
            outcome.needs_redraw = true;
        }

        if self.terminal.title_dirty {
            self.terminal.title_dirty = false;
            outcome.title_update = Some(self.terminal.title.clone());
        }

        if auto_scroll && !self.terminal.grid.is_alt_screen() {
            self.terminal.grid.scroll_viewport_bottom();
        }
    }

    fn advance_cursor_for_image(&mut self, cols: usize, rows: usize, do_not_move: bool) {
        if do_not_move {
            return;
        }
        let grid_cols = self.terminal.grid.cols;
        let grid_rows = self.terminal.grid.rows;
        let scroll_top = self.terminal.grid.scroll_region_top;
        let scroll_bottom = self.terminal.grid.scroll_region_bottom;
        let cur_row = self.terminal.grid.cursor.row;
        let in_margins = cur_row >= scroll_top && cur_row <= scroll_bottom;

        let mut c_col = self.terminal.grid.cursor.col + cols;
        let mut c_row = if rows > 0 {
            cur_row + rows - 1
        } else {
            cur_row
        };

        if c_col >= grid_cols {
            c_col = 0;
            c_row += 1;
        }

        let margin_bottom = if in_margins {
            scroll_bottom
        } else {
            grid_rows.saturating_sub(1)
        };

        if c_row > margin_bottom {
            let excess = c_row - margin_bottom;
            if in_margins {
                self.terminal.grid.scroll_up(excess);
            } else {
                for _ in 0..excess {
                    self.terminal
                        .grid
                        .scroll_up_with_bg(1, crate::color::Color::DefaultBackground);
                }
            }
            c_row = margin_bottom;
        }

        self.terminal.grid.cursor.col = c_col.min(grid_cols.saturating_sub(1));
        self.terminal.grid.cursor.row = c_row.min(grid_rows.saturating_sub(1));
    }
}
