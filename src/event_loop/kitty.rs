//! Terminal output stream ingestion and Kitty graphics protocol event handling.

use wayland_client::QueueHandle;

use crate::event_loop::AppState;
use crate::kitty::{ImagePlacement, KittyAction, KittyEvent, kitty_response};

impl AppState {
    pub(crate) fn process_terminal_output(&mut self, text: &[u8], qh: &QueueHandle<Self>) {
        if text.is_empty() {
            return;
        }
        self.terminal.advance_bytes(&mut self.vt_parser, text);
        if !self.terminal.responses.is_empty() {
            for response in self.terminal.take_responses() {
                self.write_pty_blocking(&response);
            }
        }
        if let Some(pending) = self.terminal.take_pending_clipboard() {
            match pending {
                Some(text) => self.set_clipboard_text(text, Some(qh)),
                None => {
                    self.clipboard_text = None;
                    self.terminal.set_clipboard_content(None);
                    if let Some(device) = &self.wayland.data_device {
                        device.set_selection(None, self.last_serial);
                    }
                }
            }
        }
        self.keyboard.kitty_flags = self.terminal.kitty_keyboard_flags;
        if self.terminal.palette_dirty {
            self.terminal.palette_dirty = false;
            self.palette = self.terminal.palette;
            self.default_fg = self.terminal.default_fg;
            self.default_bg = self.terminal.default_bg;
            if let Some(renderer) = &mut self.renderer {
                renderer.clear_cache();
            }
            self.terminal.grid.mark_all_dirty();
            self.needs_redraw = true;
        }
        if self.config.auto_scroll() && !self.terminal.grid.is_alt_screen() {
            self.terminal.grid.scroll_viewport_bottom();
        }
    }

    pub(crate) fn handle_kitty_event(&mut self, event: KittyEvent) {
        match event {
            KittyEvent::Transmit { command, image } => {
                let image_id = image.id;
                let placement_id = command.placement_id.unwrap_or(0);
                let ack_id = command.placement_id.filter(|id| *id != 0);
                let img_w = (image.width as f32).max(1.0);
                let img_h = (image.height as f32).max(1.0);
                self.terminal.grid.add_image(image);

                let cw = self.font_mgr.metrics.cell_width as f32;
                let ch = self.font_mgr.metrics.cell_height as f32;

                let (cols, rows) = match (command.cols, command.rows) {
                    (Some(c), Some(r)) => (c as usize, r as usize),
                    (Some(c), None) => {
                        let pixel_w = c as f32 * cw;
                        let pixel_h = pixel_w * (img_h / img_w);
                        let r = (pixel_h / ch).ceil().max(1.0) as usize;
                        (c as usize, r)
                    }
                    (None, Some(r)) => {
                        let pixel_h = r as f32 * ch;
                        let pixel_w = pixel_h * (img_w / img_h);
                        let c = (pixel_w / cw).ceil().max(1.0) as usize;
                        (c, r as usize)
                    }
                    (None, None) => {
                        let c = (img_w / cw).ceil().max(1.0) as usize;
                        let r = (img_h / ch).ceil().max(1.0) as usize;
                        (c, r)
                    }
                };

                if command.is_virtual {
                    self.terminal
                        .grid
                        .virtual_placements
                        .insert(image_id, (cols, rows));
                } else {
                    let abs_line =
                        self.terminal.grid.scrollback.len() + self.terminal.grid.cursor.row;
                    self.terminal.grid.add_placement(ImagePlacement {
                        image_id,
                        placement_id,
                        line: abs_line,
                        col: self.terminal.grid.cursor.col,
                        cols,
                        rows,
                        offset_x: command.offset_x,
                        offset_y: command.offset_y,
                        z_index: command.z_index,
                    });

                    if !command.do_not_move_cursor {
                        self.terminal.grid.cursor.col = (self.terminal.grid.cursor.col + cols)
                            .min(self.terminal.grid.cols.saturating_sub(1));
                    }
                }

                let wants_ack = command.action == KittyAction::TransmitAndDisplayWithResponse
                    || command.id_explicit;
                if wants_ack && command.quiet == 0 {
                    let resp = kitty_response(image_id, ack_id, "OK");
                    self.write_pty_blocking(&resp);
                }
            }
            KittyEvent::Place { command } => {
                let Some(image_id) = command.image_id else {
                    return;
                };
                let placement_id = command.placement_id.unwrap_or(0);
                let ack_id = command.placement_id.filter(|id| *id != 0);
                if !self.terminal.grid.images.contains_key(&image_id) {
                    if command.quiet < 2 {
                        let resp = kitty_response(image_id, ack_id, "ENOENT:image not found");
                        self.write_pty_blocking(&resp);
                    }
                    return;
                }
                let cols = command.cols.unwrap_or(1) as usize;
                let rows = command.rows.unwrap_or(1) as usize;
                if command.is_virtual {
                    self.terminal
                        .grid
                        .virtual_placements
                        .insert(image_id, (cols, rows));
                } else {
                    let abs_line =
                        self.terminal.grid.scrollback.len() + self.terminal.grid.cursor.row;
                    self.terminal.grid.add_placement(ImagePlacement {
                        image_id,
                        placement_id,
                        line: abs_line,
                        col: self.terminal.grid.cursor.col,
                        cols,
                        rows,
                        offset_x: command.offset_x,
                        offset_y: command.offset_y,
                        z_index: command.z_index,
                    });
                }
                if command.quiet == 0 {
                    let resp = kitty_response(image_id, ack_id, "OK");
                    self.write_pty_blocking(&resp);
                }
            }
            KittyEvent::Delete { target } => {
                self.terminal.grid.delete_images(target);
            }
            KittyEvent::Response(resp) => {
                self.write_pty_blocking(&resp);
            }
        }
    }
}
