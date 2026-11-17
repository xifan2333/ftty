//! Input device adapter: seat capabilities, keyboard, pointer, and cursor shape.

use std::os::fd::OwnedFd;

use wayland_client::protocol::{
    wl_keyboard::{self, KeyState, WlKeyboard},
    wl_pointer::{self, Axis, ButtonState, WlPointer},
    wl_seat::{self, Capability, WlSeat},
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
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

impl AppState {
    /// Applies a seat capabilities change: bind or release keyboard and pointer objects.
    fn handle_seat_capabilities(
        &mut self,
        seat: &WlSeat,
        caps: Capability,
        qh: &QueueHandle<Self>,
    ) {
        self.handle_keyboard_capability(seat, caps, qh);
        self.handle_pointer_capability(seat, caps, qh);
    }

    /// Binds the keyboard when the seat gains the capability, or releases it when lost.
    fn handle_keyboard_capability(
        &mut self,
        seat: &WlSeat,
        caps: Capability,
        qh: &QueueHandle<Self>,
    ) {
        if caps.contains(Capability::Keyboard) && self.wayland.keyboard.is_none() {
            let keyboard = seat.get_keyboard(qh, ());
            self.wayland.keyboard = Some(keyboard);
        } else if !caps.contains(Capability::Keyboard)
            && let Some(keyboard) = self.wayland.keyboard.take()
            && keyboard.version() >= 3
        {
            // `wl_keyboard.release` only exists from version 3; older objects are
            // dropped locally so a re-advertised capability creates a fresh keyboard.
            keyboard.release();
        }
    }

    /// Binds the pointer and cursor-shape device when the seat gains the capability,
    /// or tears both down and drops hover state when it is lost.
    fn handle_pointer_capability(
        &mut self,
        seat: &WlSeat,
        caps: Capability,
        qh: &QueueHandle<Self>,
    ) {
        if caps.contains(Capability::Pointer) {
            if self.wayland.pointer.is_none() {
                let pointer = seat.get_pointer(qh, ());
                self.wayland.pointer = Some(pointer);
                self.try_init_cursor_shape(qh);
            }
        } else {
            if let Some(device) = self.wayland.cursor_shape_device.take() {
                device.destroy();
            }
            if let Some(pointer) = self.wayland.pointer.take()
                && pointer.version() >= 3
            {
                pointer.release();
            }
            let outcome = self.pointer.handle_leave();
            if outcome.needs_redraw {
                self.needs_redraw = true;
            }
        }
    }

    /// Installs a new xkb keymap announced by the compositor.
    fn handle_keymap(&mut self, fd: OwnedFd, size: u32) {
        self.keyboard.set_keymap_from_fd(fd, size as usize);
    }

    /// Applies keyboard focus enter: focus reporting, IME activation, and cursor tracking.
    fn handle_keyboard_enter(&mut self, serial: u32, surface: &WlSurface) {
        self.last_serial = serial;
        if self.wayland.surface.as_ref() != Some(surface) {
            return;
        }
        if self.session.terminal.focus_reporting {
            self.session.write_input_bytes(b"\x1b[I");
        }
        self.ime.active = true;
        self.last_ime_cursor_rect.set(None);
        if let Some(text_input) = &self.wayland.text_input {
            text_input.enable();
            text_input.set_content_type(
                zwp_text_input_v3::ContentHint::None,
                zwp_text_input_v3::ContentPurpose::Terminal,
            );
            self.update_ime_cursor_area();
        }
    }

    /// Applies keyboard focus leave: focus reporting, IME teardown, and a repaint.
    fn handle_keyboard_leave(&mut self, surface: &WlSurface) {
        if self.wayland.surface.as_ref() != Some(surface) {
            return;
        }
        if self.session.terminal.focus_reporting {
            self.session.write_input_bytes(b"\x1b[O");
        }
        self.ime.clear();
        self.last_ime_cursor_rect.set(None);
        if let Some(text_input) = &self.wayland.text_input {
            text_input.disable();
            text_input.commit();
        }
        self.needs_redraw = true;
    }

    /// Drives a raw key event through keybindings or keyboard translation to the PTY.
    fn handle_keyboard_key(
        &mut self,
        serial: u32,
        key: u32,
        pressed: bool,
        qh: &QueueHandle<Self>,
        conn: &Connection,
    ) {
        self.last_serial = serial;
        if pressed && let Some(action) = self.keyboard.check_action(key, &self.config.keybindings) {
            self.handle_key_action(action, Some(qh), Some(conn));
        } else if let Some(bytes) = self.keyboard.handle_key_event(key, pressed, false) {
            if pressed && self.config.auto_scroll() && !self.session.terminal.grid.is_alt_screen() {
                self.session.terminal.grid.scroll_viewport_bottom();
            }
            self.session.write_input_bytes(&bytes);
        }
    }

    /// Applies a modifier update and refreshes the hovered element.
    fn handle_keyboard_modifiers(
        &mut self,
        serial: u32,
        mods_depressed: u32,
        mods_latched: u32,
        mods_locked: u32,
        group: u32,
    ) {
        self.last_serial = serial;
        self.keyboard
            .update_modifiers(mods_depressed, mods_latched, mods_locked, group);
        self.update_hover_state();
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
            state.handle_seat_capabilities(proxy, caps, qh);
        }
    }
}

impl Dispatch<WlKeyboard, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap {
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } => {
                state.handle_keymap(fd, size);
            }
            wl_keyboard::Event::Enter {
                serial, surface, ..
            } => {
                state.handle_keyboard_enter(serial, &surface);
            }
            wl_keyboard::Event::Leave { surface, .. } => {
                state.handle_keyboard_leave(&surface);
            }
            wl_keyboard::Event::Key {
                serial,
                key,
                state: WEnum::Value(key_state),
                ..
            } => {
                state.handle_keyboard_key(serial, key, key_state == KeyState::Pressed, qh, conn);
            }
            wl_keyboard::Event::Modifiers {
                serial,
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            } => {
                state.handle_keyboard_modifiers(
                    serial,
                    mods_depressed,
                    mods_latched,
                    mods_locked,
                    group,
                );
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
                state.handle_pointer_enter(serial, surface_x, surface_y, conn);
            }
            wl_pointer::Event::Leave { .. } => {
                state.handle_pointer_leave(conn);
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.handle_pointer_motion(surface_x, surface_y, conn);
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(button_state),
                time,
                serial,
            } => {
                let pressed = button_state == ButtonState::Pressed;
                state.handle_pointer_button(button, pressed, time, serial, conn);
            }
            wl_pointer::Event::Axis {
                axis: WEnum::Value(Axis::VerticalScroll),
                value,
                ..
            } => {
                state.handle_pointer_axis(value, conn);
            }
            wl_pointer::Event::AxisStop { .. } => {
                state.handle_pointer_axis_stop(conn);
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
