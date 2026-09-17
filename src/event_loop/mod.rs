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
use crate::font::{FontManager, GlyphAtlas};
use crate::grid::CellFlags;
use crate::ime::ImeState;
use crate::input::KeyboardHandler;
use crate::kitty::{KittyEvent, KittyParser};
use crate::parser::Terminal;
use crate::pty::Pty;
use crate::render::{ColorScheme, HoveredHyperlinkSpan, RenderOptions, Renderer};
use crate::selection::{Selection, SelectionPoint, SelectionType};
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
    pub(crate) render_error: Option<io::Error>,
    pub(crate) sync_output_start: Option<std::time::Instant>,
    pub(crate) last_sync_gen: u64,
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
        })
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
                        let incoming = &buf[..n];
                        if state.kitty_parser.is_fast_path(incoming) {
                            state.process_terminal_output(incoming, &qh);
                        } else {
                            let (clean_text, events) = state.kitty_parser.filter_bytes(incoming);
                            for event in &events {
                                if let KittyEvent::Response(resp) = event {
                                    state.write_pty_blocking(resp);
                                }
                            }
                            state.process_terminal_output(&clean_text, &qh);
                            for event in events {
                                if !matches!(event, KittyEvent::Response(_)) {
                                    state.handle_kitty_event(event);
                                }
                            }
                        }

                        // Rule 3332946: Synchronize Kitty keyboard flags directly on every PTY read iteration,
                        // ensuring state alignment even if the chunk contained only filtered graphics events.
                        state.keyboard.kitty_flags = state.terminal.kitty_keyboard_flags;

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
            app_state.update_ime_cursor_area();
            app_state.needs_redraw = false;
        }

        let _ = conn.flush();
    }

    app_state.render_error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests;
