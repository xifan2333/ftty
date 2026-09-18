//! Terminal geometry calculation, pixel dimension sizing, and renderer configuration.

use wayland_client::Connection;

use crate::error::{FttyError, WaylandError};
use crate::event_loop::AppState;
use crate::font::CellMetrics;
use crate::render::Renderer;

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
        if (self.terminal.grid.cols, self.terminal.grid.rows) != (cols as usize, rows as usize) {
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
        for response in self.terminal.take_responses() {
            self.write_pty_blocking(&response);
        }
        Ok(())
    }

    pub(crate) fn configure_renderer(&mut self, connection: &Connection) -> Result<(), FttyError> {
        let logical_size = self
            .pending_size
            .take()
            .unwrap_or([self.wayland.width, self.wayland.height]);
        let factor = self.wayland.scale_factor;
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
            self.renderer = Some(Renderer::new(surface, connection, physical_size)?);
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
        let factor = scale_120 as f64 / 120.0;
        if (self.wayland.scale_factor - factor).abs() > 0.001 {
            self.wayland.scale_factor = factor;
            self.wayland.preferred_scale_120 = scale_120;

            let base_font_size = self.config.font_size();
            let scaled_font_size = (base_font_size * factor as f32).max(crate::font::MIN_FONT_SIZE);
            self.update_font_size(scaled_font_size);

            if let Err(e) = self.configure_renderer(connection) {
                self.render_error = Some(e);
                self.running = false;
            }
        }
    }
}
