//! Seat, keyboard, pointer, and cursor shape protocol dispatch.

use std::io::Write;

use wayland_client::protocol::{
    wl_keyboard::{self, KeyState, WlKeyboard},
    wl_pointer::{self, Axis, ButtonState, WlPointer},
    wl_seat::{self, Capability, WlSeat},
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{
    self, Shape, WpCursorShapeDeviceV1,
};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::{
    self, WpCursorShapeManagerV1,
};
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3;

use crate::event_loop::AppState;
use crate::input::mouse::{MouseModifiers, encode_mouse_event};
use crate::input::selection::{Selection, SelectionPoint, SelectionType, find_word_boundaries};
use crate::render::HoveredHyperlinkSpan;

/// Maps a Linux input button code to the X11 mouse button index used on the wire.
#[must_use]
pub fn x11_button_index(button: u32) -> Option<u8> {
    match button {
        0x110 => Some(0), // BTN_LEFT
        0x111 => Some(1), // BTN_MIDDLE
        0x112 => Some(2), // BTN_RIGHT
        _ => None,
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
                if state.hovered_span.is_some() {
                    state.hovered_span = None;
                    state.needs_redraw = true;
                }
                state.current_cursor_shape = None;
                state.pointer_in_surface = false;
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
                    if state.terminal.focus_reporting {
                        let _ = state.pty.write_all(b"\x1b[I");
                    }
                    state.ime.active = true;
                    if let Some(text_input) = &state.wayland.text_input {
                        text_input.enable();
                        text_input.set_content_type(
                            zwp_text_input_v3::ContentHint::None,
                            zwp_text_input_v3::ContentPurpose::Terminal,
                        );
                        let (x, y, w, h) = crate::input::ime::calculate_cursor_rect(
                            &state.terminal.grid,
                            state.font_mgr.metrics,
                            [state.config.padding_x(), state.config.padding_y()],
                        );
                        text_input.set_cursor_rectangle(x, y, w, h);
                        text_input.commit();
                    }
                }
            }
            wl_keyboard::Event::Leave { surface, .. } => {
                if state.wayland.surface.as_ref() == Some(&surface) {
                    if state.terminal.focus_reporting {
                        let _ = state.pty.write_all(b"\x1b[O");
                    }
                    state.ime.clear();
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
                    if pressed && state.config.auto_scroll() && !state.terminal.grid.is_alt_screen()
                    {
                        state.terminal.grid.scroll_viewport_bottom();
                    }
                    let _ = state.pty.write_all(&bytes);
                    if pressed {
                        state.update_ime_cursor_area();
                    }
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
                state.pointer_in_surface = true;
                state.pointer_serial = serial;
                state.last_serial = serial;
                state.mouse_pos = [surface_x, surface_y];
                state.current_cursor_shape = None;
                state.update_hover_state();
                state.update_cursor_shape();
            }
            wl_pointer::Event::Leave { .. } => {
                state.pointer_in_surface = false;
                if state.hovered_span.is_some() {
                    state.hovered_span = None;
                    state.needs_redraw = true;
                }
                state.current_cursor_shape = None;
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.mouse_pos = [surface_x, surface_y];
                state.update_hover_state();
                let held = state.mouse_buttons_held != 0 || state.mouse_reported;
                if state.terminal.mouse.reports_motion(held) {
                    let button = if held {
                        if state.mouse_buttons_held != 0 {
                            state.mouse_buttons_held.trailing_zeros() as u8
                        } else {
                            state.mouse_button
                        }
                    } else {
                        3
                    };
                    if state.report_mouse_event(button, true, true) {
                        return;
                    }
                }
                if state.mouse_pressed {
                    let (line, _, col) = state.cell_at_pointer(surface_x, surface_y);
                    state.selection.end = SelectionPoint::new(line, col);
                    state.needs_redraw = true;
                }
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(ButtonState::Pressed),
                time,
                serial,
            } => {
                state.last_serial = serial;
                let Some(index) = x11_button_index(button) else {
                    return;
                };
                if index == 0
                    && state.keyboard.modifiers().ctrl
                    && let Some(url_owned) = state.url_at_pointer()
                {
                    std::thread::spawn(move || {
                        let _ = std::process::Command::new("xdg-open")
                            .arg(&url_owned)
                            .spawn();
                    });
                    return;
                }
                // Applications that requested mouse tracking own the event; Shift
                // always overrides tracking so text can still be selected.
                if state.report_mouse_event(index, true, false) {
                    state.mouse_buttons_held |= 1 << index;
                    state.mouse_reported = true;
                    state.mouse_button = index;
                    return;
                }
                if index == 1 {
                    // BTN_MIDDLE pastes the primary selection, as elsewhere on X11.
                    state.paste_clipboard(Some(conn));
                    return;
                }
                if index != 0 {
                    return;
                }
                let (line, screen_row, col) =
                    state.cell_at_pointer(state.mouse_pos[0], state.mouse_pos[1]);

                let same_cell = state.last_click_cell == Some((line, col));
                if same_cell && time.saturating_sub(state.last_click_time) < 350 {
                    state.click_count = (state.click_count % 3) + 1;
                } else {
                    state.click_count = 1;
                }
                state.last_click_time = time;
                state.last_click_cell = Some((line, col));
                state.mouse_pressed = true;
                state.update_hover_state();

                match state.click_count {
                    1 => {
                        state.selection = Selection::new(
                            SelectionPoint::new(line, col),
                            SelectionPoint::new(line, col),
                            SelectionType::Simple,
                        );
                    }
                    2 => {
                        let row = state.terminal.grid.visible_line(screen_row);
                        let (w_start, w_end) = find_word_boundaries(row, col);
                        state.selection = Selection::new(
                            SelectionPoint::new(line, w_start),
                            SelectionPoint::new(line, w_end),
                            SelectionType::Word,
                        );
                    }
                    3 => {
                        state.selection = Selection::new(
                            SelectionPoint::new(line, 0),
                            SelectionPoint::new(line, state.terminal.grid.cols.saturating_sub(1)),
                            SelectionType::Line,
                        );
                    }
                    _ => {}
                }
                state.needs_redraw = true;
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(ButtonState::Released),
                ..
            } => {
                let Some(index) = x11_button_index(button) else {
                    return;
                };
                let was_held = (state.mouse_buttons_held & (1 << index)) != 0;
                if was_held {
                    state.mouse_buttons_held &= !(1 << index);
                    state.mouse_reported = state.mouse_buttons_held != 0;
                    state.report_mouse_event(index, false, false);
                }
                if index == 0 {
                    state.mouse_pressed = false;
                    state.update_hover_state();
                }
            }
            wl_pointer::Event::Axis {
                axis: WEnum::Value(Axis::VerticalScroll),
                value,
                ..
            } => {
                let multiplier = f64::from(state.config.scroll_multiplier());
                state.scroll_accumulator += (value / 15.0) * multiplier;

                let lines = state.scroll_accumulator.trunc() as i32;
                if lines == 0 {
                    return;
                }
                state.scroll_accumulator -= f64::from(lines);
                let count = (lines.unsigned_abs() as usize).min(100);

                if state.terminal.mouse.is_reporting() && !state.keyboard.modifiers().shift {
                    // Wheel notches are reported as buttons 64 (up) and 65 (down).
                    let button = if lines < 0 { 64 } else { 65 };
                    for _ in 0..count {
                        if !state.report_mouse_event(button, true, false) {
                            break;
                        }
                    }
                } else if state.terminal.grid.is_alt_screen() {
                    let seq: &[u8] = if lines < 0 { b"\x1b[A" } else { b"\x1b[B" };
                    let batch = seq.repeat(count);
                    let _ = state.pty.write_all(&batch);
                } else {
                    if lines < 0 {
                        state.terminal.grid.scroll_viewport_up(count);
                    } else {
                        state.terminal.grid.scroll_viewport_down(count);
                    }
                    state.needs_redraw = true;
                    state.update_hover_state();
                }
            }
            wl_pointer::Event::AxisStop { .. } => {
                state.scroll_accumulator = 0.0;
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

impl AppState {
    /// Initializes the cursor shape device if protocol is available and updates shape if focused.
    pub fn try_init_cursor_shape(&mut self, qh: &QueueHandle<Self>) {
        if self.wayland.cursor_shape_device.is_none() {
            self.wayland.init_cursor_shape(qh);
            if self.wayland.cursor_shape_device.is_some()
                && self.pointer_in_surface
                && self.pointer_serial != 0
            {
                self.current_cursor_shape = None;
                self.update_cursor_shape();
            }
        }
    }

    /// Updates the Wayland cursor shape based on whether a hyperlink is currently hovered.
    pub fn update_cursor_shape(&mut self) {
        if !self.pointer_in_surface || self.pointer_serial == 0 {
            return;
        }
        let shape = if self.hovered_span.is_some() {
            Shape::Pointer
        } else {
            Shape::Text
        };
        if self.current_cursor_shape == Some(shape) {
            return;
        }
        if let Some(device) = &self.wayland.cursor_shape_device {
            device.set_shape(self.pointer_serial, shape);
            self.current_cursor_shape = Some(shape);
        }
    }

    /// Updates the hovered hyperlink and cursor shape based on current pointer coordinates.
    pub fn update_hover_state(&mut self) {
        let new_span = if !self.pointer_in_surface || self.mouse_pressed {
            None
        } else {
            let (line, screen_row, col) =
                self.cell_at_pointer(self.mouse_pos[0], self.mouse_pos[1]);
            let row = self.terminal.grid.visible_line(screen_row);
            if let Some(cell) = row.cells.get(col)
                && let Some(id) = cell.hyperlink_id
                && self.terminal.hyperlink_url(id.get()).is_some()
            {
                let mut start_col = col;
                while start_col > 0
                    && row.cells.get(start_col - 1).and_then(|c| c.hyperlink_id) == Some(id)
                {
                    start_col -= 1;
                }
                let mut end_col = col;
                while end_col + 1 < row.cells.len()
                    && row.cells.get(end_col + 1).and_then(|c| c.hyperlink_id) == Some(id)
                {
                    end_col += 1;
                }
                Some(HoveredHyperlinkSpan {
                    line,
                    start_col,
                    end_col,
                })
            } else if self.keyboard.modifiers().ctrl
                && let Some((start_col, end_col, _)) =
                    find_url_in_grid(&self.terminal.grid, line, screen_row, col)
            {
                Some(HoveredHyperlinkSpan {
                    line,
                    start_col,
                    end_col,
                })
            } else {
                None
            }
        };
        if self.hovered_span != new_span {
            self.hovered_span = new_span;
            self.needs_redraw = true;
            self.update_cursor_shape();
        }
    }

    /// Returns the resolved URL at the current pointer position if one exists.
    #[must_use]
    pub fn url_at_pointer(&self) -> Option<String> {
        let (line, screen_row, col) = self.cell_at_pointer(self.mouse_pos[0], self.mouse_pos[1]);
        let row = self.terminal.grid.visible_line(screen_row);
        let cell = row.cells.get(col);
        if let Some(cell) = cell
            && let Some(id) = cell.hyperlink_id
            && let Some(url) = self.terminal.hyperlink_url(id.get())
        {
            return Some(url.to_string());
        }
        find_url_in_grid(&self.terminal.grid, line, screen_row, col).map(|(_, _, url)| url)
    }

    /// Returns the absolute `(line, screen_row, col)` grid coordinates under the surface-relative pointer position.
    #[must_use]
    pub fn cell_at_pointer(&self, surface_x: f64, surface_y: f64) -> (usize, usize, usize) {
        let cw = f64::from(self.font_mgr.metrics.cell_width);
        let ch = f64::from(self.font_mgr.metrics.cell_height);
        let pad_x = f64::from(self.config.padding_x());
        let pad_y = f64::from(self.config.padding_y());

        let col = ((surface_x - pad_x) / cw).max(0.0) as usize;
        let col = col.min(self.terminal.grid.cols.saturating_sub(1));

        let screen_row = ((surface_y - pad_y) / ch).max(0.0) as usize;
        let screen_row = screen_row.min(self.terminal.grid.rows.saturating_sub(1));

        let abs_line =
            self.terminal.grid.scrollback.len() + screen_row - self.terminal.grid.viewport_offset;
        (abs_line, screen_row, col)
    }

    /// Encodes a pointer event for the application, or `None` when the event stays local.
    ///
    /// Mouse reports are suppressed while tracking is disabled and while Shift is held,
    /// which is the conventional override that hands the pointer back to text selection.
    pub(crate) fn mouse_report_bytes(
        &self,
        button: u8,
        pressed: bool,
        motion: bool,
    ) -> Option<Vec<u8>> {
        if !self.terminal.mouse.is_reporting() {
            return None;
        }
        let modifiers = self.keyboard.modifiers();
        if modifiers.shift && pressed {
            return None;
        }
        let (_, screen_row, col) = self.cell_at_pointer(self.mouse_pos[0], self.mouse_pos[1]);
        encode_mouse_event(
            self.terminal.mouse.encoding,
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

    /// Forwards a pointer event to the PTY and reports whether the application consumed it.
    pub(crate) fn report_mouse_event(&mut self, button: u8, pressed: bool, motion: bool) -> bool {
        let Some(bytes) = self.mouse_report_bytes(button, pressed, motion) else {
            return false;
        };
        self.write_pty_blocking(&bytes);
        true
    }
}

/// Extracts a plaintext URL across wrapped lines in `grid` containing `(target_line, target_screen_row, target_col)`.
/// Returns `(start_col, end_col, url)` where `start_col..=end_col` is the span on `target_line`.
#[must_use]
pub fn find_url_in_grid(
    grid: &crate::grid::Grid,
    target_line: usize,
    target_screen_row: usize,
    target_col: usize,
) -> Option<(usize, usize, String)> {
    let mut start_row = target_screen_row;
    while start_row > 0 {
        if grid.visible_line(start_row - 1).wrapped {
            start_row -= 1;
        } else {
            break;
        }
    }
    let mut end_row = target_screen_row;
    while end_row + 1 < grid.rows {
        if grid.visible_line(end_row).wrapped {
            end_row += 1;
        } else {
            break;
        }
    }

    let mut text = String::new();
    let mut coords: Vec<(usize, usize, usize)> = Vec::new(); // (abs_line, col, width)

    for r in start_row..=end_row {
        let line = grid.visible_line(r);
        let abs_line = grid.scrollback.len() + r - grid.viewport_offset;
        for (c_idx, cell) in line.cells.iter().enumerate() {
            if cell.flags.contains(crate::grid::CellFlags::HIDDEN) {
                text.push(' ');
                coords.push((abs_line, c_idx, 1));
            } else if cell
                .flags
                .contains(crate::grid::CellFlags::WIDE_CHAR_SPACER)
            {
                // Skip wide character continuation spacers
            } else {
                let width = if cell.flags.contains(crate::grid::CellFlags::WIDE_CHAR) {
                    2
                } else {
                    1
                };
                text.push(cell.c);
                coords.push((abs_line, c_idx, width));
            }
        }
    }

    let schemes = ["https://", "http://", "file://", "gemini://"];
    for scheme in &schemes {
        let mut search_from = 0;
        while let Some(pos) = text[search_from..].find(scheme) {
            let start = search_from + pos;
            let mut end = start;
            for (idx, ch) in text[start..].char_indices() {
                if ch.is_whitespace()
                    || ch == '<'
                    || ch == '>'
                    || ch == '"'
                    || ch == '`'
                    || ch == '^'
                    || ch == '\\'
                    || ch == '|'
                {
                    break;
                }
                end = start + idx + ch.len_utf8();
            }

            while end > start {
                let Some(last_char) = text[..end].chars().next_back() else {
                    break;
                };
                if matches!(
                    last_char,
                    '.' | ',' | '!' | '?' | ';' | ':' | ')' | ']' | '}' | '\'' | '"'
                ) {
                    if last_char == ')'
                        && text[start..end].matches('(').count()
                            == text[start..end].matches(')').count()
                    {
                        break;
                    }
                    end -= last_char.len_utf8();
                } else {
                    break;
                }
            }

            let start_char_idx = text[..start].chars().count();
            let end_char_idx = text[..end].chars().count();

            if start_char_idx < coords.len() && end_char_idx <= coords.len() {
                let url_coords = &coords[start_char_idx..end_char_idx];
                let is_hit = url_coords.iter().any(|&(line, c, w)| {
                    line == target_line && target_col >= c && target_col < c + w
                });

                if is_hit && end > start + scheme.len() {
                    let mut min_col = usize::MAX;
                    let mut max_col = 0;
                    for &(line, c, w) in url_coords {
                        if line == target_line {
                            min_col = min_col.min(c);
                            max_col = max_col.max(c + w - 1);
                        }
                    }
                    if min_col <= max_col {
                        let url = text[start..end].to_string();
                        return Some((min_col, max_col, url));
                    }
                }
            }

            search_from = start + scheme.len();
        }
    }

    None
}

/// Extracts a plaintext URL and its column span on a given row if `col` falls within it.
#[must_use]
pub fn find_url_at_col(row: &crate::grid::Row, col: usize) -> Option<(usize, usize, String)> {
    let mut grid = crate::grid::Grid::new(row.cells.len(), 1, 0);
    grid.lines[0] = row.clone();
    find_url_in_grid(&grid, 0, 0, col)
}
