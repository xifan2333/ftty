//! Input device adapter: seat capabilities, keyboard, pointer, and cursor shape.

use wayland_client::protocol::{
    wl_keyboard::{self, KeyState, WlKeyboard},
    wl_pointer::{self, Axis, ButtonState, WlPointer},
    wl_seat::{self, Capability, WlSeat},
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{
    self, WpCursorShapeDeviceV1,
};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::{
    self, WpCursorShapeManagerV1,
};
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3;

use crate::event_loop::AppState;

use super::WaylandState;

impl WaylandState {
    /// Creates and initializes the `wp_cursor_shape_device_v1` instance once the manager and pointer are available.
    pub fn init_cursor_shape<D>(&mut self, qh: &QueueHandle<D>)
    where
        D: wayland_client::Dispatch<WpCursorShapeDeviceV1, ()> + 'static,
    {
        if self.cursor_shape_device.is_some() {
            return;
        }
        let Some(manager) = &self.cursor_shape_manager else {
            return;
        };
        let Some(pointer) = &self.pointer else {
            return;
        };

        let device = manager.get_pointer(pointer, qh, ());
        self.cursor_shape_device = Some(device);
    }
}

// --- Seat & Input ---

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
            if caps.contains(Capability::Pointer) {
                if state.wayland.pointer.is_none() {
                    let pointer = proxy.get_pointer(qh, ());
                    state.wayland.pointer = Some(pointer);
                    state.try_init_cursor_shape(qh);
                }
            } else {
                if let Some(device) = state.wayland.cursor_shape_device.take() {
                    device.destroy();
                }
                if let Some(pointer) = state.wayland.pointer.take() {
                    pointer.release();
                }
                let outcome = state.pointer.handle_leave();
                if outcome.needs_redraw {
                    state.needs_redraw = true;
                }
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
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } => {
                state.keyboard.set_keymap_from_fd(fd, size as usize);
            }
            wl_keyboard::Event::Enter {
                serial, surface, ..
            } => {
                state.last_serial = serial;
                if state.wayland.surface.as_ref() == Some(&surface) {
                    if state.session.terminal.focus_reporting {
                        state.session.write_input_bytes(b"\x1b[I");
                    }
                    state.ime.active = true;
                    state.last_ime_cursor_rect.set(None);
                    if let Some(text_input) = &state.wayland.text_input {
                        text_input.enable();
                        text_input.set_content_type(
                            zwp_text_input_v3::ContentHint::None,
                            zwp_text_input_v3::ContentPurpose::Terminal,
                        );
                        state.update_ime_cursor_area();
                    }
                }
            }
            wl_keyboard::Event::Leave { surface, .. } => {
                if state.wayland.surface.as_ref() == Some(&surface) {
                    if state.session.terminal.focus_reporting {
                        state.session.write_input_bytes(b"\x1b[O");
                    }
                    state.ime.clear();
                    state.last_ime_cursor_rect.set(None);
                    if let Some(text_input) = &state.wayland.text_input {
                        text_input.disable();
                        text_input.commit();
                    }
                    state.needs_redraw = true;
                }
            }
            wl_keyboard::Event::Key {
                serial,
                key,
                state: WEnum::Value(key_state),
                ..
            } => {
                state.last_serial = serial;
                let pressed = key_state == KeyState::Pressed;
                if pressed
                    && let Some(action) =
                        state.keyboard.check_action(key, &state.config.keybindings)
                {
                    state.handle_key_action(action, Some(qh), Some(_conn));
                } else if let Some(bytes) = state.keyboard.handle_key_event(key, pressed, false) {
                    if pressed
                        && state.config.auto_scroll()
                        && !state.session.terminal.grid.is_alt_screen()
                    {
                        state.session.terminal.grid.scroll_viewport_bottom();
                    }
                    state.session.write_input_bytes(&bytes);
                }
            }
            wl_keyboard::Event::Modifiers {
                serial,
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            } => {
                state.last_serial = serial;
                state
                    .keyboard
                    .update_modifiers(mods_depressed, mods_latched, mods_locked, group);
                state.update_hover_state();
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
                serial,
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_serial = serial;
                state.last_serial = serial;
                let resolver = |id: u32| state.session.terminal.hyperlink_url(id);
                let ctx = Self::make_pointer_context(
                    &state.session.terminal,
                    &state.font_mgr,
                    &state.config,
                    &state.keyboard,
                    &state.wayland,
                    Some(&resolver),
                );
                let outcome = state
                    .pointer
                    .handle_enter(surface_x, surface_y, serial, &ctx);
                state.dispatch_pointer_outcome(outcome, conn);
            }
            wl_pointer::Event::Leave { .. } => {
                let outcome = state.pointer.handle_leave();
                state.dispatch_pointer_outcome(outcome, conn);
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                let resolver = |id: u32| state.session.terminal.hyperlink_url(id);
                let ctx = Self::make_pointer_context(
                    &state.session.terminal,
                    &state.font_mgr,
                    &state.config,
                    &state.keyboard,
                    &state.wayland,
                    Some(&resolver),
                );
                let outcome = state.pointer.handle_motion(surface_x, surface_y, &ctx);
                state.dispatch_pointer_outcome(outcome, conn);
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(button_state),
                time,
                serial,
            } => {
                state.last_serial = serial;
                let pressed = button_state == ButtonState::Pressed;
                let resolver = |id: u32| state.session.terminal.hyperlink_url(id);
                let ctx = Self::make_pointer_context(
                    &state.session.terminal,
                    &state.font_mgr,
                    &state.config,
                    &state.keyboard,
                    &state.wayland,
                    Some(&resolver),
                );
                let outcome = state
                    .pointer
                    .handle_button(button, pressed, time, serial, &ctx);
                state.dispatch_pointer_outcome(outcome, conn);
            }
            wl_pointer::Event::Axis {
                axis: WEnum::Value(Axis::VerticalScroll),
                value,
                ..
            } => {
                let resolver = |id: u32| state.session.terminal.hyperlink_url(id);
                let ctx = Self::make_pointer_context(
                    &state.session.terminal,
                    &state.font_mgr,
                    &state.config,
                    &state.keyboard,
                    &state.wayland,
                    Some(&resolver),
                );
                let outcome = state.pointer.handle_axis(value, &ctx);
                state.dispatch_pointer_outcome(outcome, conn);
            }
            wl_pointer::Event::AxisStop { .. } => {
                let outcome = state.pointer.handle_axis_stop();
                state.dispatch_pointer_outcome(outcome, conn);
            }
            _ => {}
        }
    }
}

impl Dispatch<WpCursorShapeManagerV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WpCursorShapeManagerV1,
        _event: wp_cursor_shape_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpCursorShapeDeviceV1, ()> for AppState {
    fn event(
        _state: &mut Self,
        _proxy: &WpCursorShapeDeviceV1,
        _event: wp_cursor_shape_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}
