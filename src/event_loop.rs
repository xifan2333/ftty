//! Unified calloop single-threaded event loop multiplexing Wayland, PTY I/O, and POSIX signals.

use std::io::{self, Read, Write};
use std::path::PathBuf;

use calloop::generic::Generic;
use calloop::signals::{Signal, Signals};
use calloop::{EventLoop, Interest, Mode};
use calloop_wayland_source::WaylandSource;

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
use crate::ime::ImeState;
use crate::input::{KeyAction, KeyboardHandler};
use crate::kitty::{ImagePlacement, KittyAction, KittyEvent, KittyParser};
use crate::parser::Terminal;
use crate::pty::Pty;
use crate::render::{ColorScheme, RenderOptions, Renderer};
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
    pub last_click_time: u32,
    pub click_count: u8,
    pub last_click_cell: Option<(usize, usize)>,
    pub last_serial: u32,
    pub clipboard_text: Option<String>,
    pub pending_offers: Vec<crate::wayland::OfferData>,
    pub running: bool,
    pub needs_redraw: bool,
    frame_callback: Option<WlCallback>,
    pending_size: Option<[u32; 2]>,
    render_error: Option<io::Error>,
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
        let font_mgr = FontManager::load_with_family(config.font_family(), config.font_size())?;
        let atlas = GlyphAtlas::new(128, 128);
        let palette = config.build_palette();
        let default_fg = config.foreground();
        let default_bg = config.background();
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
            last_click_time: 0,
            click_count: 0,
            last_click_cell: None,
            last_serial: 0,
            clipboard_text: None,
            pending_offers: Vec::new(),
            running: true,
            needs_redraw: true,
            frame_callback: None,
            pending_size: None,
            render_error: None,
        })
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

    /// Copies the currently selected text to the Wayland clipboard and internal buffer.
    pub fn copy_selection(&mut self, qh: Option<&QueueHandle<Self>>) {
        let text = self.selection.extract_text(&self.terminal.grid);
        if text.is_empty() {
            return;
        }

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

    /// Writes bytes to the non-blocking PTY master with a bounded readiness loop to prevent truncation.
    pub fn write_pty_blocking(&mut self, mut bytes: &[u8]) {
        let start = std::time::Instant::now();
        while !bytes.is_empty() && start.elapsed() < std::time::Duration::from_millis(500) {
            let res =
                unsafe { libc::write(self.pty.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
            if res > 0 {
                bytes = &bytes[res as usize..];
            } else if res < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if err.kind() == io::ErrorKind::WouldBlock {
                    let mut pfd = libc::pollfd {
                        fd: self.pty.as_raw_fd(),
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    let poll_res = unsafe { libc::poll(&mut pfd, 1, 100) };
                    if poll_res > 0 {
                        continue;
                    }
                }
                break;
            } else {
                break;
            }
        }
    }

    /// Pastes text from the Wayland clipboard into the terminal PTY.
    pub fn paste_clipboard(&mut self, conn: Option<&Connection>) {
        if let Some(offer_data) = &self.wayland.current_offer {
            if let Some(mime) = best_text_mime(&offer_data.mime_types) {
                use std::os::fd::{AsFd, FromRawFd, OwnedFd};
                let mut fds = [0i32; 2];
                if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == 0 {
                    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
                    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };

                    offer_data.offer.receive(mime.to_string(), write_fd.as_fd());
                    drop(write_fd);

                    if let Some(c) = conn {
                        let _ = c.flush();
                    }

                    if let Ok(pty_fd) = self.pty.try_clone_master() {
                        std::thread::spawn(move || {
                            use std::io::Read;
                            use std::os::fd::AsRawFd;

                            // Limit paste payload to at most 10 MiB to prevent memory exhaustion
                            let mut reader = std::fs::File::from(read_fd).take(10 * 1024 * 1024);
                            let mut bytes = Vec::new();
                            if reader.read_to_end(&mut bytes).is_ok() && !bytes.is_empty() {
                                // PTY master is nonblocking: write with poll readiness loop to avoid truncation
                                let mut to_write = &bytes[..];
                                let start = std::time::Instant::now();
                                while !to_write.is_empty()
                                    && start.elapsed() < std::time::Duration::from_secs(5)
                                {
                                    let mut pfd = libc::pollfd {
                                        fd: pty_fd.as_raw_fd(),
                                        events: libc::POLLOUT,
                                        revents: 0,
                                    };
                                    let poll_res = unsafe { libc::poll(&mut pfd, 1, 1000) };
                                    if poll_res <= 0 {
                                        break;
                                    }
                                    let res = unsafe {
                                        libc::write(
                                            pty_fd.as_raw_fd(),
                                            to_write.as_ptr().cast(),
                                            to_write.len(),
                                        )
                                    };
                                    if res > 0 {
                                        to_write = &to_write[res as usize..];
                                    } else if res < 0 {
                                        let err = std::io::Error::last_os_error();
                                        if err.kind() == std::io::ErrorKind::Interrupted {
                                            continue;
                                        }
                                        if err.kind() == std::io::ErrorKind::WouldBlock {
                                            continue;
                                        }
                                        break;
                                    } else {
                                        break;
                                    }
                                }
                            }
                        });
                        return;
                    }
                }
            }
            return;
        }

        // Fallback to internal clipboard buffer if offer not available
        if let Some(text) = &self.clipboard_text {
            let _ = self.pty.write_all(text.as_bytes());
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

        let max_sb = new_config.scrollback_lines();
        self.terminal.grid.max_scrollback = max_sb;
        if self.terminal.grid.scrollback.len() > max_sb {
            let overflow = self.terminal.grid.scrollback.len() - max_sb;
            for _ in 0..overflow {
                self.terminal.grid.scrollback.pop_front();
            }
            self.terminal.grid.viewport_offset = self.terminal.grid.viewport_offset.min(max_sb);
        }

        let font_changed = self.config.font_family() != new_config.font_family()
            || (self.config.font_size() - new_config.font_size()).abs() > f32::EPSILON;

        let maybe_new_font = if font_changed {
            match FontManager::load_with_family(new_config.font_family(), new_config.font_size()) {
                Ok(mgr) => Some(mgr),
                Err(e) => {
                    eprintln!("ftty: failed to reload font face or size: {e}");
                    return;
                }
            }
        } else {
            None
        };

        let padding_changed = self.config.padding_x() != new_config.padding_x()
            || self.config.padding_y() != new_config.padding_y();

        self.palette = new_config.build_palette();
        self.default_fg = new_config.foreground();
        self.default_bg = new_config.background();
        self.terminal.grid.cursor.shape = new_config.cursor_shape();
        self.config = new_config;

        if let Some(new_font_mgr) = maybe_new_font {
            self.font_mgr = new_font_mgr;
            self.atlas.clear();
            let _ = self.resize_terminal();
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
                let new_size = (self.font_mgr.font_size() + 1.0).min(72.0);
                self.update_font_size(new_size);
            }
            KeyAction::FontDecrease => {
                let new_size = (self.font_mgr.font_size() - 1.0).max(6.0);
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
        if (self.font_mgr.font_size() - new_size).abs() < f32::EPSILON {
            return;
        }
        if let Ok(new_font_mgr) = FontManager::load_with_family(self.font_mgr.family(), new_size) {
            self.font_mgr = new_font_mgr;
            self.atlas.clear();
            let _ = self.resize_terminal();
            self.needs_redraw = true;
        }
    }

    fn resize_terminal(&mut self) -> io::Result<()> {
        let (cols, rows) = terminal_size(
            [self.wayland.width, self.wayland.height],
            self.font_mgr.metrics,
            [self.config.padding_x(), self.config.padding_y()],
        );
        if (self.terminal.grid.cols, self.terminal.grid.rows) != (cols as usize, rows as usize) {
            self.pty.resize(cols, rows)?;
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
        conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            proxy.ack_configure(serial);
            state.wayland.configured = true;

            if let Err(error) = state.configure_renderer(conn) {
                state.render_error = Some(error);
                state.running = false;
            }
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
            if caps.contains(Capability::Pointer) && state.wayland.pointer.is_none() {
                let pointer = proxy.get_pointer(qh, ());
                state.wayland.pointer = Some(pointer);
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
                format: _,
                fd,
                size,
            } => unsafe {
                use std::os::fd::AsRawFd;
                state
                    .keyboard
                    .set_keymap_from_fd(fd.as_raw_fd(), size as usize);
            },
            wl_keyboard::Event::Enter { surface, .. } => {
                if state.wayland.surface.as_ref() == Some(&surface) {
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
                    state.ime.clear();
                    if let Some(text_input) = &state.wayland.text_input {
                        text_input.disable();
                        text_input.commit();
                    }
                    state.needs_redraw = true;
                }
            }
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(KeyState::Pressed),
                ..
            } => {
                if let Some(action) = state.keyboard.check_action(key, &state.config.keybindings) {
                    state.handle_key_action(action, Some(qh), Some(_conn));
                } else if let Some(bytes) = state.keyboard.handle_key(key) {
                    if state.config.auto_scroll() && !state.terminal.grid.is_alt_screen() {
                        state.terminal.grid.scroll_viewport_bottom();
                    }
                    let _ = state.pty.write_all(&bytes);
                    state.update_ime_cursor_area();
                }
            }
            wl_keyboard::Event::Modifiers {
                serial: _,
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            } => {
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
                surface_x,
                surface_y,
                ..
            } => {
                state.mouse_pos = [surface_x, surface_y];
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.mouse_pos = [surface_x, surface_y];
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
                if button == 0x110 {
                    // BTN_LEFT
                    let (line, screen_row, col) =
                        state.cell_at_pointer(state.mouse_pos[0], state.mouse_pos[1]);
                    let same_cell = state.last_click_cell == Some((line, col));
                    if same_cell && time.saturating_sub(state.last_click_time) < 350 {
                        state.click_count = (state.click_count % 3) + 1;
                    } else {
                        state.click_count = 1;
                    }
                    state.last_click_time = time;
                    state.last_click_cell = Some((line, col));
                    state.mouse_pressed = true;

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
                                SelectionPoint::new(
                                    line,
                                    state.terminal.grid.cols.saturating_sub(1),
                                ),
                                SelectionType::Line,
                            );
                        }
                        _ => {}
                    }
                    state.needs_redraw = true;
                } else if button == 0x112 {
                    // BTN_MIDDLE: paste
                    state.paste_clipboard(Some(conn));
                }
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(ButtonState::Released),
                ..
            } => {
                if button == 0x110 {
                    state.mouse_pressed = false;
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
                if lines != 0 {
                    state.scroll_accumulator -= f64::from(lines);
                    if state.terminal.grid.is_alt_screen() {
                        let count = (lines.unsigned_abs() as usize).min(100);
                        let seq: &[u8] = if lines < 0 { b"\x1b[A" } else { b"\x1b[B" };
                        let batch = seq.repeat(count);
                        let _ = state.pty.write_all(&batch);
                    } else {
                        if lines < 0 {
                            state
                                .terminal
                                .grid
                                .scroll_viewport_up(lines.unsigned_abs() as usize);
                        } else {
                            state
                                .terminal
                                .grid
                                .scroll_viewport_down(lines.unsigned_abs() as usize);
                        }
                        state.needs_redraw = true;
                    }
                }
            }
            wl_pointer::Event::AxisStop { .. } => {
                state.scroll_accumulator = 0.0;
            }
            _ => {}
        }
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
            let mut buf = [0u8; 8192];
            match state.pty.read(&mut buf) {
                Ok(n) if n > 0 => {
                    let (clean_text, events) = state.kitty_parser.filter_bytes(&buf[..n]);
                    if !clean_text.is_empty() {
                        state.terminal.advance_bytes(&clean_text);
                        if state.config.auto_scroll() && !state.terminal.grid.is_alt_screen() {
                            state.terminal.grid.scroll_viewport_bottom();
                        }
                    }

                    for event in events {
                        match event {
                            KittyEvent::Transmit { command, image } => {
                                let image_id = image.id;
                                let placement_id = command.placement_id.unwrap_or(0);
                                let img_w = (image.width as f32).max(1.0);
                                let img_h = (image.height as f32).max(1.0);
                                state.terminal.grid.add_image(image);

                                let cw = state.font_mgr.metrics.cell_width as f32;
                                let ch = state.font_mgr.metrics.cell_height as f32;

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

                                let abs_line = state.terminal.grid.scrollback.len()
                                    + state.terminal.grid.cursor.row;
                                state.terminal.grid.add_placement(ImagePlacement {
                                    image_id,
                                    placement_id,
                                    line: abs_line,
                                    col: state.terminal.grid.cursor.col,
                                    cols,
                                    rows,
                                    offset_x: command.offset_x,
                                    offset_y: command.offset_y,
                                    z_index: command.z_index,
                                });

                                if !command.do_not_move_cursor {
                                    state.terminal.grid.cursor.col =
                                        (state.terminal.grid.cursor.col + cols)
                                            .min(state.terminal.grid.cols.saturating_sub(1));
                                }

                                if command.action == KittyAction::TransmitAndDisplayWithResponse {
                                    let resp = format!("\x1b_Gi={image_id};OK\x1b\\").into_bytes();
                                    state.write_pty_blocking(&resp);
                                }
                            }
                            KittyEvent::Place { command } => {
                                if let Some(image_id) = command.image_id {
                                    let placement_id = command.placement_id.unwrap_or(0);
                                    let cols = command.cols.unwrap_or(1) as usize;
                                    let rows = command.rows.unwrap_or(1) as usize;
                                    let abs_line = state.terminal.grid.scrollback.len()
                                        + state.terminal.grid.cursor.row;
                                    state.terminal.grid.add_placement(ImagePlacement {
                                        image_id,
                                        placement_id,
                                        line: abs_line,
                                        col: state.terminal.grid.cursor.col,
                                        cols,
                                        rows,
                                        offset_x: command.offset_x,
                                        offset_y: command.offset_y,
                                        z_index: command.z_index,
                                    });
                                }
                            }
                            KittyEvent::Delete { target } => {
                                state.terminal.grid.delete_images(target);
                            }
                            KittyEvent::Response(resp) => {
                                state.write_pty_blocking(&resp);
                            }
                        }
                    }

                    state.needs_redraw = true;
                    state.update_ime_cursor_area();
                    Ok(calloop::PostAction::Continue)
                }
                Ok(_) => {
                    // EOF on PTY master
                    state.running = false;
                    Ok(calloop::PostAction::Reregister)
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    Ok(calloop::PostAction::Continue)
                }
                Err(_) => {
                    // Child process likely exited (EIO on Linux PTY)
                    state.running = false;
                    Ok(calloop::PostAction::Reregister)
                }
            }
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
        event_loop
            .dispatch(None, &mut app_state)
            .map_err(io::Error::other)?;

        if !app_state.running {
            break;
        }
        if app_state.needs_redraw
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
            );
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

        // Write an invalid font size (0.0), along with changed colors and cursor
        std::fs::write(
            &config_path,
            r##"
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

        // Ensure state was NOT partially committed
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
}
