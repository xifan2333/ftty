//! Unified calloop single-threaded event loop multiplexing Wayland, PTY I/O, and POSIX signals.

pub mod actions;
pub mod geometry;
pub mod kitty;

pub use geometry::{saturating_u16, terminal_size};

use std::io::{self, Read};
use std::path::PathBuf;

use calloop::generic::Generic;
use calloop::signals::{Signal, Signals};
use calloop::{EventLoop, Interest, Mode};
use calloop_wayland_source::WaylandSource;

use wayland_client::Connection;
use wayland_client::protocol::wl_callback::WlCallback;
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::Shape;

use crate::color::Rgb;
use crate::config::Config;
use crate::error::{FttyError, WaylandError};
use crate::font::{FontManager, GlyphAtlas};
use crate::input::KeyboardHandler;
use crate::input::ime::ImeState;
use crate::input::selection::{Selection, SelectionPoint, SelectionType};
use crate::kitty::{KittyEvent, KittyParser};
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
    pub(crate) pending_size: Option<[u32; 2]>,
    /// Set by an `xdg_surface.configure`; the resize is applied once the queue is drained so a
    /// burst of configures collapses into a single, final size.
    pub(crate) configure_pending: bool,
    pub(crate) render_error: Option<FttyError>,
    pub(crate) sync_output_start: Option<std::time::Instant>,
    pub(crate) last_sync_gen: u64,
    pub(crate) pty_registered: bool,
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
        let config = Config::load_from_path_or_default(config_path.as_deref())?;
        Self::with_loaded_config(terminal, pty, config, config_path)
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
        let font_mgr = FontManager::load_with_families_and_subpixel(
            &config.font_families(),
            config.font_size(),
            config.font_subpixel(),
        )?;
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
        let atlas = GlyphAtlas::new(1024, 1024);
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
        wayland.stashed_floating_size = Some([wayland.width, wayland.height]);

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
            pty_registered: false,
        })
    }

    /// Pre-warms EGL display, OpenGL context, and shader compilation immediately
    /// following window surface creation so the first configure only needs a sub-millisecond resize.
    pub fn prewarm_renderer(&mut self, connection: &Connection) {
        if let Some(surface) = &self.wayland.surface {
            let size = [self.wayland.width, self.wayland.height];
            if let Ok(renderer) = Renderer::new(surface, connection, size) {
                self.renderer = Some(renderer);
            }
        }
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
    conn.flush()
        .map_err(|e| WaylandError::Dispatch(e.to_string()))?;
    event_queue
        .roundtrip(&mut app_state)
        .map_err(|e| WaylandError::Dispatch(e.to_string()))?;
    conn.flush()
        .map_err(|e| WaylandError::Dispatch(e.to_string()))?;

    app_state.prewarm_renderer(&conn);

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
        if !app_state.pty_registered && app_state.renderer.is_some() {
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
                                    let (clean_text, events) =
                                        state.kitty_parser.filter_bytes(incoming);
                                    for event in &events {
                                        if let KittyEvent::Response(resp) = event {
                                            state.write_pty_blocking(resp);
                                        }
                                    }
                                    state.process_terminal_output(&clean_text, &pty_qh);
                                    for event in events {
                                        if !matches!(event, KittyEvent::Response(_)) {
                                            state.handle_kitty_event(event);
                                        }
                                    }
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
                if let Some(xdg_surface) = &app_state.wayland.xdg_surface {
                    xdg_surface.set_window_geometry(
                        0,
                        0,
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

        let _ = conn.flush();
    }

    app_state.render_error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests;
