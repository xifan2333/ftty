//! Unified calloop single-threaded event loop multiplexing Wayland, PTY I/O, and POSIX signals.

use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::path::PathBuf;

use calloop::generic::Generic;
use calloop::signals::{Signal, Signals};
use calloop::{EventLoop, Interest, Mode};
use calloop_wayland_source::WaylandSource;
use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::poll::{PollFd, PollFlags, poll};

use wayland_client::protocol::{
    wl_callback::{self, WlCallback},
    wl_compositor::WlCompositor,
    wl_data_device::{self, WlDataDevice},
    wl_data_device_manager::WlDataDeviceManager,
    wl_data_offer::{self, WlDataOffer},
    wl_data_source::{self, WlDataSource},
    wl_keyboard::{self, KeyState, WlKeyboard},
    wl_pointer::{self, Axis, ButtonState, WlPointer},
    wl_registry::{self, WlRegistry},
    wl_seat::{self, Capability, WlSeat},
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{
    self, Shape, WpCursorShapeDeviceV1,
};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::{
    self, WpCursorShapeManagerV1,
};
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::{self, ZwpTextInputV3};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

use crate::color::Rgb;
use crate::config::Config;
use crate::font::{CellMetrics, FontManager, GlyphAtlas};
use crate::grid::CellFlags;
use crate::ime::ImeState;
use crate::input::{KeyAction, KeyboardHandler};
use crate::kitty::{ImagePlacement, KittyAction, KittyEvent, KittyParser, kitty_response};
use crate::mouse::{MouseModifiers, encode_mouse_event};
use crate::parser::Terminal;
use crate::pty::Pty;
use crate::render::{ColorScheme, HoveredHyperlinkSpan, RenderOptions, Renderer};
use crate::selection::{Selection, SelectionPoint, SelectionType, find_word_boundaries};
use crate::wayland::WaylandState;

/// Shared application state passed to all calloop sources and Wayland event dispatches.
pub struct AppState {
    // Destroy the native EGL window before the Wayland surface handles.
    pub renderer: Option<Renderer>,
    pub terminal: Terminal,
    pub pty: Pty,
    pub keyboard: KeyboardHandler,
    pub wayland: WaylandState,
    pub font_mgr: FontManager,
    pub atlas: GlyphAtlas,
    pub ime: ImeState,
    pub kitty_parser: KittyParser,
    pub config: Config,
    pub config_path: Option<PathBuf>,
    pub palette: [Rgb; 256],
    pub default_fg: Rgb,
    pub default_bg: Rgb,
    pub scroll_accumulator: f64,
    pub selection: Selection,
    pub mouse_pos: [f64; 2],
    pub mouse_pressed: bool,
    /// Bitmask of X11 button indexes currently held down and reported to the application.
    pub mouse_buttons_held: u8,
    /// Set while a button press was forwarded to a mouse-tracking application.
    pub mouse_reported: bool,
    /// X11 button index most recently forwarded to the PTY, used for drag motion.
    pub mouse_button: u8,
    pub last_click_time: u32,
    pub click_count: u8,
    pub last_click_cell: Option<(usize, usize)>,
    pub last_serial: u32,
    pub pointer_serial: u32,
    pub pointer_in_surface: bool,
    pub hovered_span: Option<HoveredHyperlinkSpan>,
    pub current_cursor_shape: Option<Shape>,
    pub clipboard_text: Option<String>,
    pub pending_offers: Vec<crate::wayland::OfferData>,
    pub running: bool,
    pub needs_redraw: bool,
    frame_callback: Option<WlCallback>,
    pending_size: Option<[u32; 2]>,
    /// Set by an `xdg_surface.configure`; the resize is applied once the queue is drained so a
    /// burst of configures collapses into a single, final size.
    configure_pending: bool,
    render_error: Option<io::Error>,
    sync_output_start: Option<std::time::Instant>,
    last_sync_gen: u64,
}

fn best_text_mime(mimes: &[String]) -> Option<&str> {
    for candidate in ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"] {
        if let Some(found) = mimes.iter().find(|m| m.as_str() == candidate) {
            return Some(found.as_str());
        }
    }
    None
}

impl AppState {
    /// Creates a new `AppState` with default configuration path.
    ///
    /// # Errors
    /// Returns [`std::io::Error`] if font discovery or configuration loading fails.
    pub fn new(terminal: Terminal, pty: Pty) -> Result<Self, io::Error> {
        Self::with_config(terminal, pty, None)
    }

    /// Creates a new `AppState` with terminal, PTY, optional custom configuration path.
    ///
    /// # Errors
    /// Returns [`std::io::Error`] if font discovery or configuration loading fails.
    pub fn with_config(
        mut terminal: Terminal,
        pty: Pty,
        config_path: Option<PathBuf>,
    ) -> Result<Self, io::Error> {
        let config = Config::load_from_path_or_default(config_path.as_deref())?;
        let font_mgr =
            FontManager::load_with_families(&config.font_families(), config.font_size())?;
        let mut atlas = GlyphAtlas::new(1024, 1024);
        for c in ' '..='~' {
            let _ = atlas.get_or_insert(c, CellFlags::empty(), &font_mgr);
        }
        let palette = config.build_palette();
        let default_fg = config.foreground();
        let default_bg = config.background();
        terminal.set_default_colors(default_fg, default_bg);
        terminal.grid.cursor.shape = config.cursor_shape();
        terminal.grid.max_scrollback = config.scrollback_lines();

        let mut wayland = WaylandState::new();
        let pad_x = u32::from(config.padding_x());
        let pad_y = u32::from(config.padding_y());
        wayland.width = (terminal.grid.cols as u32)
            .saturating_mul(font_mgr.metrics.cell_width)
            .saturating_add(pad_x * 2)
            .clamp(100, i32::MAX as u32);
        wayland.height = (terminal.grid.rows as u32)
            .saturating_mul(font_mgr.metrics.cell_height)
            .saturating_add(pad_y * 2)
            .clamp(100, i32::MAX as u32);

        // Publish the pixel geometry before the first frame so image clients can size
        // themselves without waiting for a window resize.
        let cell_pixels = [
            saturating_u16(font_mgr.metrics.cell_width),
            saturating_u16(font_mgr.metrics.cell_height),
        ];
        let viewport_pixels = [
            saturating_u16(wayland.width.saturating_sub(pad_x * 2)),
            saturating_u16(wayland.height.saturating_sub(pad_y * 2)),
        ];
        terminal.set_geometry(cell_pixels, viewport_pixels);
        pty.resize(
            terminal.grid.cols as u16,
            terminal.grid.rows as u16,
            viewport_pixels[0],
            viewport_pixels[1],
        )?;

        Ok(Self {
            terminal,
            pty,
            keyboard: KeyboardHandler::new(),
            wayland,
            font_mgr,
            atlas,
            ime: ImeState::new(),
            kitty_parser: KittyParser::new(),
            config,
            config_path,
            renderer: None,
            palette,
            default_fg,
            default_bg,
            scroll_accumulator: 0.0,
            selection: Selection::new(
                SelectionPoint::new(0, 0),
                SelectionPoint::new(0, 0),
                SelectionType::Simple,
            ),
            mouse_pos: [0.0, 0.0],
            mouse_pressed: false,
            mouse_buttons_held: 0,
            mouse_reported: false,
            mouse_button: 0,
            last_click_time: 0,
            click_count: 0,
            last_click_cell: None,
            last_serial: 0,
            pointer_serial: 0,
            pointer_in_surface: false,
            hovered_span: None,
            current_cursor_shape: None,
            clipboard_text: None,
            pending_offers: Vec::new(),
            running: true,
            needs_redraw: true,
            frame_callback: None,
            pending_size: None,
            configure_pending: false,
            render_error: None,
            sync_output_start: None,
            last_sync_gen: 0,
        })
    }

    /// Initializes the cursor shape device if protocol is available and updates shape if focused.
    pub fn try_init_cursor_shape(&mut self, qh: &QueueHandle<Self>) {
        if self.wayland.cursor_shape_device.is_none() {
            self.wayland.init_cursor_shape(qh);
            if self.wayland.cursor_shape_device.is_some()
                && self.pointer_in_surface
                && self.pointer_serial != 0
            {
                self.current_cursor_shape = None;
                self.update_cursor_shape();
            }
        }
    }

    /// Updates the Wayland cursor shape based on whether a hyperlink is currently hovered.
    pub fn update_cursor_shape(&mut self) {
        if !self.pointer_in_surface || self.pointer_serial == 0 {
            return;
        }
        let shape = if self.hovered_span.is_some() {
            Shape::Pointer
        } else {
            Shape::Text
        };
        if self.current_cursor_shape == Some(shape) {
            return;
        }
        if let Some(device) = &self.wayland.cursor_shape_device {
            device.set_shape(self.pointer_serial, shape);
            self.current_cursor_shape = Some(shape);
        }
    }

    /// Updates the hovered hyperlink and cursor shape based on current pointer coordinates.
    pub fn update_hover_state(&mut self) {
        let new_span = if !self.pointer_in_surface || self.mouse_pressed {
            None
        } else {
            let (line, screen_row, col) =
                self.cell_at_pointer(self.mouse_pos[0], self.mouse_pos[1]);
            let row = self.terminal.grid.visible_line(screen_row);
            if let Some(cell) = row.cells.get(col)
                && let Some(id) = cell.hyperlink_id
                && id > 0
                && self.terminal.hyperlink_url(id).is_some()
            {
                let mut start_col = col;
                while start_col > 0
                    && row.cells.get(start_col - 1).and_then(|c| c.hyperlink_id) == Some(id)
                {
                    start_col -= 1;
                }
                let mut end_col = col;
                while end_col + 1 < row.cells.len()
                    && row.cells.get(end_col + 1).and_then(|c| c.hyperlink_id) == Some(id)
                {
                    end_col += 1;
                }
                Some(HoveredHyperlinkSpan {
                    line,
                    start_col,
                    end_col,
                })
            } else {
                None
            }
        };
        if self.hovered_span != new_span {
            self.hovered_span = new_span;
            self.needs_redraw = true;
            self.update_cursor_shape();
        }
    }

    /// Returns the absolute `(line, screen_row, col)` grid coordinates under the surface-relative pointer position.
    #[must_use]
    pub fn cell_at_pointer(&self, surface_x: f64, surface_y: f64) -> (usize, usize, usize) {
        let cw = f64::from(self.font_mgr.metrics.cell_width);
        let ch = f64::from(self.font_mgr.metrics.cell_height);
        let pad_x = f64::from(self.config.padding_x());
        let pad_y = f64::from(self.config.padding_y());

        let col = ((surface_x - pad_x) / cw).max(0.0) as usize;
        let col = col.min(self.terminal.grid.cols.saturating_sub(1));

        let screen_row = ((surface_y - pad_y) / ch).max(0.0) as usize;
        let screen_row = screen_row.min(self.terminal.grid.rows.saturating_sub(1));

        let abs_line =
            self.terminal.grid.scrollback.len() + screen_row - self.terminal.grid.viewport_offset;
        (abs_line, screen_row, col)
    }

    /// Encodes a pointer event for the application, or `None` when the event stays local.
    ///
    /// Mouse reports are suppressed while tracking is disabled and while Shift is held,
    /// which is the conventional override that hands the pointer back to text selection.
    fn mouse_report_bytes(&self, button: u8, pressed: bool, motion: bool) -> Option<Vec<u8>> {
        if !self.terminal.mouse.is_reporting() {
            return None;
        }
        let modifiers = self.keyboard.modifiers();
        if modifiers.shift && pressed {
            return None;
        }
        let (_, screen_row, col) = self.cell_at_pointer(self.mouse_pos[0], self.mouse_pos[1]);
        encode_mouse_event(
            self.terminal.mouse.encoding,
            button,
            col,
            screen_row,
            pressed,
            motion,
            MouseModifiers {
                shift: modifiers.shift,
                alt: modifiers.alt,
                ctrl: modifiers.ctrl,
            },
        )
    }

    /// Forwards a pointer event to the PTY and reports whether the application consumed it.
    fn report_mouse_event(&mut self, button: u8, pressed: bool, motion: bool) -> bool {
        let Some(bytes) = self.mouse_report_bytes(button, pressed, motion) else {
            return false;
        };
        self.write_pty_blocking(&bytes);
        true
    }

    /// Sets the clipboard content internally and offers it through the Wayland data device.
    pub fn set_clipboard_text(&mut self, text: String, qh: Option<&QueueHandle<Self>>) {
        self.terminal.set_clipboard_content(Some(text.clone()));
        self.clipboard_text = Some(text);

        if let (Some(qh), Some(manager), Some(device)) = (
            qh,
            &self.wayland.data_device_manager,
            &self.wayland.data_device,
        ) {
            let source = manager.create_data_source(qh, ());
            source.offer("text/plain;charset=utf-8".to_string());
            source.offer("text/plain".to_string());
            source.offer("UTF8_STRING".to_string());
            device.set_selection(Some(&source), self.last_serial);
            self.wayland.data_source = Some(source);
        }
    }

    /// Copies the currently selected text to the Wayland clipboard and internal buffer.
    pub fn copy_selection(&mut self, qh: Option<&QueueHandle<Self>>) {
        let text = self.selection.extract_text(&self.terminal.grid);
        if text.is_empty() {
            return;
        }

        self.set_clipboard_text(text, qh);
    }

    /// Writes bytes to the non-blocking PTY master with a bounded readiness loop to prevent truncation.
    pub fn write_pty_blocking(&mut self, mut bytes: &[u8]) {
        let start = std::time::Instant::now();
        while !bytes.is_empty() && start.elapsed() < std::time::Duration::from_millis(100) {
            match self.pty.write(bytes) {
                Ok(0) => break,
                Ok(written) => bytes = &bytes[written..],
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    let mut fds = [PollFd::new(self.pty.as_fd(), PollFlags::POLLOUT)];
                    if !poll(&mut fds, 20u16).is_ok_and(|ready| ready > 0) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }

    /// Pastes text from the Wayland clipboard into the terminal PTY.
    pub fn paste_clipboard(&mut self, conn: Option<&Connection>) {
        let bracketed = self.terminal.bracketed_paste;
        if let Some(offer_data) = &self.wayland.current_offer {
            if let Some(mime) = best_text_mime(&offer_data.mime_types)
                && let Ok((read_fd, write_fd)) = nix::unistd::pipe2(OFlag::O_CLOEXEC)
            {
                offer_data.offer.receive(mime.to_string(), write_fd.as_fd());
                drop(write_fd);

                if let Some(c) = conn {
                    let _ = c.flush();
                }

                if let Ok(pty_fd) = self.pty.try_clone_master() {
                    std::thread::spawn(move || {
                        // Limit paste payload to at most 10 MiB to prevent memory exhaustion
                        let mut reader = std::fs::File::from(read_fd).take(10 * 1024 * 1024);
                        let mut bytes = Vec::new();
                        if reader.read_to_end(&mut bytes).is_ok() && !bytes.is_empty() {
                            let payload = if bracketed {
                                let mut wrapped = Vec::with_capacity(bytes.len() + 12);
                                wrapped.extend_from_slice(b"\x1b[200~");
                                wrapped.extend_from_slice(&bytes);
                                wrapped.extend_from_slice(b"\x1b[201~");
                                wrapped
                            } else {
                                bytes
                            };
                            // PTY master is nonblocking: write with poll readiness loop to avoid truncation
                            let mut to_write = &payload[..];
                            let start = std::time::Instant::now();
                            while !to_write.is_empty()
                                && start.elapsed() < std::time::Duration::from_secs(5)
                            {
                                let mut fds = [PollFd::new(pty_fd.as_fd(), PollFlags::POLLOUT)];
                                if !poll(&mut fds, 1000u16).is_ok_and(|ready| ready > 0) {
                                    break;
                                }
                                match nix::unistd::write(&pty_fd, to_write) {
                                    Ok(0) => break,
                                    Ok(written) => to_write = &to_write[written..],
                                    Err(Errno::EINTR | Errno::EAGAIN) => continue,
                                    Err(_) => break,
                                }
                            }
                        }
                    });
                    return;
                }
            }
            return;
        }

        // Fallback to internal clipboard buffer if offer not available
        let fallback_text = self.clipboard_text.clone();
        if let Some(text) = fallback_text {
            let payload = if bracketed {
                let mut wrapped = Vec::with_capacity(text.len() + 12);
                wrapped.extend_from_slice(b"\x1b[200~");
                wrapped.extend_from_slice(text.as_bytes());
                wrapped.extend_from_slice(b"\x1b[201~");
                wrapped
            } else {
                text.into_bytes()
            };

            if payload.len() <= 4096 {
                self.write_pty_blocking(&payload);
            } else if let Ok(pty_fd) = self.pty.try_clone_master() {
                std::thread::spawn(move || {
                    let mut to_write = &payload[..];
                    let start = std::time::Instant::now();
                    while !to_write.is_empty()
                        && start.elapsed() < std::time::Duration::from_secs(5)
                    {
                        let mut fds = [PollFd::new(pty_fd.as_fd(), PollFlags::POLLOUT)];
                        if !poll(&mut fds, 1000u16).is_ok_and(|ready| ready > 0) {
                            break;
                        }
                        match nix::unistd::write(&pty_fd, to_write) {
                            Ok(0) => break,
                            Ok(written) => to_write = &to_write[written..],
                            Err(Errno::EINTR | Errno::EAGAIN) => continue,
                            Err(_) => break,
                        }
                    }
                });
            } else {
                // If cloning PTY master failed, do not block the main event loop with an oversized write;
                // write bounded head chunk only.
                self.write_pty_blocking(&payload[..4096]);
            }
        }
    }

    /// Updates the Wayland `text-input-v3` cursor bounding box so the IME popup window tracks the cursor.
    pub fn update_ime_cursor_area(&self) {
        let Some(text_input) = &self.wayland.text_input else {
            return;
        };
        let (x, y, w, h) = crate::ime::calculate_cursor_rect(
            &self.terminal.grid,
            self.font_mgr.metrics,
            [self.config.padding_x(), self.config.padding_y()],
        );
        text_input.set_cursor_rectangle(x, y, w, h);
        text_input.commit();
    }

    /// Reloads the configuration file and dynamically updates palette, fonts, cursor, and metrics.
    pub fn reload_config(&mut self) {
        let new_config = match Config::load_from_path_or_default(self.config_path.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("ftty: failed to reload config: {e}");
                return;
            }
        };

        let families_changed = self.config.font_families() != new_config.font_families();
        let new_font_size = new_config.font_size();
        let size_changed = (self.config.font_size() - new_font_size).abs() > f32::EPSILON;

        if size_changed
            && (!new_font_size.is_finite()
                || new_font_size < crate::font::MIN_FONT_SIZE
                || new_font_size > crate::font::MAX_FONT_SIZE)
        {
            eprintln!("ftty: failed to reload font size: invalid size {new_font_size}");
            return;
        }

        let maybe_new_font = if families_changed {
            match FontManager::load_with_families(
                &new_config.font_families(),
                new_config.font_size(),
            ) {
                Ok(mgr) => Some(mgr),
                Err(e) => {
                    eprintln!("ftty: failed to reload font face: {e}");
                    return;
                }
            }
        } else {
            None
        };

        let max_sb = new_config.scrollback_lines();
        self.terminal.grid.max_scrollback = max_sb;
        if self.terminal.grid.scrollback.len() > max_sb {
            let overflow = self.terminal.grid.scrollback.len() - max_sb;
            for _ in 0..overflow {
                self.terminal.grid.scrollback.pop_front();
            }
            self.terminal.grid.viewport_offset = self.terminal.grid.viewport_offset.min(max_sb);
        }

        let padding_changed = self.config.padding_x() != new_config.padding_x()
            || self.config.padding_y() != new_config.padding_y();

        self.palette = new_config.build_palette();
        self.default_fg = new_config.foreground();
        self.default_bg = new_config.background();
        self.terminal
            .set_default_colors(self.default_fg, self.default_bg);
        self.terminal.grid.cursor.shape = new_config.cursor_shape();
        self.config = new_config;

        if let Some(new_font_mgr) = maybe_new_font {
            self.font_mgr = new_font_mgr;
            self.atlas.clear();
            let _ = self.resize_terminal();
        } else if size_changed {
            self.update_font_size(new_font_size);
        } else if padding_changed {
            let _ = self.resize_terminal();
        }

        self.needs_redraw = true;
    }

    /// Executes a semantic shortcut action (e.g. scroll page up, zoom font).
    pub fn handle_key_action(
        &mut self,
        action: KeyAction,
        qh: Option<&QueueHandle<Self>>,
        conn: Option<&Connection>,
    ) {
        match action {
            KeyAction::ScrollbackUpPage => {
                self.terminal
                    .grid
                    .scroll_viewport_up(self.terminal.grid.rows);
                self.needs_redraw = true;
            }
            KeyAction::ScrollbackDownPage => {
                self.terminal
                    .grid
                    .scroll_viewport_down(self.terminal.grid.rows);
                self.needs_redraw = true;
            }
            KeyAction::ScrollbackUpLine => {
                self.terminal.grid.scroll_viewport_up(1);
                self.needs_redraw = true;
            }
            KeyAction::ScrollbackDownLine => {
                self.terminal.grid.scroll_viewport_down(1);
                self.needs_redraw = true;
            }
            KeyAction::ScrollbackHome => {
                self.terminal.grid.scroll_viewport_top();
                self.needs_redraw = true;
            }
            KeyAction::ScrollbackEnd => {
                self.terminal.grid.scroll_viewport_bottom();
                self.needs_redraw = true;
            }
            KeyAction::FontIncrease => {
                let new_size = (self.font_mgr.font_size() + 1.0).min(crate::font::MAX_FONT_SIZE);
                self.update_font_size(new_size);
            }
            KeyAction::FontDecrease => {
                let new_size = (self.font_mgr.font_size() - 1.0).max(crate::font::MIN_FONT_SIZE);
                self.update_font_size(new_size);
            }
            KeyAction::FontReset => {
                let default_size = self.config.font_size();
                self.update_font_size(default_size);
            }
            KeyAction::ClipboardCopy => {
                self.copy_selection(qh);
            }
            KeyAction::ClipboardPaste | KeyAction::PrimaryPaste => {
                self.paste_clipboard(conn);
            }
        }
    }

    fn update_font_size(&mut self, new_size: f32) {
        if self.font_mgr.set_font_size(new_size) {
            self.atlas.clear();
            let _ = self.resize_terminal();
            self.needs_redraw = true;
        }
    }

    fn resize_terminal(&mut self) -> io::Result<()> {
        let padding = [self.config.padding_x(), self.config.padding_y()];
        let (cols, rows) = terminal_size(
            [self.wayland.width, self.wayland.height],
            self.font_mgr.metrics,
            padding,
        );
        let viewport_pixels = [
            saturating_u16(self.wayland.width.saturating_sub(u32::from(padding[0]) * 2)),
            saturating_u16(
                self.wayland
                    .height
                    .saturating_sub(u32::from(padding[1]) * 2),
            ),
        ];
        self.terminal.set_geometry(
            [
                saturating_u16(self.font_mgr.metrics.cell_width),
                saturating_u16(self.font_mgr.metrics.cell_height),
            ],
            viewport_pixels,
        );
        // The kernel only signals SIGWINCH on an actual change, so this is safe to repeat.
        self.pty
            .resize(cols, rows, viewport_pixels[0], viewport_pixels[1])?;
        if (self.terminal.grid.cols, self.terminal.grid.rows) != (cols as usize, rows as usize) {
            self.terminal.grid.resize(cols as usize, rows as usize);
        }
        Ok(())
    }

    fn configure_renderer(&mut self, connection: &Connection) -> io::Result<()> {
        let size = self
            .pending_size
            .take()
            .unwrap_or([self.wayland.width, self.wayland.height]);
        if let Some(renderer) = &self.renderer {
            renderer.resize(size)?;
        } else {
            let surface = self
                .wayland
                .surface
                .as_ref()
                .ok_or_else(|| io::Error::other("configured without a Wayland surface"))?;
            self.renderer = Some(Renderer::new(surface, connection, size)?);
        }
        [self.wayland.width, self.wayland.height] = size;
        self.resize_terminal()?;
        self.needs_redraw = true;
        // A resize must be committed even if the compositor suspended the old frame callback.
        self.frame_callback = None;
        Ok(())
    }

    fn handle_kitty_event(&mut self, event: KittyEvent) {
        match event {
            KittyEvent::Transmit { command, image } => {
                let image_id = image.id;
                let placement_id = command.placement_id.unwrap_or(0);
                let ack_id = command.placement_id.filter(|id| *id != 0);
                let img_w = (image.width as f32).max(1.0);
                let img_h = (image.height as f32).max(1.0);
                self.terminal.grid.add_image(image);

                let cw = self.font_mgr.metrics.cell_width as f32;
                let ch = self.font_mgr.metrics.cell_height as f32;

                let (cols, rows) = match (command.cols, command.rows) {
                    (Some(c), Some(r)) => (c as usize, r as usize),
                    (Some(c), None) => {
                        let pixel_w = c as f32 * cw;
                        let pixel_h = pixel_w * (img_h / img_w);
                        let r = (pixel_h / ch).ceil().max(1.0) as usize;
                        (c as usize, r)
                    }
                    (None, Some(r)) => {
                        let pixel_h = r as f32 * ch;
                        let pixel_w = pixel_h * (img_w / img_h);
                        let c = (pixel_w / cw).ceil().max(1.0) as usize;
                        (c, r as usize)
                    }
                    (None, None) => {
                        let c = (img_w / cw).ceil().max(1.0) as usize;
                        let r = (img_h / ch).ceil().max(1.0) as usize;
                        (c, r)
                    }
                };

                if command.is_virtual {
                    self.terminal
                        .grid
                        .virtual_placements
                        .insert(image_id, (cols, rows));
                } else {
                    let abs_line =
                        self.terminal.grid.scrollback.len() + self.terminal.grid.cursor.row;
                    self.terminal.grid.add_placement(ImagePlacement {
                        image_id,
                        placement_id,
                        line: abs_line,
                        col: self.terminal.grid.cursor.col,
                        cols,
                        rows,
                        offset_x: command.offset_x,
                        offset_y: command.offset_y,
                        z_index: command.z_index,
                    });

                    if !command.do_not_move_cursor {
                        self.terminal.grid.cursor.col = (self.terminal.grid.cursor.col + cols)
                            .min(self.terminal.grid.cols.saturating_sub(1));
                    }
                }

                let wants_ack = command.action == KittyAction::TransmitAndDisplayWithResponse
                    || command.id_explicit;
                if wants_ack && command.quiet == 0 {
                    let resp = kitty_response(image_id, ack_id, "OK");
                    self.write_pty_blocking(&resp);
                }
            }
            KittyEvent::Place { command } => {
                let Some(image_id) = command.image_id else {
                    return;
                };
                let placement_id = command.placement_id.unwrap_or(0);
                let ack_id = command.placement_id.filter(|id| *id != 0);
                if !self.terminal.grid.images.contains_key(&image_id) {
                    if command.quiet < 2 {
                        let resp = kitty_response(image_id, ack_id, "ENOENT:image not found");
                        self.write_pty_blocking(&resp);
                    }
                    return;
                }
                let cols = command.cols.unwrap_or(1) as usize;
                let rows = command.rows.unwrap_or(1) as usize;
                if command.is_virtual {
                    self.terminal
                        .grid
                        .virtual_placements
                        .insert(image_id, (cols, rows));
                } else {
                    let abs_line =
                        self.terminal.grid.scrollback.len() + self.terminal.grid.cursor.row;
                    self.terminal.grid.add_placement(ImagePlacement {
                        image_id,
                        placement_id,
                        line: abs_line,
                        col: self.terminal.grid.cursor.col,
                        cols,
                        rows,
                        offset_x: command.offset_x,
                        offset_y: command.offset_y,
                        z_index: command.z_index,
                    });
                }
                if command.quiet == 0 {
                    let resp = kitty_response(image_id, ack_id, "OK");
                    self.write_pty_blocking(&resp);
                }
            }
            KittyEvent::Delete { target } => {
                self.terminal.grid.delete_images(target);
            }
            KittyEvent::Response(resp) => {
                self.write_pty_blocking(&resp);
            }
        }
    }
}

fn saturating_u16(value: u32) -> u16 {
    value.min(u32::from(u16::MAX)) as u16
}

/// Maps a Linux input button code to the X11 mouse button index used on the wire.
fn x11_button_index(button: u32) -> Option<u8> {
    match button {
        0x110 => Some(0), // BTN_LEFT
        0x111 => Some(1), // BTN_MIDDLE
        0x112 => Some(2), // BTN_RIGHT
        _ => None,
    }
}

fn terminal_size([width, height]: [u32; 2], metrics: CellMetrics, padding: [u16; 2]) -> (u16, u16) {
    let usable_w = width.saturating_sub(u32::from(padding[0]) * 2);
    let usable_h = height.saturating_sub(u32::from(padding[1]) * 2);
    let cols = (usable_w / metrics.cell_width.max(1)).clamp(1, u16::MAX as u32) as u16;
    let rows = (usable_h / metrics.cell_height.max(1)).clamp(1, u16::MAX as u32) as u16;
    (cols, rows)
}

// ---------------------------------------------------------------------------
// Wayland Dispatch Implementations
// ---------------------------------------------------------------------------

impl Dispatch<WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    let comp = registry.bind::<WlCompositor, _, _>(name, version.min(4), qh, ());
                    state.wayland.compositor = Some(comp);
                    state.wayland.init_window(qh);
                }
                "xdg_wm_base" => {
                    let xdg = registry.bind::<XdgWmBase, _, _>(name, 1, qh, ());
                    state.wayland.xdg_wm_base = Some(xdg);
                    state.wayland.init_window(qh);
                }
                "wl_seat" => {
                    let seat = registry.bind::<WlSeat, _, _>(name, version.min(5), qh, ());
                    state.wayland.seat = Some(seat);
                    state.wayland.init_text_input(qh);
                    state.wayland.init_data_device(qh);
                }
                "zwp_text_input_manager_v3" => {
                    let manager = registry.bind::<ZwpTextInputManagerV3, _, _>(name, 1, qh, ());
                    state.wayland.text_input_manager = Some(manager);
                    state.wayland.init_text_input(qh);
                }
                "wl_data_device_manager" => {
                    let manager = registry.bind::<WlDataDeviceManager, _, _>(name, 3, qh, ());
                    state.wayland.data_device_manager = Some(manager);
                    state.wayland.init_data_device(qh);
                }
                "wp_cursor_shape_manager_v1" => {
                    let manager = registry.bind::<WpCursorShapeManagerV1, _, _>(name, 1, qh, ());
                    state.wayland.cursor_shape_manager = Some(manager);
                    state.try_init_cursor_shape(qh);
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlCompositor, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WlCompositor,
        _event: <WlCompositor as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSurface, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WlSurface,
        _event: <WlSurface as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<XdgWmBase, ()> for AppState {
    fn event(
        _state: &mut Self,
        proxy: &XdgWmBase,
        event: xdg_wm_base::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            proxy.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for AppState {
    fn event(
        state: &mut Self,
        proxy: &XdgSurface,
        event: xdg_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            proxy.ack_configure(serial);
            state.wayland.configured = true;
            // Defer the resize until the queue is drained: back-to-back configure pairs then
            // collapse into one size change instead of scrolling content on a transient size.
            state.configure_pending = true;
        }
    }
}

impl Dispatch<XdgToplevel, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &XdgToplevel,
        event: xdg_toplevel::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states: _,
            } => {
                let mut size = [state.wayland.width, state.wayland.height];
                // Zero lets the client choose that dimension independently.
                if width > 0 {
                    size[0] = width as u32;
                }
                if height > 0 {
                    size[1] = height as u32;
                }
                state.pending_size = Some(size);
            }
            xdg_toplevel::Event::Close => {
                state.wayland.close_requested = true;
                state.running = false;
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for AppState {
    fn event(
        state: &mut Self,
        proxy: &WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(Capability::Keyboard) && state.wayland.keyboard.is_none() {
                let keyboard = proxy.get_keyboard(qh, ());
                state.wayland.keyboard = Some(keyboard);
            }
            if caps.contains(Capability::Pointer) {
                if state.wayland.pointer.is_none() {
                    let pointer = proxy.get_pointer(qh, ());
                    state.wayland.pointer = Some(pointer);
                    state.try_init_cursor_shape(qh);
                }
            } else {
                if let Some(device) = state.wayland.cursor_shape_device.take() {
                    device.destroy();
                }
                if let Some(pointer) = state.wayland.pointer.take() {
                    pointer.release();
                }
                if state.hovered_span.is_some() {
                    state.hovered_span = None;
                    state.needs_redraw = true;
                }
                state.current_cursor_shape = None;
                state.pointer_in_surface = false;
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap {
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } => {
                state.keyboard.set_keymap_from_fd(fd, size as usize);
            }
            wl_keyboard::Event::Enter {
                serial, surface, ..
            } => {
                state.last_serial = serial;
                if state.wayland.surface.as_ref() == Some(&surface) {
                    if state.terminal.focus_reporting {
                        let _ = state.pty.write_all(b"\x1b[I");
                    }
                    state.ime.active = true;
                    if let Some(text_input) = &state.wayland.text_input {
                        text_input.enable();
                        text_input.set_content_type(
                            zwp_text_input_v3::ContentHint::None,
                            zwp_text_input_v3::ContentPurpose::Terminal,
                        );
                        let (x, y, w, h) = crate::ime::calculate_cursor_rect(
                            &state.terminal.grid,
                            state.font_mgr.metrics,
                            [state.config.padding_x(), state.config.padding_y()],
                        );
                        text_input.set_cursor_rectangle(x, y, w, h);
                        text_input.commit();
                    }
                }
            }
            wl_keyboard::Event::Leave { surface, .. } => {
                if state.wayland.surface.as_ref() == Some(&surface) {
                    if state.terminal.focus_reporting {
                        let _ = state.pty.write_all(b"\x1b[O");
                    }
                    state.ime.clear();
                    if let Some(text_input) = &state.wayland.text_input {
                        text_input.disable();
                        text_input.commit();
                    }
                    state.needs_redraw = true;
                }
            }
            wl_keyboard::Event::Key {
                serial,
                key,
                state: WEnum::Value(key_state),
                ..
            } => {
                state.last_serial = serial;
                let pressed = key_state == KeyState::Pressed;
                if pressed
                    && let Some(action) =
                        state.keyboard.check_action(key, &state.config.keybindings)
                {
                    state.handle_key_action(action, Some(qh), Some(_conn));
                } else if let Some(bytes) = state.keyboard.handle_key_event(key, pressed, false) {
                    if pressed && state.config.auto_scroll() && !state.terminal.grid.is_alt_screen()
                    {
                        state.terminal.grid.scroll_viewport_bottom();
                    }
                    let _ = state.pty.write_all(&bytes);
                    if pressed {
                        state.update_ime_cursor_area();
                    }
                }
            }
            wl_keyboard::Event::Modifiers {
                serial,
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            } => {
                state.last_serial = serial;
                state
                    .keyboard
                    .update_modifiers(mods_depressed, mods_latched, mods_locked, group);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlPointer, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WlPointer,
        event: wl_pointer::Event,
        _data: &(),
        conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_in_surface = true;
                state.pointer_serial = serial;
                state.last_serial = serial;
                state.mouse_pos = [surface_x, surface_y];
                state.current_cursor_shape = None;
                state.update_hover_state();
                state.update_cursor_shape();
            }
            wl_pointer::Event::Leave { .. } => {
                state.pointer_in_surface = false;
                if state.hovered_span.is_some() {
                    state.hovered_span = None;
                    state.needs_redraw = true;
                }
                state.current_cursor_shape = None;
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.mouse_pos = [surface_x, surface_y];
                state.update_hover_state();
                let held = state.mouse_buttons_held != 0 || state.mouse_reported;
                if state.terminal.mouse.reports_motion(held) {
                    let button = if held {
                        if state.mouse_buttons_held != 0 {
                            state.mouse_buttons_held.trailing_zeros() as u8
                        } else {
                            state.mouse_button
                        }
                    } else {
                        3
                    };
                    if state.report_mouse_event(button, true, true) {
                        return;
                    }
                }
                if state.mouse_pressed {
                    let (line, _, col) = state.cell_at_pointer(surface_x, surface_y);
                    state.selection.end = SelectionPoint::new(line, col);
                    state.needs_redraw = true;
                }
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(ButtonState::Pressed),
                time,
                serial,
            } => {
                state.last_serial = serial;
                let Some(index) = x11_button_index(button) else {
                    return;
                };
                // Applications that requested mouse tracking own the event; Shift
                // always overrides tracking so text can still be selected.
                if state.report_mouse_event(index, true, false) {
                    state.mouse_buttons_held |= 1 << index;
                    state.mouse_reported = true;
                    state.mouse_button = index;
                    return;
                }
                if index == 1 {
                    // BTN_MIDDLE pastes the primary selection, as elsewhere on X11.
                    state.paste_clipboard(Some(conn));
                    return;
                }
                if index != 0 {
                    return;
                }
                let (line, screen_row, col) =
                    state.cell_at_pointer(state.mouse_pos[0], state.mouse_pos[1]);

                if state.keyboard.modifiers().ctrl {
                    let cell = state.terminal.grid.visible_line(screen_row).cells.get(col);
                    if let Some(cell) = cell
                        && let Some(id) = cell.hyperlink_id
                        && let Some(url) = state.terminal.hyperlink_url(id)
                    {
                        let url_owned = url.to_string();
                        std::thread::spawn(move || {
                            let _ = std::process::Command::new("xdg-open")
                                .arg(&url_owned)
                                .spawn();
                        });
                        return;
                    }
                }

                let same_cell = state.last_click_cell == Some((line, col));
                if same_cell && time.saturating_sub(state.last_click_time) < 350 {
                    state.click_count = (state.click_count % 3) + 1;
                } else {
                    state.click_count = 1;
                }
                state.last_click_time = time;
                state.last_click_cell = Some((line, col));
                state.mouse_pressed = true;
                state.update_hover_state();

                match state.click_count {
                    1 => {
                        state.selection = Selection::new(
                            SelectionPoint::new(line, col),
                            SelectionPoint::new(line, col),
                            SelectionType::Simple,
                        );
                    }
                    2 => {
                        let row = state.terminal.grid.visible_line(screen_row);
                        let (w_start, w_end) = find_word_boundaries(row, col);
                        state.selection = Selection::new(
                            SelectionPoint::new(line, w_start),
                            SelectionPoint::new(line, w_end),
                            SelectionType::Word,
                        );
                    }
                    3 => {
                        state.selection = Selection::new(
                            SelectionPoint::new(line, 0),
                            SelectionPoint::new(line, state.terminal.grid.cols.saturating_sub(1)),
                            SelectionType::Line,
                        );
                    }
                    _ => {}
                }
                state.needs_redraw = true;
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(ButtonState::Released),
                ..
            } => {
                let Some(index) = x11_button_index(button) else {
                    return;
                };
                let was_held = (state.mouse_buttons_held & (1 << index)) != 0;
                if was_held {
                    state.mouse_buttons_held &= !(1 << index);
                    state.mouse_reported = state.mouse_buttons_held != 0;
                    state.report_mouse_event(index, false, false);
                }
                if index == 0 {
                    state.mouse_pressed = false;
                    state.update_hover_state();
                }
            }
            wl_pointer::Event::Axis {
                axis: WEnum::Value(Axis::VerticalScroll),
                value,
                ..
            } => {
                let multiplier = f64::from(state.config.scroll_multiplier());
                state.scroll_accumulator += (value / 15.0) * multiplier;

                let lines = state.scroll_accumulator.trunc() as i32;
                if lines == 0 {
                    return;
                }
                state.scroll_accumulator -= f64::from(lines);
                let count = (lines.unsigned_abs() as usize).min(100);

                if state.terminal.mouse.is_reporting() && !state.keyboard.modifiers().shift {
                    // Wheel notches are reported as buttons 64 (up) and 65 (down).
                    let button = if lines < 0 { 64 } else { 65 };
                    for _ in 0..count {
                        if !state.report_mouse_event(button, true, false) {
                            break;
                        }
                    }
                } else if state.terminal.grid.is_alt_screen() {
                    let seq: &[u8] = if lines < 0 { b"\x1b[A" } else { b"\x1b[B" };
                    let batch = seq.repeat(count);
                    let _ = state.pty.write_all(&batch);
                } else {
                    if lines < 0 {
                        state.terminal.grid.scroll_viewport_up(count);
                    } else {
                        state.terminal.grid.scroll_viewport_down(count);
                    }
                    state.needs_redraw = true;
                    state.update_hover_state();
                }
            }
            wl_pointer::Event::AxisStop { .. } => {
                state.scroll_accumulator = 0.0;
            }
            _ => {}
        }
    }
}

impl Dispatch<WpCursorShapeManagerV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WpCursorShapeManagerV1,
        _event: wp_cursor_shape_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpCursorShapeDeviceV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WpCursorShapeDeviceV1,
        _event: wp_cursor_shape_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlDataDeviceManager, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WlDataDeviceManager,
        _event: <WlDataDeviceManager as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlDataDevice, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WlDataDevice,
        event: wl_data_device::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                if state.pending_offers.len() >= 4 {
                    state.pending_offers.remove(0);
                }
                state.pending_offers.push(crate::wayland::OfferData {
                    offer: id,
                    mime_types: Vec::new(),
                });
            }
            wl_data_device::Event::Selection { id } => {
                state.wayland.current_offer = id.and_then(|offer| {
                    state
                        .pending_offers
                        .iter()
                        .position(|o| o.offer == offer)
                        .map(|idx| state.pending_offers.swap_remove(idx))
                });
                if state.wayland.current_offer.is_none() {
                    state.pending_offers.clear();
                }
            }
            _ => {}
        }
    }

    // data_offer creates a server-owned proxy before its MIME and selection events arrive.
    wayland_client::event_created_child!(AppState, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);
}

impl Dispatch<WlDataSource, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WlDataSource,
        event: wl_data_source::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type: _, fd } => {
                let text = state.clipboard_text.clone();
                std::thread::spawn(move || {
                    let mut file = std::fs::File::from(fd);
                    if let Some(text) = text {
                        let _ = file.write_all(text.as_bytes());
                    }
                });
            }
            wl_data_source::Event::Cancelled => {
                state.wayland.data_source = None;
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataOffer, ()> for AppState {
    fn event(
        state: &mut Self,
        proxy: &WlDataOffer,
        event: wl_data_offer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            if let Some(data) = state.pending_offers.iter_mut().find(|o| &o.offer == proxy) {
                data.mime_types.push(mime_type);
            } else if let Some(current) = &mut state.wayland.current_offer
                && &current.offer == proxy
            {
                current.mime_types.push(mime_type);
            }
        }
    }
}

impl Dispatch<WlCallback, ()> for AppState {
    fn event(
        state: &mut Self,
        proxy: &WlCallback,
        event: wl_callback::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event
            && state.frame_callback.as_ref() == Some(proxy)
        {
            state.frame_callback = None;
        }
    }
}

impl Dispatch<ZwpTextInputManagerV3, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpTextInputManagerV3,
        _event: <ZwpTextInputManagerV3 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpTextInputV3, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwp_text_input_v3::Event::Enter { surface } => {
                if state.wayland.surface.as_ref() == Some(&surface) {
                    state.ime.active = true;
                }
            }
            zwp_text_input_v3::Event::Leave { surface } => {
                if state.wayland.surface.as_ref() == Some(&surface) {
                    state.ime.clear();
                    state.needs_redraw = true;
                }
            }
            zwp_text_input_v3::Event::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => {
                state.ime.stage_preedit(text, cursor_begin, cursor_end);
            }
            zwp_text_input_v3::Event::CommitString { text } => {
                state.ime.stage_commit(text);
            }
            zwp_text_input_v3::Event::DeleteSurroundingText {
                before_length,
                after_length,
            } => {
                state.ime.stage_delete(before_length, after_length);
            }
            zwp_text_input_v3::Event::Done { .. } => {
                let (delete, commit) = state.ime.apply_done();
                if let Some((before, after)) = delete {
                    for _ in 0..before {
                        let _ = state.pty.write_all(b"\x08");
                    }
                    for _ in 0..after {
                        let _ = state.pty.write_all(b"\x1b[3~");
                    }
                }
                if let Some(text) = commit {
                    let _ = state.pty.write_all(text.as_bytes());
                }
                state.update_ime_cursor_area();
                state.needs_redraw = true;
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Unified Event Loop Runner
// ---------------------------------------------------------------------------

/// Runs the unified calloop event loop until terminal exits.
///
/// # Errors
/// Returns [`io::Error`] if Wayland connection, calloop initialization, or event dispatching fails.
pub fn run_event_loop(mut app_state: AppState) -> io::Result<()> {
    let conn = Connection::connect_to_env()
        .map_err(|e| io::Error::new(io::ErrorKind::ConnectionRefused, e))?;

    let event_queue = conn.new_event_queue();
    let qh = event_queue.handle();

    let display = conn.display();
    display.get_registry(&qh, ());

    let mut event_loop: EventLoop<AppState> = EventLoop::try_new().map_err(io::Error::other)?;

    // 1. Wayland Event Source
    let wayland_source = WaylandSource::new(conn.clone(), event_queue);
    event_loop
        .handle()
        .insert_source(wayland_source, |(), queue, state: &mut AppState| {
            queue.dispatch_pending(state)
        })
        .map_err(io::Error::other)?;

    // 2. PTY Master Read Event Source
    let pty_master = app_state.pty.try_clone_master()?;
    let pty_source = Generic::new(pty_master, Interest::READ, Mode::Level);
    event_loop
        .handle()
        .insert_source(pty_source, |_event, _fd, state: &mut AppState| {
            let mut buf = [0u8; 16384];
            let mut total_read = 0;
            loop {
                match state.pty.read(&mut buf) {
                    Ok(n) if n > 0 => {
                        total_read += n;
                        let (clean_text, events) = state.kitty_parser.filter_bytes(&buf[..n]);
                        for event in &events {
                            if let KittyEvent::Response(resp) = event {
                                state.write_pty_blocking(resp);
                            }
                        }

                        if !clean_text.is_empty() {
                            state.terminal.advance_bytes(&clean_text);
                            for response in state.terminal.take_responses() {
                                state.write_pty_blocking(&response);
                            }
                            if let Some(pending) = state.terminal.take_pending_clipboard() {
                                match pending {
                                    Some(text) => state.set_clipboard_text(text, Some(&qh)),
                                    None => {
                                        state.clipboard_text = None;
                                        state.terminal.set_clipboard_content(None);
                                        if let Some(device) = &state.wayland.data_device {
                                            device.set_selection(None, state.last_serial);
                                        }
                                    }
                                }
                            }
                            state.keyboard.kitty_flags = state.terminal.kitty_keyboard_flags;
                            if state.config.auto_scroll() && !state.terminal.grid.is_alt_screen() {
                                state.terminal.grid.scroll_viewport_bottom();
                            }
                        }

                        for event in events {
                            if !matches!(event, KittyEvent::Response(_)) {
                                state.handle_kitty_event(event);
                            }
                        }

                        if total_read >= 65536 {
                            break;
                        }
                    }
                    Ok(_) => {
                        // EOF on PTY master
                        state.running = false;
                        return Ok(calloop::PostAction::Reregister);
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        // Child process likely exited (EIO on Linux PTY)
                        state.running = false;
                        return Ok(calloop::PostAction::Reregister);
                    }
                }
            }

            if total_read > 0 {
                state.needs_redraw = true;
                state.update_ime_cursor_area();
            }
            Ok(calloop::PostAction::Continue)
        })
        .map_err(io::Error::other)?;

    // 3. POSIX Signals Event Source
    let signals = Signals::new(&[
        Signal::SIGCHLD,
        Signal::SIGINT,
        Signal::SIGTERM,
        Signal::SIGUSR1,
    ])
    .map_err(io::Error::other)?;
    event_loop
        .handle()
        .insert_source(signals, |event, _, state: &mut AppState| {
            match event.signal() {
                Signal::SIGCHLD => {
                    if !state.pty.is_alive() {
                        state.running = false;
                    }
                }
                Signal::SIGINT | Signal::SIGTERM => {
                    state.running = false;
                }
                Signal::SIGUSR1 => {
                    state.reload_config();
                }
                _ => {}
            }
        })
        .map_err(io::Error::other)?;

    // 4. Main Event Loop Tick
    while app_state.running {
        let dispatch_timeout = if app_state.terminal.synchronized_output {
            Some(std::time::Duration::from_millis(50))
        } else {
            None
        };

        event_loop
            .dispatch(dispatch_timeout, &mut app_state)
            .map_err(io::Error::other)?;

        if !app_state.running {
            break;
        }

        // Apply the last configure of this dispatch, if any, before drawing the frame.
        if app_state.configure_pending {
            app_state.configure_pending = false;
            if let Err(error) = app_state.configure_renderer(&conn) {
                app_state.render_error = Some(error);
                app_state.running = false;
                break;
            }
        }

        if app_state.terminal.sync_output_gen != app_state.last_sync_gen {
            app_state.last_sync_gen = app_state.terminal.sync_output_gen;
            app_state.sync_output_start = Some(std::time::Instant::now());
        }

        let mut sync_active = app_state.terminal.synchronized_output;
        if sync_active {
            let start = *app_state
                .sync_output_start
                .get_or_insert_with(std::time::Instant::now);
            if start.elapsed() > std::time::Duration::from_millis(150) {
                app_state.terminal.synchronized_output = false;
                sync_active = false;
                app_state.sync_output_start = None;
            }
        } else {
            app_state.sync_output_start = None;
        }

        if app_state.needs_redraw && !sync_active && app_state.frame_callback.is_none() {
            app_state.update_hover_state();
        }

        if app_state.needs_redraw
            && !sync_active
            && app_state.frame_callback.is_none()
            && let Some(renderer) = &mut app_state.renderer
        {
            let colors = ColorScheme::new(
                &app_state.palette,
                app_state.default_fg,
                app_state.default_bg,
            );
            let options = RenderOptions::new(
                [app_state.config.padding_x(), app_state.config.padding_y()],
                app_state.ime.preedit.as_ref(),
                Some(&app_state.selection),
            )
            .with_hovered_span(app_state.hovered_span);
            renderer.render_grid(
                &app_state.terminal.grid,
                colors,
                &app_state.font_mgr,
                &mut app_state.atlas,
                [app_state.wayland.width, app_state.wayland.height],
                options,
            )?;
            if let Some(surface) = &app_state.wayland.surface {
                app_state.frame_callback = Some(surface.frame(&qh, ()));
            }
            renderer.present()?;
            app_state.update_ime_cursor_area();
            app_state.needs_redraw = false;
        }

        let _ = conn.flush();
    }

    app_state.render_error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn clipboard_offer_is_created_and_dispatched_from_the_wire() {
        use std::os::unix::net::UnixStream;
        use wayland_client::Proxy;

        let (client, mut server) = UnixStream::pair().unwrap();
        let conn = Connection::from_socket(client).unwrap();
        let mut queue = conn.new_event_queue::<AppState>();
        let qh = queue.handle();
        let registry = conn.display().get_registry(&qh, ());
        let manager = registry.bind::<WlDataDeviceManager, _, _>(1, 3, &qh, ());
        let seat = registry.bind::<WlSeat, _, _>(2, 5, &qh, ());
        let device = manager.get_data_device(&seat, &qh, ());

        // Encode the startup clipboard sequence from a compositor without requiring
        // a desktop session in CI: data_offer(new_id), offer(MIME), selection(id).
        let offer_id = 0xff00_0000u32;
        let device_id = device.id().protocol_id();
        let mime = b"text/plain;charset=utf-8\0";
        let mut events = Vec::new();
        for word in [device_id, 12 << 16, offer_id] {
            events.extend_from_slice(&word.to_ne_bytes());
        }
        let padded_len = mime.len().next_multiple_of(4);
        for word in [
            offer_id,
            ((12 + padded_len) as u32) << 16,
            mime.len() as u32,
        ] {
            events.extend_from_slice(&word.to_ne_bytes());
        }
        events.extend_from_slice(mime);
        events.resize(events.len() + padded_len - mime.len(), 0);
        for word in [device_id, (12 << 16) | 5, offer_id] {
            events.extend_from_slice(&word.to_ne_bytes());
        }
        server.write_all(&events).unwrap();

        let term = Terminal::new(80, 24, 100);
        let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).unwrap();
        let mut app = AppState::new(term, pty).unwrap();
        conn.prepare_read().unwrap().read().unwrap();
        queue.dispatch_pending(&mut app).unwrap();

        let offer = app.wayland.current_offer.as_ref().unwrap();
        assert_eq!(offer.offer.id().protocol_id(), offer_id);
        assert_eq!(offer.mime_types, ["text/plain;charset=utf-8"]);
        assert!(app.pending_offers.is_empty());
    }

    #[test]
    fn terminal_dimensions_use_metrics_and_fit_the_pty() {
        let metrics = CellMetrics {
            cell_width: 9,
            cell_height: 18,
            ascent: 14,
        };
        assert_eq!(terminal_size([720, 480], metrics, [0, 0]), (80, 26));
        assert_eq!(terminal_size([720, 480], metrics, [18, 18]), (76, 24));
        assert_eq!(terminal_size([1, 1], metrics, [0, 0]), (1, 1));
        assert_eq!(
            terminal_size([u32::MAX, u32::MAX], metrics, [0, 0]),
            (u16::MAX, u16::MAX)
        );
    }

    #[test]
    fn test_app_state_initialization() {
        let term = Terminal::new(80, 24, 100);
        let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
        let app = AppState::new(term, pty).expect("AppState new");

        assert!(app.running);
        assert_eq!(app.terminal.grid.cols, 80);
        assert_eq!(app.terminal.grid.rows, 24);
        assert!(app.font_mgr.metrics.cell_width > 0);
        assert!(app.font_mgr.metrics.cell_height > 0);
        assert!(!app.wayland.configured);
    }

    #[test]
    fn test_pty_and_terminal_roundtrip() {
        let term = Terminal::new(80, 24, 100);
        let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
        let mut app = AppState::new(term, pty).expect("AppState new");

        // Send a command to shell via PTY
        app.pty
            .write_all(b"echo ftty_test_ok\n")
            .expect("write to pty");

        // Wait briefly and read output back into terminal
        let mut received = false;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(20));
            let mut buf = [0u8; 1024];
            if let Ok(n) = app.pty.read(&mut buf)
                && n > 0
            {
                app.terminal.advance_bytes(&buf[..n]);
                let full_screen: String = app
                    .terminal
                    .grid
                    .lines
                    .iter()
                    .flat_map(|r| r.cells.iter().map(|c| c.c))
                    .collect();
                if full_screen.contains("ftty_test_ok") {
                    received = true;
                    break;
                }
            }
        }
        assert!(received, "Expected shell echo in terminal grid");
    }

    #[test]
    fn test_app_state_reload_config() {
        use crate::grid::CursorShape;

        let temp_dir = std::env::temp_dir().join(format!("ftty_reload_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let config_path = temp_dir.join("ftty.toml");

        std::fs::write(
            &config_path,
            r##"
            [cursor]
            shape = "block"

            [colors]
            foreground = "#ffffff"
            background = "#000000"
            "##,
        )
        .unwrap();

        let term = Terminal::new(80, 24, 100);
        let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
        let mut app = AppState::with_config(term, pty, Some(config_path.clone()))
            .expect("AppState with_config");

        assert_eq!(app.default_bg, Rgb::new(0, 0, 0));
        assert_eq!(app.default_fg, Rgb::new(255, 255, 255));
        assert_eq!(app.terminal.grid.cursor.shape, CursorShape::Block);

        // Update config file with new colors, cursor, and padding
        std::fs::write(
            &config_path,
            r##"
            [window]
            padding_x = 20
            padding_y = 20

            [cursor]
            shape = "underline"

            [colors]
            foreground = "#ff0000"
            background = "#123456"
            "##,
        )
        .unwrap();

        app.needs_redraw = false;
        app.reload_config();

        assert_eq!(app.default_bg, Rgb::new(18, 52, 86));
        assert_eq!(app.default_fg, Rgb::new(255, 0, 0));
        assert_eq!(app.terminal.grid.cursor.shape, CursorShape::Underline);
        assert_eq!(app.config.padding_x(), 20);
        assert_eq!(app.config.padding_y(), 20);
        assert!(app.needs_redraw);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_failed_reload_preserves_state() {
        use crate::grid::CursorShape;

        let temp_dir =
            std::env::temp_dir().join(format!("ftty_reload_fail_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let config_path = temp_dir.join("ftty.toml");

        std::fs::write(
            &config_path,
            r##"
            [scrollback]
            lines = 100

            [font]
            size = 14.0

            [cursor]
            shape = "block"

            [colors]
            foreground = "#ffffff"
            background = "#000000"
            "##,
        )
        .unwrap();

        let term = Terminal::new(80, 24, 100);
        let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
        let mut app = AppState::with_config(term, pty, Some(config_path.clone()))
            .expect("AppState with_config");

        // Populate scrollback with lines
        for _ in 0..10 {
            app.terminal
                .grid
                .scrollback
                .push_back(crate::grid::Row::new(80));
        }
        assert_eq!(app.terminal.grid.scrollback.len(), 10);

        // Write an invalid font size (0.0), along with changed colors, cursor, and smaller scrollback
        std::fs::write(
            &config_path,
            r##"
            [scrollback]
            lines = 2

            [font]
            size = 0.0

            [cursor]
            shape = "underline"

            [colors]
            foreground = "#ff0000"
            background = "#123456"
            "##,
        )
        .unwrap();

        app.needs_redraw = false;
        app.reload_config();

        // Ensure state was NOT partially committed (scrollback must not be truncated!)
        assert_eq!(app.terminal.grid.scrollback.len(), 10);
        assert_eq!(app.terminal.grid.max_scrollback, 100);
        assert_eq!(app.default_bg, Rgb::new(0, 0, 0));
        assert_eq!(app.default_fg, Rgb::new(255, 255, 255));
        assert_eq!(app.terminal.grid.cursor.shape, CursorShape::Block);
        assert_eq!(app.font_mgr.font_size(), 14.0);
        assert!(!app.needs_redraw);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_handle_key_actions_scrolling_and_zoom() {
        use crate::color::Color;
        use crate::grid::CellFlags;

        let term = Terminal::new(80, 24, 100);
        let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
        let mut app = AppState::new(term, pty).expect("AppState new");

        // Populate lines and scrollback
        for _ in 0..50 {
            app.terminal.grid.write_char(
                'A',
                Color::DefaultForeground,
                Color::DefaultBackground,
                CellFlags::empty(),
            );
            app.terminal.grid.newline();
        }

        assert_eq!(app.terminal.grid.viewport_offset(), 0);

        // Page Up
        app.handle_key_action(KeyAction::ScrollbackUpPage, None, None);
        assert_eq!(app.terminal.grid.viewport_offset(), 24);

        // Scroll to Top
        app.handle_key_action(KeyAction::ScrollbackHome, None, None);
        assert_eq!(
            app.terminal.grid.viewport_offset(),
            app.terminal.grid.scrollback.len()
        );

        // Line Down
        let top = app.terminal.grid.viewport_offset();
        app.handle_key_action(KeyAction::ScrollbackDownLine, None, None);
        assert_eq!(app.terminal.grid.viewport_offset(), top - 1);

        // Scroll to Bottom
        app.handle_key_action(KeyAction::ScrollbackEnd, None, None);
        assert_eq!(app.terminal.grid.viewport_offset(), 0);

        // Font zoom actions
        let initial_size = app.font_mgr.font_size();
        app.handle_key_action(KeyAction::FontIncrease, None, None);
        assert_eq!(app.font_mgr.font_size(), initial_size + 1.0);

        app.handle_key_action(KeyAction::FontDecrease, None, None);
        assert_eq!(app.font_mgr.font_size(), initial_size);

        app.handle_key_action(KeyAction::FontReset, None, None);
        assert_eq!(app.font_mgr.font_size(), app.config.font_size());
    }

    #[test]
    fn test_copy_and_paste_clipboard() {
        use crate::color::Color;
        use crate::grid::CellFlags;

        let term = Terminal::new(80, 24, 100);
        let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
        let mut app = AppState::new(term, pty).expect("AppState new");

        for c in "copied_text".chars() {
            app.terminal.grid.write_char(
                c,
                Color::DefaultForeground,
                Color::DefaultBackground,
                CellFlags::empty(),
            );
        }

        app.selection = Selection::new(
            SelectionPoint::new(0, 0),
            SelectionPoint::new(0, 10),
            SelectionType::Simple,
        );

        app.copy_selection(None);
        assert_eq!(app.clipboard_text, Some("copied_text".to_string()));

        app.paste_clipboard(None);
    }

    #[test]
    fn pointer_events_from_the_wire_drive_mouse_reports() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;
        use wayland_client::Proxy;

        use crate::mouse::{MouseEncoding, MouseTracking};

        let (client, mut server) = UnixStream::pair().unwrap();
        let conn = Connection::from_socket(client).unwrap();
        let mut queue = conn.new_event_queue::<AppState>();
        let qh = queue.handle();
        let registry = conn.display().get_registry(&qh, ());
        let seat = registry.bind::<WlSeat, _, _>(1, 5, &qh, ());

        // wl_seat.capabilities(pointer): the client binds wl_pointer in response.
        let mut events = Vec::new();
        for word in [seat.id().protocol_id(), 12 << 16, 1] {
            events.extend_from_slice(&word.to_ne_bytes());
        }
        server.write_all(&events).unwrap();

        let term = Terminal::new(80, 24, 100);
        let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).unwrap();
        let mut app = AppState::new(term, pty).unwrap();
        app.terminal.mouse.tracking = MouseTracking::Drag;
        app.terminal.mouse.encoding = MouseEncoding::Sgr;
        conn.prepare_read().unwrap().read().unwrap();
        queue.dispatch_pending(&mut app).unwrap();

        let pointer = app.wayland.pointer.clone().expect("wl_pointer bound");
        let pointer_id = pointer.id().protocol_id();
        let cw = app.font_mgr.metrics.cell_width as f64;
        let ch = app.font_mgr.metrics.cell_height as f64;
        let x = ((cw * 2.5) * 256.0) as i32 as u32;
        let y = ((ch * 1.5) * 256.0) as i32 as u32;

        // wl_pointer.motion(time, x, y) followed by wl_pointer.button(serial, time, BTN_LEFT, pressed).
        let mut events = Vec::new();
        for word in [pointer_id, (20 << 16) | 2, 7, x, y] {
            events.extend_from_slice(&word.to_ne_bytes());
        }
        for word in [pointer_id, (24 << 16) | 3, 9, 9, 0x110, 1] {
            events.extend_from_slice(&word.to_ne_bytes());
        }
        server.write_all(&events).unwrap();
        conn.prepare_read().unwrap().read().unwrap();
        queue.dispatch_pending(&mut app).unwrap();

        assert!(app.mouse_reported, "left press was not forwarded");
        assert_eq!(app.mouse_button, 0);
        assert!(
            !app.mouse_pressed,
            "local selection must stay idle while reporting"
        );
        assert_eq!(app.last_serial, 9);

        // Turning tracking off hands the very same press back to local selection.
        app.terminal.mouse.tracking = MouseTracking::Disabled;
        app.mouse_reported = false;
        let mut events = Vec::new();
        for word in [pointer_id, (24 << 16) | 3, 10, 10, 0x110, 1] {
            events.extend_from_slice(&word.to_ne_bytes());
        }
        server.write_all(&events).unwrap();
        conn.prepare_read().unwrap().read().unwrap();
        queue.dispatch_pending(&mut app).unwrap();
        assert!(app.mouse_pressed);
        assert!(!app.mouse_reported);
        assert_eq!(app.selection.start, SelectionPoint::new(1, 2));
    }

    #[test]
    fn x11_buttons_map_to_protocol_indexes() {
        assert_eq!(x11_button_index(0x110), Some(0));
        assert_eq!(x11_button_index(0x111), Some(1));
        assert_eq!(x11_button_index(0x112), Some(2));
        assert_eq!(x11_button_index(0x113), None);
    }

    #[test]
    fn mouse_reports_are_forwarded_only_when_tracking_is_enabled() {
        use crate::mouse::{MouseEncoding, MouseTracking};

        let term = Terminal::new(80, 24, 100);
        let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
        let mut app = AppState::new(term, pty).expect("AppState new");
        let cw = f64::from(app.font_mgr.metrics.cell_width);
        let ch = f64::from(app.font_mgr.metrics.cell_height);
        app.mouse_pos = [cw * 2.5, ch * 1.5];

        // Tracking disabled: the pointer stays available for local text selection.
        assert!(app.mouse_report_bytes(0, true, false).is_none());

        app.terminal.mouse.tracking = MouseTracking::Drag;
        app.terminal.mouse.encoding = MouseEncoding::Sgr;
        assert_eq!(
            app.mouse_report_bytes(0, true, false),
            Some(b"\x1b[<0;3;2M".to_vec())
        );
        assert_eq!(
            app.mouse_report_bytes(0, false, false),
            Some(b"\x1b[<0;3;2m".to_vec())
        );
        assert_eq!(
            app.mouse_report_bytes(64, true, false),
            Some(b"\x1b[<64;3;2M".to_vec())
        );

        // Shift is the conventional escape hatch back to local selection.
        app.keyboard.update_modifiers(1, 0, 0, 0);
        assert!(app.keyboard.modifiers().shift);
        assert!(app.mouse_report_bytes(0, true, false).is_none());
    }

    #[test]
    fn test_cell_at_pointer_calculation() {
        let term = Terminal::new(80, 24, 100);
        let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
        let app = AppState::new(term, pty).expect("AppState new");

        let cw = f64::from(app.font_mgr.metrics.cell_width);
        let ch = f64::from(app.font_mgr.metrics.cell_height);

        let (line, screen_row, col) = app.cell_at_pointer(cw * 5.5, ch * 3.5);
        assert_eq!(col, 5);
        assert_eq!(screen_row, 3);
        assert_eq!(line, 3);
    }

    #[test]
    fn test_font_chain_reload_and_zoom_preserves_fallbacks() {
        let temp_dir = std::env::temp_dir().join(format!("ftty_font_chain_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let config_path = temp_dir.join("ftty.toml");

        std::fs::write(
            &config_path,
            r##"
            [font]
            families = ["monospace", "sans-serif"]
            size = 15.0
            "##,
        )
        .unwrap();

        let term = Terminal::new(80, 24, 100);
        let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
        let mut app = AppState::with_config(term, pty, Some(config_path.clone()))
            .expect("AppState with_config");

        assert_eq!(
            app.font_mgr.families(),
            &["monospace".to_string(), "sans-serif".to_string()]
        );
        assert_eq!(app.font_mgr.font_size(), 15.0);

        // Zoom font in: families chain must be preserved
        app.handle_key_action(KeyAction::FontIncrease, None, None);
        assert_eq!(app.font_mgr.font_size(), 16.0);
        assert_eq!(
            app.font_mgr.families(),
            &["monospace".to_string(), "sans-serif".to_string()]
        );

        // Reload with single family: families chain updates
        std::fs::write(
            &config_path,
            r##"
            [font]
            family = "monospace"
            size = 14.0
            "##,
        )
        .unwrap();

        app.reload_config();
        assert_eq!(app.font_mgr.families(), &["monospace".to_string()]);
        assert_eq!(app.font_mgr.font_size(), 14.0);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_update_hover_state_and_pointer_shape() {
        let term = Terminal::new(80, 24, 100);
        let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
        let mut app = AppState::new(term, pty).expect("AppState new");

        // Intern a real URL so hyperlink_id 1 is resolvable
        let id = app
            .terminal
            .get_or_intern_hyperlink("https://example.com".to_string());
        assert_eq!(id, 1);

        // Cells at cols 4..=6 have hyperlink ID 1
        for col in 4..=6 {
            app.terminal.grid.lines[0].cells[col].hyperlink_id = Some(1);
        }
        // Cell at col 8 has hyperlink ID 0 (invalid/exhausted sentinel)
        app.terminal.grid.lines[0].cells[8].hyperlink_id = Some(0);

        let cw = f64::from(app.font_mgr.metrics.cell_width);
        let ch = f64::from(app.font_mgr.metrics.cell_height);

        // If pointer is not in surface, hover must be None
        app.pointer_in_surface = false;
        app.mouse_pos = [cw * 5.5, ch * 0.5];
        app.update_hover_state();
        assert_eq!(app.hovered_span, None);

        // Enter surface
        app.pointer_in_surface = true;

        // Pointer over cell (row 0, col 0) has no hyperlink
        app.mouse_pos = [cw * 0.5, ch * 0.5];
        app.update_hover_state();
        assert_eq!(app.hovered_span, None);

        // Move pointer over cell (row 0, col 5): detects contiguous span [4..=6]
        app.mouse_pos = [cw * 5.5, ch * 0.5];
        app.needs_redraw = false;
        app.update_hover_state();
        assert_eq!(
            app.hovered_span,
            Some(HoveredHyperlinkSpan {
                line: 0,
                start_col: 4,
                end_col: 6,
            })
        );
        assert!(app.needs_redraw);

        // Pointer over cell with sentinel id 0 (pool exhaustion) must produce no hover
        app.mouse_pos = [cw * 8.5, ch * 0.5];
        app.needs_redraw = false;
        app.update_hover_state();
        assert_eq!(app.hovered_span, None);
        assert!(app.needs_redraw);

        // Back to col 5
        app.mouse_pos = [cw * 5.5, ch * 0.5];
        app.update_hover_state();
        assert!(app.hovered_span.is_some());

        // If mouse is pressed (dragging selection), hover is suppressed
        app.mouse_pressed = true;
        app.needs_redraw = false;
        app.update_hover_state();
        assert_eq!(app.hovered_span, None);
        assert!(app.needs_redraw);

        // When mouse is released, hover is restored
        app.mouse_pressed = false;
        app.update_hover_state();
        assert_eq!(
            app.hovered_span,
            Some(HoveredHyperlinkSpan {
                line: 0,
                start_col: 4,
                end_col: 6,
            })
        );
    }
}
