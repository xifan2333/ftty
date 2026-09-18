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

    pub(crate) fn configure_renderer(&mut self, connection: &Connection) -> Result<(), FttyError> {
        let size = self
            .pending_size
            .take()
            .unwrap_or([self.wayland.width, self.wayland.height]);
        if let Some(renderer) = &mut self.renderer {
            renderer.resize(size)?;
        } else {
            let surface = self
                .wayland
                .surface
                .as_ref()
                .ok_or(WaylandError::WindowNotCreated)?;
            self.renderer = Some(Renderer::new(surface, connection, size)?);
        }
        [self.wayland.width, self.wayland.height] = size;
        if let Some(xdg_surface) = &self.wayland.xdg_surface {
            xdg_surface.set_window_geometry(0, 0, size[0] as i32, size[1] as i32);
        }
        self.resize_terminal()?;
        self.needs_redraw = true;
        // A resize must be committed even if the compositor suspended the old frame callback.
        self.frame_callback = None;
        Ok(())
    }
}
