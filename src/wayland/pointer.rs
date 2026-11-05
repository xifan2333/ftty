//! Pointer bridges: layout context, declarative outcome dispatch, and hover tracking.

use wayland_client::{Connection, QueueHandle};

use crate::event_loop::AppState;
use crate::input::mouse::{MouseModifiers, encode_mouse_event};
use crate::input::pointer::{PointerContext, PointerController, PointerOutcome};

use super::WaylandState;

impl AppState {
    /// Builds transient layout and metrics context for pointer operations with separate field borrows.
    pub(crate) fn make_pointer_context<'a>(
        terminal: &'a crate::parser::Terminal,
        font_mgr: &'a crate::font::FontManager,
        config: &'a crate::config::Config,
        keyboard: &'a crate::input::KeyboardHandler,
        wayland: &'a WaylandState,
        hyperlink_resolver: Option<&'a dyn Fn(u32) -> Option<&'a str>>,
    ) -> PointerContext<'a> {
        let scale = if wayland.is_fractional_scale_active() {
            wayland.scale_factor.max(0.1)
        } else {
            1.0
        };
        PointerContext::new(
            &terminal.grid,
            font_mgr.metrics,
            scale,
            [config.padding_x(), config.padding_y()],
            keyboard.modifiers(),
            &terminal.mouse,
            f64::from(config.scroll_multiplier()),
            terminal.grid.is_alt_screen(),
            hyperlink_resolver,
        )
    }

    /// Builds transient layout and metrics context for pointer operations.
    pub(crate) fn pointer_ctx<'a>(
        &'a self,
        hyperlink_resolver: Option<&'a dyn Fn(u32) -> Option<&'a str>>,
    ) -> PointerContext<'a> {
        Self::make_pointer_context(
            &self.session.terminal,
            &self.font_mgr,
            &self.config,
            &self.keyboard,
            &self.wayland,
            hyperlink_resolver,
        )
    }

    /// Dispatches declarative actions produced by [`PointerController`] to PTY, compositor, and system.
    pub(crate) fn dispatch_pointer_outcome(&mut self, outcome: PointerOutcome, conn: &Connection) {
        if let Some(bytes) = outcome.pty_write {
            self.write_pty_blocking(&bytes);
        }
        if let Some(shape) = outcome.cursor_shape
            && let Some(device) = &self.wayland.cursor_shape_device
        {
            device.set_shape(self.pointer_serial, shape);
        }
        if let Some(url) = outcome.open_url {
            std::thread::spawn(move || {
                let _ = std::process::Command::new("xdg-open").arg(&url).status();
            });
        }
        if outcome.paste_primary {
            self.paste_clipboard(Some(conn));
        }
        if let Some(lines) = outcome.scroll_lines {
            if lines < 0 {
                self.session
                    .terminal
                    .grid
                    .scroll_viewport_up(lines.unsigned_abs() as usize);
            } else {
                self.session
                    .terminal
                    .grid
                    .scroll_viewport_down(lines as usize);
            }
            let resolver = |id: u32| self.session.terminal.hyperlink_url(id);
            let ctx = Self::make_pointer_context(
                &self.session.terminal,
                &self.font_mgr,
                &self.config,
                &self.keyboard,
                &self.wayland,
                Some(&resolver),
            );
            let (_, shape_update) = self.pointer.update_hover_state(&ctx);
            if let Some(shape) = shape_update
                && let Some(device) = &self.wayland.cursor_shape_device
                && self.pointer_serial != 0
            {
                device.set_shape(self.pointer_serial, shape);
            }
        }
        if outcome.needs_redraw {
            self.needs_redraw = true;
        }
    }

    /// Initializes the cursor shape device if protocol is available and updates shape if focused.
    pub fn try_init_cursor_shape(&mut self, qh: &QueueHandle<Self>) {
        if self.wayland.cursor_shape_device.is_none() {
            self.wayland.init_cursor_shape(qh);
            if self.wayland.cursor_shape_device.is_some()
                && self.pointer.is_in_surface()
                && self.pointer_serial != 0
            {
                self.pointer.current_cursor_shape = None;
                self.update_cursor_shape();
            }
        }
    }

    /// Updates the Wayland cursor shape based on whether a hyperlink is hovered or mouse tracking is active.
    /// Aligns with Kitty: Shape::Pointer on hyperlinks, Shape::Default when an app tracks the pointer, Shape::Text for text selection.
    pub fn update_cursor_shape(&mut self) {
        if !self.pointer.is_in_surface() || self.pointer_serial == 0 {
            return;
        }
        let resolver = |id: u32| self.session.terminal.hyperlink_url(id);
        let ctx = self.pointer_ctx(Some(&resolver));
        let shape = self.pointer.expected_cursor_shape(&ctx);
        if self.pointer.current_cursor_shape() == Some(shape) {
            return;
        }
        if let Some(device) = &self.wayland.cursor_shape_device {
            device.set_shape(self.pointer_serial, shape);
            self.pointer.current_cursor_shape = Some(shape);
        }
    }

    /// Updates the hovered hyperlink and cursor shape based on current pointer coordinates.
    pub fn update_hover_state(&mut self) {
        let resolver = |id: u32| self.session.terminal.hyperlink_url(id);
        let ctx = Self::make_pointer_context(
            &self.session.terminal,
            &self.font_mgr,
            &self.config,
            &self.keyboard,
            &self.wayland,
            Some(&resolver),
        );
        let (span_changed, shape_update) = self.pointer.update_hover_state(&ctx);
        if span_changed {
            self.needs_redraw = true;
        }
        if let Some(shape) = shape_update
            && let Some(device) = &self.wayland.cursor_shape_device
            && self.pointer_serial != 0
        {
            device.set_shape(self.pointer_serial, shape);
        }
    }

    /// Returns the resolved URL at the current pointer position if one exists.
    #[must_use]
    pub fn url_at_pointer(&self) -> Option<String> {
        let resolver = |id: u32| self.session.terminal.hyperlink_url(id);
        let ctx = self.pointer_ctx(Some(&resolver));
        self.pointer.url_at_pointer(&ctx)
    }

    /// Returns the absolute `(line, screen_row, col)` grid coordinates under the surface-relative pointer position.
    #[must_use]
    pub fn cell_at_pointer(&self, surface_x: f64, surface_y: f64) -> (usize, usize, usize) {
        let resolver = |id: u32| self.session.terminal.hyperlink_url(id);
        let ctx = self.pointer_ctx(Some(&resolver));
        PointerController::cell_at_coords(surface_x, surface_y, &ctx)
    }

    /// Encodes a pointer event for the application, or `None` when the event stays local.
    ///
    /// Mouse reports are suppressed while tracking is disabled and while Shift is held,
    /// which is the conventional override that hands the pointer back to text selection.
    #[doc(hidden)]
    pub fn mouse_report_bytes(&self, button: u8, pressed: bool, motion: bool) -> Option<Vec<u8>> {
        let resolver = |id: u32| self.session.terminal.hyperlink_url(id);
        let ctx = self.pointer_ctx(Some(&resolver));
        if !ctx.mouse_mode.is_reporting() {
            return None;
        }
        let modifiers = self.keyboard.modifiers();
        if modifiers.shift && pressed {
            return None;
        }
        let (_, screen_row, col) = PointerController::cell_at_coords(
            self.pointer.mouse_pos[0],
            self.pointer.mouse_pos[1],
            &ctx,
        );
        encode_mouse_event(
            ctx.mouse_mode.encoding,
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
}
