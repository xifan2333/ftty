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
    wl_keyboard::{self, KeyState, WlKeyboard},
    wl_pointer::{self, Axis, WlPointer},
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
use crate::parser::Terminal;
use crate::pty::Pty;
use crate::render::{ColorScheme, RenderOptions, Renderer};
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
    pub config: Config,
    pub config_path: Option<PathBuf>,
    pub palette: [Rgb; 256],
    pub default_fg: Rgb,
    pub default_bg: Rgb,
    pub scroll_accumulator: f64,
    pub running: bool,
    pub needs_redraw: bool,
    frame_callback: Option<WlCallback>,
    pending_size: Option<[u32; 2]>,
    render_error: Option<io::Error>,
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
            config,
            config_path,
            renderer: None,
            palette,
            default_fg,
            default_bg,
            scroll_accumulator: 0.0,
            running: true,
            needs_redraw: true,
            frame_callback: None,
            pending_size: None,
            render_error: None,
        })
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
    pub fn handle_key_action(&mut self, action: KeyAction) {
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
            KeyAction::ClipboardCopy | KeyAction::ClipboardPaste | KeyAction::PrimaryPaste => {}
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
                }
                "zwp_text_input_manager_v3" => {
                    let manager = registry.bind::<ZwpTextInputManagerV3, _, _>(name, 1, qh, ());
                    state.wayland.text_input_manager = Some(manager);
                    state.wayland.init_text_input(qh);
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
        _qh: &QueueHandle<Self>,
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
                    state.handle_key_action(action);
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
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
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
                        let seq = if lines < 0 { b"\x1b[A" } else { b"\x1b[B" };
                        for _ in 0..lines.unsigned_abs() {
                            let _ = state.pty.write_all(seq);
                        }
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
                    state.terminal.advance_bytes(&buf[..n]);
                    if state.config.auto_scroll() && !state.terminal.grid.is_alt_screen() {
                        state.terminal.grid.scroll_viewport_bottom();
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
        app.handle_key_action(KeyAction::ScrollbackUpPage);
        assert_eq!(app.terminal.grid.viewport_offset(), 24);

        // Scroll to Top
        app.handle_key_action(KeyAction::ScrollbackHome);
        assert_eq!(
            app.terminal.grid.viewport_offset(),
            app.terminal.grid.scrollback.len()
        );

        // Line Down
        let top = app.terminal.grid.viewport_offset();
        app.handle_key_action(KeyAction::ScrollbackDownLine);
        assert_eq!(app.terminal.grid.viewport_offset(), top - 1);

        // Scroll to Bottom
        app.handle_key_action(KeyAction::ScrollbackEnd);
        assert_eq!(app.terminal.grid.viewport_offset(), 0);

        // Font zoom actions
        let initial_size = app.font_mgr.font_size();
        app.handle_key_action(KeyAction::FontIncrease);
        assert_eq!(app.font_mgr.font_size(), initial_size + 1.0);

        app.handle_key_action(KeyAction::FontDecrease);
        assert_eq!(app.font_mgr.font_size(), initial_size);

        app.handle_key_action(KeyAction::FontReset);
        assert_eq!(app.font_mgr.font_size(), app.config.font_size());
    }
}
