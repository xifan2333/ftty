//! Unified calloop single-threaded event loop multiplexing Wayland, PTY I/O, and POSIX signals.

use std::path::PathBuf;

use calloop::generic::Generic;
use calloop::signals::{Signal, Signals};
use calloop::{EventLoop, Interest, Mode};
use calloop_wayland_source::WaylandSource;

use wayland_client::protocol::wl_callback::WlCallback;
use wayland_client::{Connection, QueueHandle};

use crate::config::Config;
use crate::error::{FttyError, WaylandError};
use crate::font::{CellMetrics, FontManager, GlyphAtlas};
use crate::input::ime::ImeState;
use crate::input::{KeyAction, KeyboardHandler};
use crate::render::{ColorScheme, RenderOptions, Renderer};
use crate::session::TerminalSession;
use crate::wayland::WaylandState;

/// Shared application state passed to all calloop sources and Wayland event dispatches.
pub struct AppState {
    pub session: TerminalSession,
    // Destroy the native EGL window before the Wayland surface handles.
    pub renderer: Option<Renderer>,
    pub pointer: crate::input::PointerController,
    pub keyboard: KeyboardHandler,
    pub wayland: WaylandState,
    pub font_mgr: FontManager,
    pub atlas: GlyphAtlas,
    pub ime: ImeState,
    pub config: Config,
    pub config_path: Option<PathBuf>,
    pub last_serial: u32,
    pub pointer_serial: u32,
    pub clipboard_text: Option<String>,
    pub pending_offers: Vec<crate::wayland::OfferData>,
    pub running: bool,
    pub needs_redraw: bool,
    #[doc(hidden)]
    pub frame_callback: Option<WlCallback>,
    #[doc(hidden)]
    pub pending_size: Option<[u32; 2]>,
    /// Set by an `xdg_surface.configure`; the resize is applied once the queue is drained so a
    /// burst of configures collapses into a single, final size.
    pub(crate) configure_pending: bool,
    pub(crate) render_error: Option<FttyError>,
    #[doc(hidden)]
    pub pty_registered: bool,
    pub(crate) last_activity: std::time::Instant,
    pub(crate) last_trim: std::time::Instant,
    pub(crate) pending_trim: bool,
    pub logical_font_size: f32,
    pub(crate) last_ime_cursor_rect: std::cell::Cell<Option<(i32, i32, i32, i32)>>,
}

impl AppState {
    /// Creates a new `AppState` with default configuration path.
    ///
    /// # Errors
    /// Returns [`FttyError`] if font discovery or configuration loading fails.
    pub fn new(session: TerminalSession) -> Result<Self, FttyError> {
        Self::with_config(session, None)
    }

    /// Creates a new `AppState` with terminal session and optional custom configuration path.
    ///
    /// # Errors
    /// Returns [`FttyError`] if font discovery or configuration loading fails.
    pub fn with_config(
        session: TerminalSession,
        config_path: Option<PathBuf>,
    ) -> Result<Self, FttyError> {
        let config = if cfg!(test) && config_path.is_none() {
            Config::default()
        } else {
            Config::load_from_path_or_default(config_path.as_deref())?
        };
        Self::with_loaded_config(session, config, config_path)
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

    /// Creates a new `AppState` with terminal session, pre-loaded configuration, and optional configuration path.
    ///
    /// # Errors
    /// Returns [`FttyError`] if font discovery fails.
    pub fn with_loaded_config(
        session: TerminalSession,
        config: Config,
        config_path: Option<PathBuf>,
    ) -> Result<Self, FttyError> {
        let font_mgr =
            FontManager::load_with_families(&config.font_families(), config.font_size())?;
        Self::with_font_and_config(session, font_mgr, config, config_path)
    }

    /// Creates a new `AppState` with terminal session, pre-loaded font manager, configuration, and optional configuration path.
    ///
    /// # Errors
    /// Returns [`FttyError`] if PTY resizing fails.
    pub fn with_font_and_config(
        mut session: TerminalSession,
        font_mgr: FontManager,
        config: Config,
        config_path: Option<PathBuf>,
    ) -> Result<Self, FttyError> {
        let atlas = GlyphAtlas::default();
        let initial_font_size = config.font_size();
        session
            .terminal
            .set_default_colors(config.foreground(), config.background());
        session.terminal.set_palette(config.build_palette());
        session.terminal.allow_osc52_read = config.allow_osc52_read();
        session.terminal.allow_osc52_write = config.allow_osc52_write();
        session.terminal.grid.cursor.shape = config.cursor_shape();
        session.terminal.grid.max_scrollback = config.scrollback_lines();

        let mut wayland = WaylandState::new();
        let pad_x = u32::from(config.padding_x());
        let pad_y = u32::from(config.padding_y());
        wayland.width = (session.terminal.grid.cols as u32)
            .saturating_mul(font_mgr.metrics.cell_width)
            .saturating_add(pad_x * 2)
            .clamp(100, i32::MAX as u32);
        wayland.height = (session.terminal.grid.rows as u32)
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
        session.resize(
            session.terminal.grid.cols as u16,
            session.terminal.grid.rows as u16,
            cell_pixels,
            viewport_pixels,
        )?;

        let mut keyboard = KeyboardHandler::new();
        keyboard.update_keybindings(&config.keybindings);

        Ok(Self {
            session,
            renderer: None,
            pointer: crate::input::PointerController::new(),
            keyboard,
            wayland,
            font_mgr,
            atlas,
            ime: ImeState::new(),
            config,
            config_path,
            last_serial: 0,
            pointer_serial: 0,
            clipboard_text: None,
            pending_offers: Vec::new(),
            running: true,
            needs_redraw: true,
            frame_callback: None,
            pending_size: None,
            configure_pending: false,
            render_error: None,
            pty_registered: false,
            last_activity: std::time::Instant::now(),
            last_trim: std::time::Instant::now(),
            pending_trim: false,
            logical_font_size: initial_font_size,
            last_ime_cursor_rect: std::cell::Cell::new(None),
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
                    if !state.session.pty.is_alive() {
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
        let dispatch_timeout = if app_state.session.terminal.synchronized_output {
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
            let pty_master = app_state.session.pty.try_clone_master()?;
            let pty_source = Generic::new(pty_master, Interest::READ, Mode::Level);
            let pty_qh = qh.clone();
            event_loop
                .handle()
                .insert_source(pty_source, move |_event, _fd, state: &mut AppState| {
                    let cw = state.font_mgr.metrics.cell_width as f32;
                    let ch = state.font_mgr.metrics.cell_height as f32;
                    let auto_scroll = state.config.auto_scroll();

                    let outcome = match state.session.read_and_process_pty(cw, ch, auto_scroll) {
                        Ok(o) => o,
                        Err(_) => {
                            state.running = false;
                            return Ok(calloop::PostAction::Reregister);
                        }
                    };

                    if outcome.process_exited {
                        state.running = false;
                        return Ok(calloop::PostAction::Reregister);
                    }

                    state.keyboard.kitty_flags = outcome.kitty_keyboard_flags;

                    if outcome.theme_changed {
                        if let Some(renderer) = &mut state.renderer {
                            renderer.clear_cache();
                        }
                        state.session.terminal.grid.mark_all_dirty();
                        state.needs_redraw = true;
                    }

                    if let Some(title) = outcome.title_update {
                        state.wayland.title = title.clone();
                        if let Some(toplevel) = &state.wayland.xdg_toplevel {
                            let display_title = if title.is_empty() { "ftty" } else { &title };
                            toplevel.set_title(display_title.to_string());
                        }
                    }

                    if let Some(clip) = outcome.clipboard_update {
                        match clip {
                            Some(text) => state.set_clipboard_text(text, Some(&pty_qh)),
                            None => {
                                state.clipboard_text = None;
                                state.session.terminal.set_clipboard_content(None);
                                if let Some(device) = &state.wayland.data_device {
                                    device.set_selection(None, state.last_serial);
                                }
                            }
                        }
                    }

                    if outcome.needs_redraw {
                        state.needs_redraw = true;
                        state.last_activity = std::time::Instant::now();
                        state.pending_trim = true;
                        state.update_ime_cursor_area();
                    }
                    Ok(calloop::PostAction::Continue)
                })
                .map_err(|e| WaylandError::Dispatch(e.to_string()))?;
        }

        let sync_active = app_state.session.is_synchronized_output_active();

        if app_state.needs_redraw && !sync_active && app_state.frame_callback.is_none() {
            app_state.update_hover_state();
            app_state.update_cursor_shape();
        }

        if app_state.needs_redraw
            && !sync_active
            && app_state.frame_callback.is_none()
            && let Some(renderer) = &mut app_state.renderer
        {
            let colors = ColorScheme::from(&app_state.session.terminal.theme);
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
                Some(&app_state.pointer.selection),
            )
            .with_hovered_span(app_state.pointer.hovered_span);
            let physical_size = [
                (app_state.wayland.width as f64 * factor).round().max(1.0) as u32,
                (app_state.wayland.height as f64 * factor).round().max(1.0) as u32,
            ];
            renderer.render_grid(
                &mut app_state.session.terminal.grid,
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
            self.session.terminal.grid.mark_all_dirty();
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
        if (
            self.session.terminal.grid.cols,
            self.session.terminal.grid.rows,
        ) != (cols as usize, rows as usize)
        {
            self.pointer.clear_selection();
        }
        self.session.resize(
            cols,
            rows,
            [
                saturating_u16(self.font_mgr.metrics.cell_width),
                saturating_u16(self.font_mgr.metrics.cell_height),
            ],
            viewport_pixels,
        )?;
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
        self.session.terminal.synchronized_output = false;
        self.session.terminal.grid.mark_all_dirty();
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
    /// Writes bytes to the PTY master via the session.
    pub fn write_pty_blocking(&mut self, bytes: &[u8]) {
        self.session.write_input_bytes(bytes);
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
        self.session.terminal.grid.max_scrollback = max_sb;
        if self.session.terminal.grid.scrollback.len() > max_sb {
            let overflow = self.session.terminal.grid.scrollback.len() - max_sb;
            for _ in 0..overflow {
                self.session.terminal.grid.scrollback.pop_front();
            }
            self.session.terminal.grid.viewport_offset =
                self.session.terminal.grid.viewport_offset.min(max_sb);
        }

        let padding_changed = self.config.padding_x() != new_config.padding_x()
            || self.config.padding_y() != new_config.padding_y();

        self.session
            .terminal
            .set_default_colors(new_config.foreground(), new_config.background());
        self.session
            .terminal
            .set_palette(new_config.build_palette());
        self.session.terminal.allow_osc52_read = new_config.allow_osc52_read();
        self.session.terminal.allow_osc52_write = new_config.allow_osc52_write();
        self.session.terminal.grid.cursor.shape = new_config.cursor_shape();
        self.keyboard.update_keybindings(&new_config.keybindings);
        self.config = new_config;

        self.session.terminal.grid.mark_all_dirty();
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
                self.session
                    .terminal
                    .grid
                    .scroll_viewport_up(self.session.terminal.grid.rows);
                self.needs_redraw = true;
            }
            KeyAction::ScrollbackDownPage => {
                self.session
                    .terminal
                    .grid
                    .scroll_viewport_down(self.session.terminal.grid.rows);
                self.needs_redraw = true;
            }
            KeyAction::ScrollbackUpLine => {
                self.session.terminal.grid.scroll_viewport_up(1);
                self.needs_redraw = true;
            }
            KeyAction::ScrollbackDownLine => {
                self.session.terminal.grid.scroll_viewport_down(1);
                self.needs_redraw = true;
            }
            KeyAction::ScrollbackHome => {
                self.session.terminal.grid.scroll_viewport_top();
                self.needs_redraw = true;
            }
            KeyAction::ScrollbackEnd => {
                self.session.terminal.grid.scroll_viewport_bottom();
                self.needs_redraw = true;
            }
            KeyAction::PromptPrev => {
                self.session.terminal.grid.scroll_to_prompt_prev();
                self.needs_redraw = true;
            }
            KeyAction::PromptNext => {
                self.session.terminal.grid.scroll_to_prompt_next();
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
                let text = self.session.terminal.grid.extract_visible_text();
                Self::spawn_pipe_async(cmd, text);
            }
            KeyAction::PipeScrollback(cmd) => {
                let text = self.session.terminal.grid.extract_scrollback_text();
                Self::spawn_pipe_async(cmd, text);
            }
            KeyAction::PipeSelection(cmd) => {
                let text = self
                    .pointer
                    .selection
                    .extract_text(&self.session.terminal.grid);
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

#[cfg(test)]
mod tests {


use std::io::Write;
use std::time::Duration;

use crate::color::Rgb;
use crate::config::Config;
use crate::error::FttyError;

fn new_test_app(term: Terminal, pty: Pty) -> Result<AppState, FttyError> {
    let session = crate::TerminalSession::new(term, pty);
    AppState::with_loaded_config(session, Config::default(), None)
}
use crate::event_loop::{AppState, terminal_size};
use crate::font::CellMetrics;
use crate::input::KeyAction;
use crate::input::selection::{Selection, SelectionPoint, SelectionType};
use crate::parser::Terminal;
use crate::pty::Pty;
use crate::render::HoveredHyperlinkSpan;

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
    let app = new_test_app(term, pty).expect("AppState new");

    assert!(app.running);
    assert_eq!(app.session.terminal.grid.cols, 80);
    assert_eq!(app.session.terminal.grid.rows, 24);
    assert!(app.font_mgr.metrics.cell_width > 0);
    assert!(app.font_mgr.metrics.cell_height > 0);
    assert!(!app.wayland.configured);
}

#[test]
fn test_pty_and_terminal_roundtrip() {
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app = new_test_app(term, pty).expect("AppState new");

    // Send a command to shell via PTY
    app.session
        .pty
        .write_all(b"echo ftty_test_ok\n")
        .expect("write to pty");

    // Wait briefly and read output back into terminal
    let mut received = false;
    let cw = app.font_mgr.metrics.cell_width as f32;
    let ch = app.font_mgr.metrics.cell_height as f32;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(20));
        if let Ok(_outcome) = app.session.read_and_process_pty(cw, ch, false) {
            let full_screen: String = app
                .session
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
    let session = crate::TerminalSession::new(term, pty);
    let mut app =
        AppState::with_config(session, Some(config_path.clone())).expect("AppState with_config");

    assert_eq!(app.session.terminal.theme.default_bg(), Rgb::new(0, 0, 0));
    assert_eq!(
        app.session.terminal.theme.default_fg(),
        Rgb::new(255, 255, 255)
    );
    assert_eq!(app.session.terminal.grid.cursor.shape, CursorShape::Block);

    // Update config file with new colors, cursor, and padding
    std::fs::write(
        &config_path,
        r##"
        [window]
        padding = [20, 20]

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

    assert_eq!(
        app.session.terminal.theme.default_bg(),
        Rgb::new(18, 52, 86)
    );
    assert_eq!(app.session.terminal.theme.default_fg(), Rgb::new(255, 0, 0));
    assert_eq!(
        app.session.terminal.grid.cursor.shape,
        CursorShape::Underline
    );
    assert_eq!(app.config.padding_x(), 20);
    assert_eq!(app.config.padding_y(), 20);
    assert!(app.needs_redraw);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_combined_reload_updates_size_and_padding() {
    let temp_dir = std::env::temp_dir().join(format!("ftty_reload_comb_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&temp_dir);
    let config_path = temp_dir.join("ftty.toml");

    std::fs::write(
        &config_path,
        r##"
        [font]
        size = 11.0

        [window]
        padding = [10, 10]
        "##,
    )
    .unwrap();

    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let session = crate::TerminalSession::new(term, pty);
    let mut app =
        AppState::with_config(session, Some(config_path.clone())).expect("AppState with_config");

    assert_eq!(app.font_mgr.font_size(), 11.0);
    assert_eq!(app.config.padding_x(), 10);

    // Concurrently change font size and window padding in one reload
    std::fs::write(
        &config_path,
        r##"
        [font]
        size = 13.0

        [window]
        padding = [25, 25]
        "##,
    )
    .unwrap();

    app.reload_config();

    assert_eq!(app.font_mgr.font_size(), 13.0);
    assert_eq!(app.config.padding_x(), 25);
    assert_eq!(app.config.padding_y(), 25);
    assert!(app.needs_redraw);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_failed_reload_preserves_state() {
    use crate::grid::CursorShape;

    let temp_dir = std::env::temp_dir().join(format!("ftty_reload_fail_{}", std::process::id()));
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
    let session = crate::TerminalSession::new(term, pty);
    let mut app =
        AppState::with_config(session, Some(config_path.clone())).expect("AppState with_config");

    // Populate scrollback with lines
    for _ in 0..10 {
        app.session
            .terminal
            .grid
            .scrollback
            .push_back(crate::grid::Row::new(80));
    }
    assert_eq!(app.session.terminal.grid.scrollback.len(), 10);

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
    assert_eq!(app.session.terminal.grid.scrollback.len(), 10);
    assert_eq!(app.session.terminal.grid.max_scrollback, 100);
    assert_eq!(app.session.terminal.theme.default_bg(), Rgb::new(0, 0, 0));
    assert_eq!(
        app.session.terminal.theme.default_fg(),
        Rgb::new(255, 255, 255)
    );
    assert_eq!(app.session.terminal.grid.cursor.shape, CursorShape::Block);
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
    let mut app = new_test_app(term, pty).expect("AppState new");

    // Populate lines and scrollback
    for _ in 0..50 {
        app.session.terminal.grid.write_char(
            'A',
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
        app.session.terminal.grid.newline();
    }

    assert_eq!(app.session.terminal.grid.viewport_offset(), 0);

    // Page Up
    app.handle_key_action(KeyAction::ScrollbackUpPage, None, None);
    assert_eq!(app.session.terminal.grid.viewport_offset(), 24);

    // Scroll to Top
    app.handle_key_action(KeyAction::ScrollbackHome, None, None);
    assert_eq!(
        app.session.terminal.grid.viewport_offset(),
        app.session.terminal.grid.scrollback.len()
    );

    // Line Down
    let top = app.session.terminal.grid.viewport_offset();
    app.handle_key_action(KeyAction::ScrollbackDownLine, None, None);
    assert_eq!(app.session.terminal.grid.viewport_offset(), top - 1);

    // Scroll to Bottom
    app.handle_key_action(KeyAction::ScrollbackEnd, None, None);
    assert_eq!(app.session.terminal.grid.viewport_offset(), 0);

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
    let mut app = new_test_app(term, pty).expect("AppState new");

    for c in "copied_text".chars() {
        app.session.terminal.grid.write_char(
            c,
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
    }

    app.pointer.selection = Selection::new(
        SelectionPoint::new(0, 0),
        SelectionPoint::new(0, 10),
        SelectionType::Simple,
    );

    app.copy_selection(None);
    assert_eq!(app.clipboard_text, Some("copied_text".to_string()));

    app.paste_clipboard(None);
}

#[test]
fn test_pipe_visible_action_execution() {
    use crate::color::Color;
    use crate::grid::CellFlags;

    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app = new_test_app(term, pty).expect("AppState new");

    for c in "ftty_pipe_test".chars() {
        app.session.terminal.grid.write_char(
            c,
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
    }

    let temp_file = std::env::temp_dir().join(format!("ftty_pipe_out_{}", std::process::id()));
    let out_path = temp_file.to_string_lossy().to_string();

    let cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!("cat > '{out_path}'"),
    ];
    app.handle_key_action(KeyAction::PipeVisible(cmd), None, None);

    let mut success = false;
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(20));
        if let Ok(content) = std::fs::read_to_string(&temp_file)
            && content.contains("ftty_pipe_test")
        {
            success = true;
            break;
        }
    }
    let _ = std::fs::remove_file(&temp_file);
    assert!(success, "piped output must contain 'ftty_pipe_test'");
}

#[test]
fn x11_buttons_map_to_protocol_indexes() {
    use crate::input::pointer::x11_button_index;

    assert_eq!(x11_button_index(0x110), Some(0)); // BTN_LEFT -> Left 0
    assert_eq!(x11_button_index(0x111), Some(2)); // BTN_RIGHT -> Right 2
    assert_eq!(x11_button_index(0x112), Some(1)); // BTN_MIDDLE -> Middle 1
    assert_eq!(x11_button_index(0x113), None);
}

#[test]
fn mouse_reports_are_forwarded_only_when_tracking_is_enabled() {
    use crate::input::mouse::{MouseEncoding, MouseTracking};

    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app = new_test_app(term, pty).expect("AppState new");
    let cw = f64::from(app.font_mgr.metrics.cell_width);
    let ch = f64::from(app.font_mgr.metrics.cell_height);
    app.pointer.mouse_pos = [cw * 2.5, ch * 1.5];

    // Tracking disabled: the pointer stays available for local text selection.
    assert!(app.mouse_report_bytes(0, true, false).is_none());

    app.session.terminal.mouse.tracking = MouseTracking::Drag;
    app.session.terminal.mouse.encoding = MouseEncoding::Sgr;
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
    let app = new_test_app(term, pty).expect("AppState new");

    let cw = f64::from(app.font_mgr.metrics.cell_width);
    let ch = f64::from(app.font_mgr.metrics.cell_height);

    let (line, screen_row, col) = app.cell_at_pointer(cw * 5.5, ch * 3.5);
    assert_eq!(col, 5);
    assert_eq!(screen_row, 3);
    assert_eq!(line, 3);
}

#[test]
fn test_fractional_scale_pointer_and_state() {
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app = new_test_app(term, pty).expect("AppState new");

    assert_eq!(app.wayland.scale_factor, 1.0);
    assert_eq!(app.wayland.preferred_scale_120, 120);

    app.wayland.scale_factor = 1.5;
    app.wayland.preferred_scale_120 = 180;

    let scale = 1.5;
    let logical_cw = f64::from(app.font_mgr.metrics.cell_width) / scale;
    let logical_ch = f64::from(app.font_mgr.metrics.cell_height) / scale;

    let (line, screen_row, col) = app.cell_at_pointer(logical_cw * 4.5, logical_ch * 2.5);
    assert_eq!(col, 4);
    assert_eq!(screen_row, 2);
    assert_eq!(line, 2);
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
    let session = crate::TerminalSession::new(term, pty);
    let mut app =
        AppState::with_config(session, Some(config_path.clone())).expect("AppState with_config");

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
    // Subpixel and BGR output attributes must be preserved across family reload
    assert_eq!(app.font_mgr.subpixel, app.is_subpixel_enabled());
    assert_eq!(app.font_mgr.bgr, app.is_bgr_subpixel());

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_update_hover_state_and_pointer_shape() {
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app = new_test_app(term, pty).expect("AppState new");

    // Intern a real URL so hyperlink_id 1 is resolvable
    let id = app
        .session
        .terminal
        .get_or_intern_hyperlink("https://example.com".to_string());
    assert_eq!(id, 1);

    // Cells at cols 4..=6 have hyperlink ID 1
    for col in 4..=6 {
        app.session.terminal.grid.lines[0].cells[col].set_hyperlink_id(Some(1));
    }
    // Cell at col 8 has hyperlink ID 0 (invalid/exhausted sentinel)
    app.session.terminal.grid.lines[0].cells[8].set_hyperlink_id(Some(0));

    let cw = f64::from(app.font_mgr.metrics.cell_width);
    let ch = f64::from(app.font_mgr.metrics.cell_height);

    // If pointer is not in surface, hover must be None
    app.pointer.pointer_in_surface = false;
    app.pointer.mouse_pos = [cw * 5.5, ch * 0.5];
    app.update_hover_state();
    assert_eq!(app.pointer.hovered_span, None);

    // Enter surface
    app.pointer.pointer_in_surface = true;

    // Pointer over cell (row 0, col 0) has no hyperlink
    app.pointer.mouse_pos = [cw * 0.5, ch * 0.5];
    app.update_hover_state();
    assert_eq!(app.pointer.hovered_span, None);

    // Move pointer over cell (row 0, col 5): detects contiguous span [4..=6]
    app.pointer.mouse_pos = [cw * 5.5, ch * 0.5];
    app.needs_redraw = false;
    app.update_hover_state();
    assert_eq!(
        app.pointer.hovered_span,
        Some(HoveredHyperlinkSpan {
            line: 0,
            start_col: 4,
            end_col: 6,
        })
    );
    assert!(app.needs_redraw);

    // Pointer over cell with sentinel id 0 (pool exhaustion) must produce no hover
    app.pointer.mouse_pos = [cw * 8.5, ch * 0.5];
    app.needs_redraw = false;
    app.update_hover_state();
    assert_eq!(app.pointer.hovered_span, None);
    assert!(app.needs_redraw);

    // Back to col 5
    app.pointer.mouse_pos = [cw * 5.5, ch * 0.5];
    app.update_hover_state();
    assert!(app.pointer.hovered_span.is_some());

    // If mouse is pressed (dragging selection), hover is suppressed
    app.pointer.mouse_pressed = true;
    app.needs_redraw = false;
    app.update_hover_state();
    assert_eq!(app.pointer.hovered_span, None);
    assert!(app.needs_redraw);

    // When mouse is released, hover is restored
    app.pointer.mouse_pressed = false;
    app.update_hover_state();
    assert_eq!(
        app.pointer.hovered_span,
        Some(HoveredHyperlinkSpan {
            line: 0,
            start_col: 4,
            end_col: 6,
        })
    );
}

#[test]
fn test_app_state_with_loaded_config() {
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let session = crate::TerminalSession::new(term, pty);
    let config = crate::config::Config::default();
    let app = AppState::with_loaded_config(session, config, None).expect("with_loaded_config");
    assert_eq!(app.session.terminal.grid.cols, 80);
    assert_eq!(app.session.terminal.grid.rows, 24);
    assert!(app.font_mgr.metrics.cell_width > 0);
}

#[test]
fn test_app_state_with_font_and_config() {
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let session = crate::TerminalSession::new(term, pty);
    let config = crate::config::Config::default();
    let font_mgr = crate::font::FontManager::load(14.0).expect("load font");
    let app = AppState::with_font_and_config(session, font_mgr, config, None)
        .expect("with_font_and_config");
    assert_eq!(app.session.terminal.grid.cols, 80);
    assert_eq!(app.session.terminal.grid.rows, 24);
    assert!(app.font_mgr.metrics.cell_width > 0);
    assert!(app.renderer.is_none());
}

#[test]
fn test_app_state_with_font_and_config_initialization() {
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let session = crate::TerminalSession::new(term, pty);
    let config = crate::config::Config::default();
    let font_mgr =
        crate::font::FontManager::load_with_families(&config.font_families(), config.font_size())
            .expect("load font");
    let app = AppState::with_font_and_config(session, font_mgr, config, None)
        .expect("with_font_and_config");
    assert!(app.font_mgr.metrics.cell_width > 0);
    assert!(app.font_mgr.metrics.cell_height > 0);
}

#[test]
fn test_pre_event_loop_window_creation_and_surface_setup() {
    let (client, server) = std::os::unix::net::UnixStream::pair().expect("socketpair");
    let conn = wayland_client::Connection::from_socket(client).expect("conn");
    let queue = conn.new_event_queue::<AppState>();
    let qh = queue.handle();
    let registry = conn.display().get_registry(&qh, ());
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app = new_test_app(term, pty).expect("app");

    // Before binding globals, window remains unmapped
    assert_eq!(
        app.wayland.window_state,
        crate::wayland::WindowState::Unmapped
    );
    assert!(app.wayland.surface.is_none());

    // Bind compositor and xdg_wm_base globals into app
    let comp =
        registry.bind::<wayland_client::protocol::wl_compositor::WlCompositor, _, _>(1, 4, &qh, ());
    app.wayland.compositor = Some(comp);
    let xdg = registry.bind::<wayland_protocols::xdg::shell::client::xdg_wm_base::XdgWmBase, _, _>(
        2,
        1,
        &qh,
        (),
    );
    app.wayland.xdg_wm_base = Some(xdg);

    // Initializing window creates surface and transitions to Initializing
    app.wayland.init_window(&qh);
    assert!(app.wayland.surface.is_some());
    assert!(app.wayland.xdg_surface.is_some());
    assert!(app.wayland.xdg_toplevel.is_some());
    assert_eq!(
        app.wayland.window_state,
        crate::wayland::WindowState::Initializing
    );

    // Flushing connection succeeds without error
    assert!(conn.flush().is_ok());
    drop(server);
}

#[test]
fn test_pty_registration_guarded_by_wayland_configured() {
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app = new_test_app(term, pty).expect("app");

    // 1. Unmapped state: must reject registration
    assert!(!app.wayland.configured);
    assert!(!app.should_register_pty());

    // 2. Initializing state: still unconfigured, must reject registration
    assert!(
        app.wayland
            .transition_window_to(crate::wayland::WindowState::Initializing)
            .is_ok()
    );
    assert!(!app.should_register_pty());

    // 3. Configured state: must accept registration
    assert!(
        app.wayland
            .transition_window_to(crate::wayland::WindowState::Configured)
            .is_ok()
    );
    assert!(app.wayland.configured);
    assert!(app.should_register_pty());

    // 4. Once registered: must not register a second time
    app.pty_registered = true;
    assert!(!app.should_register_pty());
}

#[test]
fn test_configure_renderer_requires_window_surface() {
    let (client, server) = std::os::unix::net::UnixStream::pair().expect("socketpair");
    let conn = wayland_client::Connection::from_socket(client).expect("conn");
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app = new_test_app(term, pty).expect("app");

    // Without a window surface, configure_renderer must fail with WindowNotCreated
    assert!(app.wayland.surface.is_none());
    assert!(matches!(
        app.configure_renderer(&conn),
        Err(crate::error::FttyError::Wayland(
            crate::error::WaylandError::WindowNotCreated
        ))
    ));
    drop(server);
}

#[test]
fn test_plaintext_url_hover_with_ctrl() {
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app = new_test_app(term, pty).expect("AppState new");

    let url = "https://github.com/xifan2333/ftty";
    for (i, c) in format!("Open {url} now!").chars().enumerate() {
        app.session.terminal.grid.lines[0].cells[i].c = c;
    }

    let cw = f64::from(app.font_mgr.metrics.cell_width);
    let ch = f64::from(app.font_mgr.metrics.cell_height);

    app.pointer.pointer_in_surface = true;
    // Over the URL (col 10)
    app.pointer.mouse_pos = [cw * 10.5, ch * 0.5];

    // Without Ctrl: no hover span
    app.update_hover_state();
    assert_eq!(app.pointer.hovered_span, None);

    // With Ctrl held: hover span detected!
    app.keyboard.update_modifiers(4, 0, 0, 0);
    assert!(app.keyboard.modifiers().ctrl);
    app.update_hover_state();
    assert_eq!(
        app.pointer.hovered_span,
        Some(HoveredHyperlinkSpan {
            line: 0,
            start_col: 5,
            end_col: 37,
        })
    );

    // Release Ctrl: hover span cleared
    app.keyboard.update_modifiers(0, 0, 0, 0);
    assert!(!app.keyboard.modifiers().ctrl);
    app.update_hover_state();
    assert_eq!(app.pointer.hovered_span, None);
}

#[test]
fn test_kitty_place_event_derives_dimensions_and_advances_cursor() {
    use crate::kitty::model::{ImageData, KittyAction, KittyCommand, KittyEvent};

    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app = new_test_app(term, pty).expect("AppState new");

    // 1. Transmit image (a=t) into terminal memory without placing
    let cw = app.font_mgr.metrics.cell_width as f32;
    let ch = app.font_mgr.metrics.cell_height as f32;
    let img_w = (cw * 10.0).round() as u32;
    let img_h = (ch * 4.0).round() as u32;
    let img = ImageData::new(42, img_w, img_h, vec![0; (img_w * img_h * 4) as usize]);

    let transmit_cmd = KittyCommand {
        action: KittyAction::Transmit,
        image_id: Some(42),
        ..Default::default()
    };
    let ctx = crate::kitty::GraphicContext {
        cell_width: cw,
        cell_height: ch,
        cursor_row: app.session.terminal.grid.cursor.row,
        cursor_col: app.session.terminal.grid.cursor.col,
        scrollback_len: app.session.terminal.grid.scrollback.len(),
    };
    let _ = app.session.terminal.grid.graphics.handle_event(
        KittyEvent::Transmit {
            command: transmit_cmd,
            image: img,
        },
        ctx,
        &std::collections::HashSet::new(),
    );
    assert_eq!(app.session.terminal.grid.graphics.placements().len(), 0);
    assert!(app.session.terminal.grid.contains_image(42));

    // Initial cursor position
    app.session.terminal.grid.cursor.col = 0;
    app.session.terminal.grid.cursor.row = 2;

    // 2. Place image (a=p) without specifying cols and rows (omitted c and r)
    let place_cmd = KittyCommand {
        action: KittyAction::Place,
        image_id: Some(42),
        ..Default::default()
    };
    let ctx2 = crate::kitty::GraphicContext {
        cell_width: cw,
        cell_height: ch,
        cursor_row: 2,
        cursor_col: 0,
        scrollback_len: app.session.terminal.grid.scrollback.len(),
    };
    let outcome = app.session.terminal.grid.graphics.handle_event(
        KittyEvent::Place { command: place_cmd },
        ctx2,
        &std::collections::HashSet::new(),
    );

    // Placement must derive cols (10) and rows (4) from the stored image
    assert_eq!(app.session.terminal.grid.graphics.placements().len(), 1);
    let p = &app.session.terminal.grid.graphics.placements()[0];
    assert_eq!(p.image_id, 42);
    assert_eq!(p.cols, 10);
    assert_eq!(p.rows, 4);

    // Cursor advance
    if let Some((cols, rows, _)) = outcome.cursor_advance {
        app.session.terminal.grid.cursor.col += cols;
        app.session.terminal.grid.cursor.row += rows.saturating_sub(1);
    }

    assert_eq!(app.session.terminal.grid.cursor.col, 10);
    assert_eq!(app.session.terminal.grid.cursor.row, 5);
}
}
