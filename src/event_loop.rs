//! Unified calloop single-threaded event loop multiplexing Wayland, PTY I/O, and POSIX signals.

use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::path::PathBuf;

use calloop::generic::Generic;
use calloop::signals::{Signal, Signals};
use calloop::{EventLoop, Interest, Mode};
use calloop_wayland_source::WaylandSource;
use nix::poll::{PollFd, PollFlags, poll};

use wayland_client::protocol::wl_callback::WlCallback;
use wayland_client::{Connection, QueueHandle};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::Shape;

use crate::color::Rgb;
use crate::config::Config;
use crate::error::{FttyError, WaylandError};
use crate::font::{CellMetrics, FontManager, GlyphAtlas};
use crate::input::ime::ImeState;
use crate::input::selection::{Selection, SelectionPoint, SelectionType};
use crate::input::{KeyAction, KeyboardHandler};
use crate::kitty::{ImagePlacement, KittyAction, KittyEvent, KittyParser, kitty_response};
use crate::parser::Terminal;
use crate::pty::Pty;
use crate::render::{ColorScheme, HoveredHyperlinkSpan, RenderOptions, Renderer};
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
    pub vt_parser: crate::parser::VtParser,
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
    pub(crate) frame_callback: Option<WlCallback>,
    #[doc(hidden)]
    pub pending_size: Option<[u32; 2]>,
    /// Set by an `xdg_surface.configure`; the resize is applied once the queue is drained so a
    /// burst of configures collapses into a single, final size.
    pub(crate) configure_pending: bool,
    pub(crate) render_error: Option<FttyError>,
    pub(crate) sync_output_start: Option<std::time::Instant>,
    pub(crate) last_sync_gen: u64,
    #[doc(hidden)]
    pub pty_registered: bool,
    pub(crate) last_activity: std::time::Instant,
    pub(crate) last_trim: std::time::Instant,
    pub(crate) pending_trim: bool,
    pub logical_font_size: f32,
}

impl AppState {
    /// Creates a new `AppState` with default configuration path.
    ///
    /// # Errors
    /// Returns [`FttyError`] if font discovery or configuration loading fails.
    pub fn new(terminal: Terminal, pty: Pty) -> Result<Self, FttyError> {
        Self::with_config(terminal, pty, None)
    }

    /// Creates a new `AppState` with terminal, PTY, optional custom configuration path.
    ///
    /// # Errors
    /// Returns [`FttyError`] if font discovery or configuration loading fails.
    pub fn with_config(
        terminal: Terminal,
        pty: Pty,
        config_path: Option<PathBuf>,
    ) -> Result<Self, FttyError> {
        let config = if cfg!(test) && config_path.is_none() {
            Config::default()
        } else {
            Config::load_from_path_or_default(config_path.as_deref())?
        };
        Self::with_loaded_config(terminal, pty, config, config_path)
    }

    /// Returns `true` if the reported Wayland output geometry supports horizontal LCD subpixel rendering.
    #[must_use]
    pub fn is_subpixel_preferred(&self) -> bool {
        matches!(
            self.wayland
                .output_subpixel
                .unwrap_or(wayland_client::protocol::wl_output::Subpixel::HorizontalRgb),
            wayland_client::protocol::wl_output::Subpixel::HorizontalRgb
                | wayland_client::protocol::wl_output::Subpixel::HorizontalBgr
        )
    }

    /// Returns `true` if horizontal LCD subpixel rendering is supported by GPU and preferred by output.
    #[must_use]
    pub fn is_subpixel_enabled(&self) -> bool {
        let has_dual_source = self.renderer.as_ref().is_some_and(|r| r.has_dual_source);
        has_dual_source && self.is_subpixel_preferred()
    }

    /// Returns `true` if the reported Wayland output geometry has a BGR horizontal subpixel layout.
    #[must_use]
    pub fn is_bgr_subpixel(&self) -> bool {
        self.wayland.output_subpixel
            == Some(wayland_client::protocol::wl_output::Subpixel::HorizontalBgr)
    }

    /// Creates a new `AppState` with terminal, PTY, pre-loaded configuration, and optional configuration path.
    ///
    /// # Errors
    /// Returns [`FttyError`] if font discovery fails.
    pub fn with_loaded_config(
        terminal: Terminal,
        pty: Pty,
        config: Config,
        config_path: Option<PathBuf>,
    ) -> Result<Self, FttyError> {
        let font_mgr =
            FontManager::load_with_families(&config.font_families(), config.font_size())?;
        Self::with_font_and_config(terminal, pty, font_mgr, config, config_path)
    }

    /// Creates a new `AppState` with terminal, PTY, pre-loaded font manager, configuration, and optional configuration path.
    ///
    /// # Errors
    /// Returns [`FttyError`] if PTY resizing fails.
    pub fn with_font_and_config(
        mut terminal: Terminal,
        pty: Pty,
        font_mgr: FontManager,
        config: Config,
        config_path: Option<PathBuf>,
    ) -> Result<Self, FttyError> {
        let atlas = GlyphAtlas::default();
        let initial_font_size = config.font_size();
        let palette = config.build_palette();
        let default_fg = config.foreground();
        let default_bg = config.background();
        terminal.set_default_colors(default_fg, default_bg);
        terminal.set_palette(palette);
        terminal.allow_osc52_read = config.allow_osc52_read();
        terminal.allow_osc52_write = config.allow_osc52_write();
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

        let mut keyboard = KeyboardHandler::new();
        keyboard.update_keybindings(&config.keybindings);

        Ok(Self {
            terminal,
            pty,
            keyboard,
            wayland,
            font_mgr,
            atlas,
            ime: ImeState::new(),
            kitty_parser: KittyParser::new(),
            vt_parser: crate::parser::VtParser::new(),
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
            pty_registered: false,
            last_activity: std::time::Instant::now(),
            last_trim: std::time::Instant::now(),
            pending_trim: false,
            logical_font_size: initial_font_size,
        })
    }

    #[must_use]
    #[doc(hidden)]
    pub fn should_register_pty(&self) -> bool {
        !self.pty_registered && self.wayland.configured
    }
}

// ---------------------------------------------------------------------------
// Unified Event Loop Runner
// ---------------------------------------------------------------------------

/// Runs the unified calloop event loop until terminal exits.
///
/// # Errors
/// Returns [`FttyError`] if Wayland connection, calloop initialization, or event dispatching fails.
pub fn run_event_loop(mut app_state: AppState) -> Result<(), FttyError> {
    let conn = Connection::connect_to_env().map_err(|e| WaylandError::Connection(e.to_string()))?;
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    let display = conn.display();
    display.get_registry(&qh, ());
    event_queue
        .roundtrip(&mut app_state)
        .map_err(|e| WaylandError::Dispatch(e.to_string()))?;
    conn.flush()
        .map_err(|e| WaylandError::Dispatch(e.to_string()))?;

    run_event_loop_with_connection(app_state, conn, event_queue)
}

/// Runs the unified calloop event loop with an existing Wayland connection and event queue.
///
/// # Errors
/// Returns [`FttyError`] if calloop initialization or event dispatching fails.
pub fn run_event_loop_with_connection(
    mut app_state: AppState,
    conn: Connection,
    event_queue: wayland_client::EventQueue<AppState>,
) -> Result<(), FttyError> {
    let qh = event_queue.handle();

    let mut event_loop: EventLoop<AppState> =
        EventLoop::try_new().map_err(|e| WaylandError::Dispatch(e.to_string()))?;

    // 1. Wayland Event Source
    let wayland_source = WaylandSource::new(conn.clone(), event_queue);
    event_loop
        .handle()
        .insert_source(wayland_source, |(), queue, state: &mut AppState| {
            queue.dispatch_pending(state)
        })
        .map_err(|e| WaylandError::Dispatch(e.to_string()))?;

    // 2. POSIX Signals Event Source
    let signals = Signals::new(&[
        Signal::SIGCHLD,
        Signal::SIGINT,
        Signal::SIGTERM,
        Signal::SIGUSR1,
    ])
    .map_err(|e| WaylandError::Dispatch(e.to_string()))?;
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
        .map_err(|e| WaylandError::Dispatch(e.to_string()))?;

    // 3. Main Event Loop Tick
    while app_state.running {
        let dispatch_timeout = if app_state.terminal.synchronized_output {
            Some(std::time::Duration::from_millis(50))
        } else {
            None
        };

        event_loop
            .dispatch(dispatch_timeout, &mut app_state)
            .map_err(|e| WaylandError::Dispatch(e.to_string()))?;

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

        // Register PTY read source exactly once after initial configure establishes
        // final tiling dimensions and creates the renderer, preventing busy-looping and SIGWINCH restarts.
        if app_state.should_register_pty() {
            app_state.pty_registered = true;
            let pty_master = app_state.pty.try_clone_master()?;
            let pty_source = Generic::new(pty_master, Interest::READ, Mode::Level);
            let pty_qh = qh.clone();
            event_loop
                .handle()
                .insert_source(pty_source, move |_event, _fd, state: &mut AppState| {
                    let mut buf = [0u8; 65536];
                    let mut total_read = 0;
                    loop {
                        match state.pty.read(&mut buf) {
                            Ok(n) if n > 0 => {
                                total_read += n;
                                let incoming = &buf[..n];
                                if state.kitty_parser.is_fast_path(incoming) {
                                    state.process_terminal_output(incoming, &pty_qh);
                                } else {
                                    let mut parser = std::mem::take(&mut state.kitty_parser);
                                    parser.process_stream(incoming, |chunk| match chunk {
                                        crate::kitty::KittyStreamChunk::Text(text) => {
                                            state.process_terminal_output(text, &pty_qh);
                                        }
                                        crate::kitty::KittyStreamChunk::Event(event) => {
                                            if let KittyEvent::Response(resp) = &event {
                                                state.write_pty_blocking(resp);
                                            } else {
                                                state.handle_kitty_event(event);
                                            }
                                        }
                                    });
                                    state.kitty_parser = parser;
                                }

                                state.keyboard.kitty_flags = state.terminal.kitty_keyboard_flags;

                                if total_read >= 65536 {
                                    break;
                                }
                            }
                            Ok(_) => {
                                state.running = false;
                                return Ok(calloop::PostAction::Reregister);
                            }
                            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            Err(_) => {
                                state.running = false;
                                return Ok(calloop::PostAction::Reregister);
                            }
                        }
                    }

                    if total_read > 0 {
                        state.needs_redraw = true;
                        state.last_activity = std::time::Instant::now();
                        state.pending_trim = true;
                        state.update_ime_cursor_area();
                    }
                    Ok(calloop::PostAction::Continue)
                })
                .map_err(|e| WaylandError::Dispatch(e.to_string()))?;
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
            app_state.update_cursor_shape();
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
            let factor = if app_state.wayland.is_fractional_scale_active() {
                app_state.wayland.scale_factor
            } else {
                1.0
            };
            let pad_x = (f64::from(app_state.config.padding_x()) * factor).round() as u16;
            let pad_y = (f64::from(app_state.config.padding_y()) * factor).round() as u16;
            let options = RenderOptions::new(
                [pad_x, pad_y],
                app_state.ime.preedit.as_ref(),
                Some(&app_state.selection),
            )
            .with_hovered_span(app_state.hovered_span);
            let physical_size = [
                (app_state.wayland.width as f64 * factor).round().max(1.0) as u32,
                (app_state.wayland.height as f64 * factor).round().max(1.0) as u32,
            ];
            renderer.render_grid(
                &mut app_state.terminal.grid,
                colors,
                &app_state.font_mgr,
                &mut app_state.atlas,
                physical_size,
                options,
            )?;
            if let Some(surface) = &app_state.wayland.surface {
                if let Some(xdg_surface) = &app_state.wayland.xdg_surface {
                    xdg_surface.set_window_geometry(
                        0,
                        0,
                        app_state.wayland.width as i32,
                        app_state.wayland.height as i32,
                    );
                }
                if let Some(viewport) = &app_state.wayland.viewport {
                    viewport.set_destination(
                        app_state.wayland.width as i32,
                        app_state.wayland.height as i32,
                    );
                }
                app_state.frame_callback = Some(surface.frame(&qh, ()));
            }
            renderer.present()?;
            let _ = app_state
                .wayland
                .transition_window_to(crate::wayland::WindowState::Active);
            app_state.update_ime_cursor_area();
            app_state.needs_redraw = false;
        }

        if app_state.pending_trim
            && app_state.last_activity.elapsed() >= std::time::Duration::from_secs(3)
            && app_state.last_trim.elapsed() >= std::time::Duration::from_secs(10)
        {
            app_state.pending_trim = false;
            app_state.last_trim = std::time::Instant::now();
            crate::alloc::trim_memory();
        }

        let _ = conn.flush();
    }

    app_state.render_error.map_or(Ok(()), Err)
}

pub use crate::alloc::trim_memory;

// --- Geometry ---

#[must_use]
pub fn saturating_u16(value: u32) -> u16 {
    value.min(u32::from(u16::MAX)) as u16
}

#[must_use]
pub fn terminal_size(
    [width, height]: [u32; 2],
    metrics: CellMetrics,
    padding: [u16; 2],
) -> (u16, u16) {
    let usable_w = width.saturating_sub(u32::from(padding[0]) * 2);
    let usable_h = height.saturating_sub(u32::from(padding[1]) * 2);
    let cols = (usable_w / metrics.cell_width.max(1)).clamp(1, u16::MAX as u32) as u16;
    let rows = (usable_h / metrics.cell_height.max(1)).clamp(1, u16::MAX as u32) as u16;
    (cols, rows)
}

impl AppState {
    pub(crate) fn update_font_size(&mut self, new_size: f32) {
        if self.font_mgr.set_font_size(new_size) {
            self.atlas.clear();
            self.terminal.grid.mark_all_dirty();
            if let Some(renderer) = &mut self.renderer {
                renderer.clear_cache();
            }
            let _ = self.resize_terminal();
            self.needs_redraw = true;
        }
    }

    pub(crate) fn resize_terminal(&mut self) -> Result<(), FttyError> {
        let factor = if self.wayland.is_fractional_scale_active() {
            self.wayland.scale_factor
        } else {
            1.0
        };
        let phys_width = (self.wayland.width as f64 * factor).round() as u32;
        let phys_height = (self.wayland.height as f64 * factor).round() as u32;
        let pad_x = (f64::from(self.config.padding_x()) * factor).round() as u16;
        let pad_y = (f64::from(self.config.padding_y()) * factor).round() as u16;
        let (cols, rows) = terminal_size(
            [phys_width, phys_height],
            self.font_mgr.metrics,
            [pad_x, pad_y],
        );
        let viewport_pixels = [
            saturating_u16(phys_width.saturating_sub(u32::from(pad_x) * 2)),
            saturating_u16(phys_height.saturating_sub(u32::from(pad_y) * 2)),
        ];
        if (self.terminal.grid.cols, self.terminal.grid.rows) != (cols as usize, rows as usize) {
            self.selection.clear();
            self.terminal.grid.resize(cols as usize, rows as usize);
        }
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
        if !self.terminal.responses.is_empty() {
            for response in self.terminal.take_responses() {
                self.write_pty_blocking(&response);
            }
        }
        Ok(())
    }

    #[doc(hidden)]
    pub fn configure_renderer(&mut self, connection: &Connection) -> Result<(), FttyError> {
        let logical_size = self
            .pending_size
            .take()
            .unwrap_or([self.wayland.width, self.wayland.height]);
        let factor = if self.wayland.is_fractional_scale_active() {
            self.wayland.scale_factor
        } else {
            1.0
        };
        let physical_size = [
            (logical_size[0] as f64 * factor).round().max(1.0) as u32,
            (logical_size[1] as f64 * factor).round().max(1.0) as u32,
        ];
        if let Some(renderer) = &mut self.renderer {
            renderer.resize(physical_size)?;
        } else {
            let surface = self
                .wayland
                .surface
                .as_ref()
                .ok_or(WaylandError::WindowNotCreated)?;
            let renderer = Renderer::new(surface, connection, physical_size)?;
            // Automatically align font rasterization mode with GPU dual-source blending and output subpixel layout
            let enable_subpixel = self.is_subpixel_enabled();
            let bgr = self.is_bgr_subpixel();
            if self.font_mgr.subpixel != enable_subpixel || self.font_mgr.bgr != bgr {
                self.font_mgr.subpixel = enable_subpixel;
                self.font_mgr.bgr = bgr;
                self.atlas.clear();
            }
            self.renderer = Some(renderer);
        }
        [self.wayland.width, self.wayland.height] = logical_size;
        if let Some(xdg_surface) = &self.wayland.xdg_surface {
            xdg_surface.set_window_geometry(0, 0, logical_size[0] as i32, logical_size[1] as i32);
        }
        if let Some(viewport) = &self.wayland.viewport {
            viewport.set_destination(logical_size[0] as i32, logical_size[1] as i32);
        }
        self.resize_terminal()?;
        self.terminal.synchronized_output = false;
        self.terminal.grid.mark_all_dirty();
        if let Some(renderer) = &mut self.renderer {
            renderer.clear_cache();
        }
        self.needs_redraw = true;
        // A resize must be committed even if the compositor suspended the old frame callback.
        self.frame_callback = None;
        Ok(())
    }

    /// Initializes `wp_fractional_scale_v1` on the window surface if the manager is bound.
    pub fn try_init_fractional_scale(&mut self, qh: &wayland_client::QueueHandle<Self>) {
        self.wayland.init_fractional_scale(qh);
    }

    /// Initializes `wp_viewport` on the window surface if viewporter is bound.
    pub fn try_init_viewport(&mut self, qh: &wayland_client::QueueHandle<Self>) {
        self.wayland.init_viewport(qh);
    }

    /// Handles Wayland fractional scale updates from `wp_fractional_scale_v1`.
    pub fn handle_preferred_scale(&mut self, scale_120: u32, connection: &Connection) {
        if !self.wayland.is_fractional_scale_active() {
            return;
        }
        let factor = scale_120 as f64 / 120.0;
        if (self.wayland.scale_factor - factor).abs() > 0.001 {
            self.wayland.scale_factor = factor;
            self.wayland.preferred_scale_120 = scale_120;

            let scaled_font_size = (self.logical_font_size * factor as f32).clamp(
                crate::font::MIN_FONT_SIZE,
                crate::font::MAX_RASTER_FONT_SIZE,
            );
            self.update_font_size(scaled_font_size);

            if let Err(e) = self.configure_renderer(connection) {
                self.render_error = Some(e);
                self.running = false;
            }
        }
    }
}

// --- Actions ---

static ACTIVE_PIPELINES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const MAX_CONCURRENT_PIPELINES: usize = 8;

struct PipelineGuard;
impl Drop for PipelineGuard {
    fn drop(&mut self) {
        ACTIVE_PIPELINES.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl AppState {
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

        let active_subpixel = self.is_subpixel_enabled();
        let active_bgr = self.is_bgr_subpixel();
        let maybe_new_font = if families_changed {
            match FontManager::load_with_families_and_subpixel(
                &new_config.font_families(),
                new_config.font_size(),
                active_subpixel,
            ) {
                Ok(mut mgr) => {
                    mgr.bgr = active_bgr;
                    Some(mgr)
                }
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
        self.terminal.set_palette(self.palette);
        self.terminal.allow_osc52_read = new_config.allow_osc52_read();
        self.terminal.allow_osc52_write = new_config.allow_osc52_write();
        self.terminal.grid.cursor.shape = new_config.cursor_shape();
        self.keyboard.update_keybindings(&new_config.keybindings);
        self.config = new_config;

        self.terminal.grid.mark_all_dirty();
        if let Some(renderer) = &mut self.renderer {
            renderer.clear_cache();
        }

        self.logical_font_size = new_font_size;
        let factor = if self.wayland.is_fractional_scale_active() {
            self.wayland.scale_factor
        } else {
            1.0
        };
        let scaled_font_size = (self.logical_font_size * factor as f32).clamp(
            crate::font::MIN_FONT_SIZE,
            crate::font::MAX_RASTER_FONT_SIZE,
        );

        if let Some(new_font_mgr) = maybe_new_font {
            self.font_mgr = new_font_mgr;
            self.atlas.clear();
            self.update_font_size(scaled_font_size);
            let _ = self.resize_terminal();
        } else if size_changed {
            self.update_font_size(scaled_font_size);
        } else if padding_changed {
            let _ = self.resize_terminal();
        }

        self.needs_redraw = true;
        crate::alloc::trim_memory();
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
            KeyAction::PromptPrev => {
                self.terminal.grid.scroll_to_prompt_prev();
                self.needs_redraw = true;
            }
            KeyAction::PromptNext => {
                self.terminal.grid.scroll_to_prompt_next();
                self.needs_redraw = true;
            }
            KeyAction::FontIncrease => {
                let new_size = (self.logical_font_size + 1.0).min(crate::font::MAX_FONT_SIZE);
                self.set_logical_font_size(new_size);
            }
            KeyAction::FontDecrease => {
                let new_size = (self.logical_font_size - 1.0).max(crate::font::MIN_FONT_SIZE);
                self.set_logical_font_size(new_size);
            }
            KeyAction::FontReset => {
                let default_size = self.config.font_size();
                self.set_logical_font_size(default_size);
            }
            KeyAction::ClipboardCopy => {
                self.copy_selection(qh);
            }
            KeyAction::ClipboardPaste | KeyAction::PrimaryPaste => {
                self.paste_clipboard(conn);
            }
            KeyAction::PipeVisible(cmd) => {
                let text = self.terminal.grid.extract_visible_text();
                Self::spawn_pipe_async(cmd, text);
            }
            KeyAction::PipeScrollback(cmd) => {
                let text = self.terminal.grid.extract_scrollback_text();
                Self::spawn_pipe_async(cmd, text);
            }
            KeyAction::PipeSelection(cmd) => {
                let text = self.selection.extract_text(&self.terminal.grid);
                if !text.is_empty() {
                    Self::spawn_pipe_async(cmd, text);
                }
            }
        }
    }

    pub(crate) fn spawn_pipe_async(cmd: Vec<String>, text: String) {
        if cmd.is_empty() {
            return;
        }
        if ACTIVE_PIPELINES.load(std::sync::atomic::Ordering::Relaxed) >= MAX_CONCURRENT_PIPELINES {
            eprintln!(
                "ftty: pipeline concurrency limit reached ({MAX_CONCURRENT_PIPELINES}), ignoring request"
            );
            return;
        }
        ACTIVE_PIPELINES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::thread::spawn(move || {
            let _guard = PipelineGuard;
            let (program, args) = match cmd.split_first() {
                Some((prog, args)) => (prog, args),
                None => return,
            };
            let mut child = match std::process::Command::new(program)
                .args(args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .spawn()
            {
                Ok(child) => child,
                Err(e) => {
                    eprintln!("ftty: failed to spawn pipe command '{program}': {e}");
                    return;
                }
            };

            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                if let Err(e) = stdin.write_all(text.as_bytes()).and_then(|_| stdin.flush())
                    && e.kind() != std::io::ErrorKind::BrokenPipe
                {
                    eprintln!("ftty: pipeline write error for '{program}': {e}");
                }
                drop(stdin);
            }
            let _ = child.wait();
        });
    }

    /// Sets the logical font size and updates font metrics scaled by the active display factor.
    pub fn set_logical_font_size(&mut self, size: f32) {
        self.logical_font_size = size;
        let factor = if self.wayland.is_fractional_scale_active() {
            self.wayland.scale_factor
        } else {
            1.0
        };
        let scaled_size = (size * factor as f32).clamp(
            crate::font::MIN_FONT_SIZE,
            crate::font::MAX_RASTER_FONT_SIZE,
        );
        self.update_font_size(scaled_size);
    }
}

// --- Kitty Event Processing ---

impl AppState {
    pub(crate) fn process_terminal_output(&mut self, text: &[u8], qh: &QueueHandle<Self>) {
        if text.is_empty() {
            return;
        }
        self.terminal.advance_bytes(&mut self.vt_parser, text);
        if !self.terminal.responses.is_empty() {
            for response in self.terminal.take_responses() {
                self.write_pty_blocking(&response);
            }
        }
        if let Some(pending) = self.terminal.take_pending_clipboard() {
            match pending {
                Some(text) => self.set_clipboard_text(text, Some(qh)),
                None => {
                    self.clipboard_text = None;
                    self.terminal.set_clipboard_content(None);
                    if let Some(device) = &self.wayland.data_device {
                        device.set_selection(None, self.last_serial);
                    }
                }
            }
        }
        self.keyboard.kitty_flags = self.terminal.kitty_keyboard_flags;
        if self.terminal.palette_dirty {
            self.terminal.palette_dirty = false;
            self.palette = self.terminal.palette;
            self.default_fg = self.terminal.default_fg;
            self.default_bg = self.terminal.default_bg;
            if let Some(renderer) = &mut self.renderer {
                renderer.clear_cache();
            }
            self.terminal.grid.mark_all_dirty();
            self.needs_redraw = true;
        }
        if self.terminal.title_dirty {
            self.terminal.title_dirty = false;
            self.wayland.title = self.terminal.title.clone();
            if let Some(toplevel) = &self.wayland.xdg_toplevel {
                let title = if self.wayland.title.is_empty() {
                    "ftty"
                } else {
                    &self.wayland.title
                };
                toplevel.set_title(title.to_string());
            }
        }
        if self.config.auto_scroll() && !self.terminal.grid.is_alt_screen() {
            self.terminal.grid.scroll_viewport_bottom();
        }
    }

    pub(crate) fn handle_kitty_event(&mut self, event: KittyEvent) {
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

                let should_place = command.action == KittyAction::TransmitAndDisplay
                    || command.action == KittyAction::TransmitAndDisplayWithResponse;

                if command.is_virtual {
                    self.terminal
                        .grid
                        .virtual_placements
                        .insert(image_id, (cols, rows));
                } else if should_place {
                    let abs_line =
                        self.terminal.grid.scrollback.len() + self.terminal.grid.cursor.row;
                    self.terminal.grid.add_placement(ImagePlacement {
                        image_id,
                        placement_id,
                        line: abs_line,
                        col: self.terminal.grid.cursor.col,
                        cols,
                        rows,
                        src_x: command.src_x.unwrap_or(0),
                        src_y: command.src_y.unwrap_or(0),
                        src_w: command.src_w,
                        src_h: command.src_h,
                        offset_x: command.offset_x,
                        offset_y: command.offset_y,
                        z_index: command.z_index,
                    });

                    if !command.do_not_move_cursor {
                        self.terminal.grid.cursor.col = (self.terminal.grid.cursor.col + cols)
                            .min(self.terminal.grid.cols.saturating_sub(1));
                        if rows > 1 {
                            let scroll_top = self.terminal.grid.scroll_region_top;
                            let scroll_bottom = self.terminal.grid.scroll_region_bottom;
                            let cur_row = self.terminal.grid.cursor.row;
                            if cur_row >= scroll_top && cur_row <= scroll_bottom {
                                let target_row = cur_row + rows - 1;
                                if target_row > scroll_bottom {
                                    let scroll_amount = target_row - scroll_bottom;
                                    self.terminal
                                        .grid
                                        .scroll_up_with_bg(scroll_amount, self.terminal.active_bg);
                                    self.terminal.grid.cursor.row = scroll_bottom;
                                } else {
                                    self.terminal.grid.cursor.row = target_row;
                                }
                            } else {
                                self.terminal.grid.cursor.row = (cur_row + rows - 1)
                                    .min(self.terminal.grid.rows.saturating_sub(1));
                            }
                        }
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
                        src_x: command.src_x.unwrap_or(0),
                        src_y: command.src_y.unwrap_or(0),
                        src_w: command.src_w,
                        src_h: command.src_h,
                        offset_x: command.offset_x,
                        offset_y: command.offset_y,
                        z_index: command.z_index,
                    });

                    if !command.do_not_move_cursor {
                        self.terminal.grid.cursor.col = (self.terminal.grid.cursor.col + cols)
                            .min(self.terminal.grid.cols.saturating_sub(1));
                        if rows > 1 {
                            let scroll_top = self.terminal.grid.scroll_region_top;
                            let scroll_bottom = self.terminal.grid.scroll_region_bottom;
                            let cur_row = self.terminal.grid.cursor.row;
                            if cur_row >= scroll_top && cur_row <= scroll_bottom {
                                let target_row = cur_row + rows - 1;
                                if target_row > scroll_bottom {
                                    let scroll_amount = target_row - scroll_bottom;
                                    self.terminal
                                        .grid
                                        .scroll_up_with_bg(scroll_amount, self.terminal.active_bg);
                                    self.terminal.grid.cursor.row = scroll_bottom;
                                } else {
                                    self.terminal.grid.cursor.row = target_row;
                                }
                            } else {
                                self.terminal.grid.cursor.row = (cur_row + rows - 1)
                                    .min(self.terminal.grid.rows.saturating_sub(1));
                            }
                        }
                    }
                }
                let wants_ack = command.action == KittyAction::TransmitAndDisplayWithResponse
                    || (command.id_explicit && command.quiet == 0);
                if wants_ack {
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

pub mod geometry {
    pub use super::{saturating_u16, terminal_size};
}
pub mod actions {}
pub mod kitty {}
