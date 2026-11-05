//! Text-input-v3 adapter: IME activation, preedit/commit staging, and cursor box tracking.

use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::{self, ZwpTextInputV3};

use crate::event_loop::AppState;

use super::WaylandState;

impl WaylandState {
    /// Creates and initializes the `zwp_text_input_v3` instance once the manager and seat are available.
    pub fn init_text_input<D>(&mut self, qh: &QueueHandle<D>)
    where
        D: wayland_client::Dispatch<ZwpTextInputV3, ()> + 'static,
    {
        if self.text_input.is_some() {
            return;
        }
        let Some(manager) = &self.text_input_manager else {
            return;
        };
        let Some(seat) = &self.seat else {
            return;
        };

        let text_input = manager.get_text_input(seat, qh, ());
        self.text_input = Some(text_input);
    }
}

// --- Text Input (IME) ---

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
                state.session.ime_commit(delete, commit);
                state.needs_redraw = true;
            }
            _ => {}
        }
    }
}

impl AppState {
    /// Updates the Wayland `text-input-v3` cursor bounding box so the IME popup window tracks the cursor.
    /// Aligns with Kitty (`glfw/wl_text_input.c`): deduplicates cursor rectangle bounds and only commits when coordinates change.
    pub fn update_ime_cursor_area(&self) {
        if !self.ime.active {
            return;
        }
        let Some(text_input) = &self.wayland.text_input else {
            return;
        };
        let (x, y, w, h) = crate::input::ime::calculate_cursor_rect(
            &self.session.terminal.grid,
            self.font_mgr.metrics,
            [self.config.padding_x(), self.config.padding_y()],
        );
        let scale = if self.wayland.is_fractional_scale_active() {
            self.wayland.scale_factor.max(0.1)
        } else {
            1.0
        };
        let logical_x = (f64::from(x) / scale).round() as i32;
        let logical_y = (f64::from(y) / scale).round() as i32;
        let logical_w = (f64::from(w) / scale).round().max(1.0) as i32;
        let logical_h = (f64::from(h) / scale).round().max(1.0) as i32;
        let new_rect = (logical_x, logical_y, logical_w, logical_h);

        if self.last_ime_cursor_rect.get() != Some(new_rect) {
            self.last_ime_cursor_rect.set(Some(new_rect));
            text_input.set_cursor_rectangle(logical_x, logical_y, logical_w, logical_h);
            text_input.commit();
        }
    }
}
