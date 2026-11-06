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
                // No capacity yet: wait for readiness and retry. Only `budget` ends the loop,
                // otherwise a momentarily full PTY would silently truncate the payload.
                let mut fds = [nix::poll::PollFd::new(
                    borrowed,
                    nix::poll::PollFlags::POLLOUT,
                )];
                match nix::poll::poll(&mut fds, POLL_SLICE_MS) {
                    Ok(_) => continue,
                    Err(Errno::EINTR) => continue,
                    Err(_) => break,
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
    /// Whether the thread palette or default colors changed since the last report.
    pub theme_changed: bool,
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
    last_theme_revision: u64,
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
            last_theme_revision: 0,
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
        let bytes = &bytes[..bytes.len().min(MAX_PASTE_BYTES)];
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
                    if step_outcome.theme_changed {
                        aggregated.theme_changed = true;
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

        let theme_revision = self.terminal.theme.revision();
        if theme_revision != self.last_theme_revision {
            self.last_theme_revision = theme_revision;
            outcome.theme_changed = true;
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Read;
    use std::time::Duration;

    fn create_test_session(cols: u16, rows: u16) -> TerminalSession {
        let term = Terminal::new(cols as usize, rows as usize, 100);
        let pty = Pty::spawn(Some(&["/bin/sh"]), cols, rows).expect("spawn test shell");
        TerminalSession::new(term, pty)
    }

    #[test]
    fn test_session_initialization_and_accessors() {
        let mut session = create_test_session(80, 24);
        assert_eq!(session.terminal().grid.cols, 80);
        assert_eq!(session.terminal().grid.rows, 24);
        assert_eq!(session.terminal_mut().grid.max_scrollback, 100);
        assert!(session.pty().is_alive());
    }

    #[test]
    fn test_session_write_input_bytes_and_pty_roundtrip() {
        let mut session = create_test_session(80, 24);

        session.write_input_bytes(b"echo ftty_session_ok\n");

        let mut found = false;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(20));
            let mut buf = [0u8; 1024];
            if let Ok(n) = session.pty_mut().read(&mut buf)
                && n > 0
            {
                let outcome = session.process_incoming_bytes(&buf[..n], 10.0, 20.0, true);
                assert!(outcome.needs_redraw);

                let text: String = session
                    .terminal()
                    .grid
                    .lines
                    .iter()
                    .flat_map(|r| r.cells.iter().map(|c| c.c))
                    .collect();
                if text.contains("ftty_session_ok") {
                    found = true;
                    break;
                }
            }
        }
        assert!(found, "Expected shell echo inside terminal session");
    }

    #[test]
    fn test_session_title_update_via_osc() {
        let mut session = create_test_session(80, 24);

        let outcome =
            session.process_incoming_bytes(b"\x1b]0;session_window_title\x07", 10.0, 20.0, false);
        assert_eq!(
            outcome.title_update,
            Some("session_window_title".to_string())
        );
    }

    #[test]
    fn test_session_palette_update_via_osc() {
        let mut session = create_test_session(80, 24);
        let before = session.terminal().theme.revision();

        // Set palette color 1 to red via OSC 4
        let outcome =
            session.process_incoming_bytes(b"\x1b]4;1;rgb:ff/00/00\x07", 10.0, 20.0, false);
        assert!(outcome.theme_changed);
        assert!(outcome.needs_redraw);
        assert_eq!(
            session.terminal().theme.palette_color(1),
            Some(crate::color::Rgb::new(255, 0, 0))
        );
        assert!(session.terminal().theme.revision() > before);

        // A second batch with no theme mutation reports no change.
        let quiet = session.process_incoming_bytes(b"x", 10.0, 20.0, false);
        assert!(!quiet.theme_changed);
    }

    #[test]
    fn test_theme_resets_restore_configured_colors() {
        let mut session = create_test_session(80, 24);
        let configured = crate::color::Rgb::new(0x11, 0x22, 0x33);
        session.terminal_mut().theme.apply(
            crate::color::default_256_palette(),
            configured,
            configured,
        );

        // OSC 10 overrides the foreground; OSC 110 restores the configured value.
        let _ = session.process_incoming_bytes(b"\x1b]10;rgb:ff/ff/ff\x07", 10.0, 20.0, false);
        assert_eq!(
            session.terminal().theme.default_fg(),
            crate::color::Rgb::new(255, 255, 255)
        );
        let _ = session.process_incoming_bytes(b"\x1b]110\x07", 10.0, 20.0, false);
        assert_eq!(session.terminal().theme.default_fg(), configured);

        // OSC 4 mutates one entry; OSC 104 with an index restores just that entry.
        let _ = session.process_incoming_bytes(b"\x1b]4;3;rgb:00/ff/00\x07", 10.0, 20.0, false);
        assert_ne!(
            session.terminal().theme.palette_color(3),
            session.terminal().theme.configured_palette_color(3)
        );
        let _ = session.process_incoming_bytes(b"\x1b]104;3\x07", 10.0, 20.0, false);
        assert_eq!(
            session.terminal().theme.palette_color(3),
            session.terminal().theme.configured_palette_color(3)
        );
    }

    #[test]
    fn test_session_osc52_clipboard_integration() {
        let mut session = create_test_session(80, 24);
        session.terminal_mut().allow_osc52_write = true;

        // "hello" in base64 is "aGVsbG8="
        let outcome = session.process_incoming_bytes(b"\x1b]52;c;aGVsbG8=\x07", 10.0, 20.0, false);
        assert_eq!(outcome.clipboard_update, Some(Some("hello".to_string())));
    }

    #[test]
    fn test_session_mode_2026_synchronized_output_timeout() {
        let mut session = create_test_session(80, 24);

        // Initially inactive
        assert!(!session.is_synchronized_output_active());

        // Enter Mode 2026
        let _ = session.process_incoming_bytes(b"\x1b[?2026h", 10.0, 20.0, false);
        assert!(session.terminal().synchronized_output);
        assert!(session.is_synchronized_output_active());

        // Wait past the 150ms timeout
        std::thread::sleep(Duration::from_millis(160));

        // Must automatically time out and disable synchronized output
        assert!(!session.is_synchronized_output_active());
        assert!(!session.terminal().synchronized_output);
    }

    #[test]
    fn test_session_atomic_resize() {
        let mut session = create_test_session(80, 24);

        assert_eq!(session.terminal().grid.cols, 80);
        assert_eq!(session.terminal().grid.rows, 24);

        session
            .resize(100, 30, [10, 20], [1000, 600])
            .expect("session resize");

        assert_eq!(session.terminal().grid.cols, 100);
        assert_eq!(session.terminal().grid.rows, 30);
        assert_eq!(session.terminal().cell_pixels, [10, 20]);
        assert_eq!(session.terminal().viewport_pixels, [1000, 600]);
    }

    #[test]
    fn test_session_kitty_stream_placement_and_cursor_advance() {
        use base64::Engine;
        use base64::prelude::BASE64_STANDARD;

        let mut session = create_test_session(80, 24);

        let raw = [0u8, 0, 255, 255];
        let b64 = BASE64_STANDARD.encode(raw);
        let payload = format!("\x1b_Ga=T,f=32,s=1,v=1,i=77;{b64}\x1b\\");
        let outcome = session.process_incoming_bytes(payload.as_bytes(), 10.0, 20.0, false);

        assert!(outcome.needs_redraw);
        assert_eq!(session.terminal().grid.graphics.placements().len(), 1);
    }

    #[test]
    fn test_session_read_and_process_pty_title_aggregation() {
        let mut session = create_test_session(80, 24);
        session.write_input_bytes(b"printf '\\033]0;aggregated_title\\007'\n");

        let mut found_title = false;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(20));
            if let Ok(outcome) = session.read_and_process_pty(10.0, 20.0, false)
                && outcome.title_update == Some("aggregated_title".to_string())
            {
                found_title = true;
                break;
            }
        }
        assert!(
            found_title,
            "read_and_process_pty must aggregate title notifications"
        );
    }

    #[test]
    fn test_encode_paste_wraps_only_when_bracketed_mode_active() {
        let mut session = create_test_session(80, 24);

        // Bracketed paste is off by default: bytes pass through untouched.
        assert!(!session.terminal().bracketed_paste);
        assert_eq!(session.encode_paste(b"hello"), b"hello".to_vec());

        // Enabling DEC mode 2004 wraps the payload in the bracketed markers.
        let _ = session.process_incoming_bytes(b"\x1b[?2004h", 10.0, 20.0, false);
        assert!(session.terminal().bracketed_paste);
        assert_eq!(
            session.encode_paste(b"hello"),
            b"\x1b[200~hello\x1b[201~".to_vec()
        );

        // Disabling the mode restores pass-through behaviour.
        let _ = session.process_incoming_bytes(b"\x1b[?2004l", 10.0, 20.0, false);
        assert_eq!(session.encode_paste(b"hello"), b"hello".to_vec());
    }

    #[test]
    fn test_session_ime_commit_encodes_erasure_then_text() {
        use crate::input::ime::encode_ime_edits;

        assert_eq!(encode_ime_edits(None, None), Vec::<u8>::new());
        assert_eq!(encode_ime_edits(Some((0, 0)), None), Vec::<u8>::new());
        // Two characters erased before the cursor emit two DEL bytes, matching the Backspace key.
        assert_eq!(encode_ime_edits(Some((2, 0)), None), b"\x7f\x7f".to_vec());
        // One character erased after the cursor emits a single forward delete.
        assert_eq!(encode_ime_edits(Some((0, 1)), None), b"\x1b[3~".to_vec());
        // Erasure always precedes the committed text.
        assert_eq!(
            encode_ime_edits(Some((1, 1)), Some("\u{4f60}")),
            b"\x7f\x1b[3~\xe4\xbd\xa0".to_vec()
        );
        assert_eq!(encode_ime_edits(None, Some("hi")), b"hi".to_vec());
    }

    #[test]
    fn test_encode_ime_edits_clamps_hostile_delete_lengths() {
        use crate::input::ime::{MAX_IME_ERASE_CHARS, encode_ime_edits};

        // A hostile compositor must not force an unbounded allocation: output stays bounded.
        let bytes = encode_ime_edits(Some((u32::MAX, u32::MAX)), None);
        let expected = MAX_IME_ERASE_CHARS as usize * (1 + 4);
        assert_eq!(bytes.len(), expected);
    }

    #[test]
    fn test_session_paste_writes_payload_to_pty() {
        let mut session = create_test_session(80, 24);

        // Small payloads stay synchronous on the calling thread.
        session.paste(b"echo ftty_paste_ok\n");

        let mut echoed = String::new();
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(20));
            let _ = session.read_and_process_pty(10.0, 20.0, false);
            echoed = session
                .terminal()
                .grid
                .lines
                .iter()
                .flat_map(|row| row.cells.iter().map(|c| c.c))
                .collect();
            if echoed.contains("ftty_paste_ok") {
                break;
            }
        }
        assert!(
            echoed.contains("ftty_paste_ok"),
            "pasted command should reach the shell"
        );
    }

    #[test]
    fn test_session_paste_from_fd_streams_to_pty() {
        let mut session = create_test_session(80, 24);

        // Simulate a clipboard owner writing into the read end of a pipe.
        let (read_fd, write_fd) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).expect("pipe2");
        nix::unistd::write(&write_fd, b"echo ftty_fd_paste_ok\n").expect("write pipe");
        drop(write_fd);

        session.paste_from_fd(read_fd);

        let mut echoed = String::new();
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(20));
            let _ = session.read_and_process_pty(10.0, 20.0, false);
            echoed = session
                .terminal()
                .grid
                .lines
                .iter()
                .flat_map(|row| row.cells.iter().map(|c| c.c))
                .collect();
            if echoed.contains("ftty_fd_paste_ok") {
                break;
            }
        }
        assert!(
            echoed.contains("ftty_fd_paste_ok"),
            "paste_from_fd should drain the clipboard fd into the shell"
        );
    }
}
