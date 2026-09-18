//! Control Sequence Introducer (CSI) escape sequence and DEC private mode dispatch.

use vte::Params;

use crate::grid::ClearMode;
use crate::parser::Terminal;

pub(crate) const MAX_KEYBOARD_STACK_DEPTH: usize = 64;
pub(crate) const SUPPORTED_KITTY_FLAGS: u16 = (1 | 2) as u16;

impl Terminal {
    pub(crate) fn handle_csi(
        &mut self,
        params: &Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        let is_private = intermediates.contains(&b'?');
        let mut flat_params_buf = [0u16; 32];
        let mut flat_len = 0;
        for param in params.iter() {
            for &val in param {
                if flat_len < flat_params_buf.len() {
                    flat_params_buf[flat_len] = val;
                    flat_len += 1;
                }
            }
        }
        let flat_params = &flat_params_buf[..flat_len];

        let first_param = flat_params.first().copied().unwrap_or(0);
        let param_or = |default: usize| -> usize {
            if first_param == 0 {
                default
            } else {
                first_param as usize
            }
        };

        if is_private {
            if action == 'u' {
                // CSI ? u - Query Kitty keyboard protocol flags
                self.responses
                    .push(format!("\x1b[?{}u", self.kitty_keyboard_flags).into_bytes());
                return;
            }
            if intermediates.contains(&b'$') && action == 'p' {
                // DECRQM - Request DEC Private Mode
                for mode in flat_params {
                    let status = match *mode {
                        25 => {
                            if self.grid.cursor.visible {
                                1
                            } else {
                                2
                            }
                        }
                        1004 => {
                            if self.focus_reporting {
                                1
                            } else {
                                2
                            }
                        }
                        1049 => {
                            if self.grid.is_alt_screen() {
                                1
                            } else {
                                2
                            }
                        }
                        2004 => {
                            if self.bracketed_paste {
                                1
                            } else {
                                2
                            }
                        }
                        2026 => {
                            if self.synchronized_output {
                                1
                            } else {
                                2
                            }
                        }
                        2031 => {
                            if self.report_color_scheme {
                                1
                            } else {
                                2
                            }
                        }
                        2048 => {
                            if self.report_window_size {
                                1
                            } else {
                                2
                            }
                        }
                        _ => 0, // Not recognized
                    };
                    self.responses
                        .push(format!("\x1b[?{};{}$y", mode, status).into_bytes());
                }
                return;
            }
            if action == 'h' || action == 'l' {
                let enabled = action == 'h';
                for mode in flat_params {
                    match *mode {
                        25 => self.grid.cursor.visible = enabled,
                        1049 => {
                            if enabled {
                                self.grid.enter_alt_screen();
                            } else {
                                self.grid.exit_alt_screen();
                            }
                        }
                        1004 => self.focus_reporting = enabled,
                        2004 => self.bracketed_paste = enabled,
                        2026 => {
                            if enabled {
                                self.sync_output_gen = self.sync_output_gen.wrapping_add(1);
                            }
                            self.synchronized_output = enabled;
                        }
                        2031 => {
                            self.report_color_scheme = enabled;
                            if enabled {
                                self.send_color_scheme_report();
                            }
                        }
                        2048 => {
                            self.report_window_size = enabled;
                            if enabled {
                                self.responses.push(
                                    format!(
                                        "\x1b[48;{};{};{};{}t",
                                        self.grid.rows,
                                        self.grid.cols,
                                        self.viewport_pixels[1],
                                        self.viewport_pixels[0]
                                    )
                                    .into_bytes(),
                                );
                            }
                        }
                        _ => {
                            self.mouse.apply_private_mode(*mode, enabled);
                        }
                    }
                }
            }
            return;
        }

        if action == 'u' {
            if intermediates.contains(&b'>') && flat_params.len() <= 1 {
                // CSI > flags u - Push Kitty keyboard flags
                let raw = flat_params.first().copied().unwrap_or(0);
                let flags = (raw & SUPPORTED_KITTY_FLAGS) as u8;
                if self.kitty_keyboard_stack.len() >= MAX_KEYBOARD_STACK_DEPTH {
                    self.kitty_keyboard_stack.remove(0);
                }
                self.kitty_keyboard_stack.push(self.kitty_keyboard_flags);
                self.kitty_keyboard_flags = flags;
                self.pending_kitty_keyboard = Some((flags, 1));
                return;
            } else if intermediates.contains(&b'=') || intermediates.contains(&b'>') {
                // CSI = flags ; mode u  or  CSI > flags ; mode u
                let raw = flat_params.first().copied().unwrap_or(0);
                let flags = (raw & SUPPORTED_KITTY_FLAGS) as u8;
                let mode = flat_params.get(1).copied().unwrap_or(1) as u8;
                match mode {
                    1 => self.kitty_keyboard_flags = flags,
                    2 => self.kitty_keyboard_flags |= flags,
                    3 => self.kitty_keyboard_flags &= !flags,
                    _ => {}
                }
                self.pending_kitty_keyboard = Some((flags, mode));
                return;
            } else if intermediates.contains(&b'<') {
                // CSI < count u - Pop Kitty keyboard flags
                let count = flat_params.first().copied().unwrap_or(1).max(1) as usize;
                for _ in 0..count {
                    if let Some(f) = self.kitty_keyboard_stack.pop() {
                        self.kitty_keyboard_flags = f;
                    } else {
                        // Empty stack resets to 0 according to Kitty keyboard protocol
                        self.kitty_keyboard_flags = 0;
                    }
                }
                self.pending_kitty_keyboard = Some((self.kitty_keyboard_flags, 1));
                return;
            }
        }

        match action {
            // CUU - Cursor Up
            'A' => {
                let count = param_or(1);
                self.grid.cursor.row = self.grid.cursor.row.saturating_sub(count);
            }
            // CUD - Cursor Down
            'B' => {
                let count = param_or(1);
                self.grid.cursor.row =
                    (self.grid.cursor.row + count).min(self.grid.rows.saturating_sub(1));
            }
            // CUF - Cursor Forward
            'C' => {
                let count = param_or(1);
                self.grid.cursor.col =
                    (self.grid.cursor.col + count).min(self.grid.cols.saturating_sub(1));
            }
            // CUB - Cursor Back
            'D' => {
                let count = param_or(1);
                self.grid.cursor.col = self.grid.cursor.col.saturating_sub(count);
            }
            // CNL - Cursor Next Line
            'E' => {
                let count = param_or(1);
                self.grid.cursor.row =
                    (self.grid.cursor.row + count).min(self.grid.rows.saturating_sub(1));
                self.grid.cursor.col = 0;
            }
            // CPL - Cursor Previous Line
            'F' => {
                let count = param_or(1);
                self.grid.cursor.row = self.grid.cursor.row.saturating_sub(count);
                self.grid.cursor.col = 0;
            }
            // CHA / HPA - Cursor Character Absolute
            'G' | '\'' => {
                let col = param_or(1).saturating_sub(1);
                self.grid.cursor.col = col.min(self.grid.cols.saturating_sub(1));
            }
            // CUP / HVP - Cursor Position
            'H' | 'f' => {
                let row = flat_params
                    .first()
                    .copied()
                    .unwrap_or(1)
                    .max(1)
                    .saturating_sub(1) as usize;
                let col = flat_params
                    .get(1)
                    .copied()
                    .unwrap_or(1)
                    .max(1)
                    .saturating_sub(1) as usize;
                self.grid.cursor.row = row.min(self.grid.rows.saturating_sub(1));
                self.grid.cursor.col = col.min(self.grid.cols.saturating_sub(1));
            }
            // ED - Erase in Display
            'J' => {
                let mode = match first_param {
                    1 => ClearMode::Above,
                    2 => ClearMode::All,
                    3 => ClearMode::Saved,
                    _ => ClearMode::Below,
                };
                self.grid.clear_screen(mode);
            }
            // EL - Erase in Line
            'K' => {
                let mode = match first_param {
                    1 => ClearMode::Above,
                    2 => ClearMode::All,
                    _ => ClearMode::Below,
                };
                self.grid.clear_line(mode);
            }
            // IL - Insert Lines
            'L' => self.grid.insert_lines(param_or(1)),
            // DL - Delete Lines
            'M' => self.grid.delete_lines(param_or(1)),
            // DCH - Delete Characters
            'P' => self.grid.delete_chars(param_or(1)),
            // ICH - Insert Blank Characters
            '@' => self.grid.insert_blank_chars(param_or(1)),
            // SU - Scroll Up
            'S' => self.grid.scroll_up(param_or(1)),
            // SD - Scroll Down
            'T' => self.grid.scroll_down(param_or(1)),
            // VPA - Line Position Absolute
            'd' => {
                let row = param_or(1).saturating_sub(1);
                self.grid.cursor.row = row.min(self.grid.rows.saturating_sub(1));
            }
            // DSR - Device Status Report
            'n' => match first_param {
                5 => self.responses.push(b"\x1b[0n".to_vec()),
                6 => self.responses.push(
                    format!(
                        "\x1b[{};{}R",
                        self.grid.cursor.row + 1,
                        self.grid.cursor.col + 1
                    )
                    .into_bytes(),
                ),
                _ => {}
            },
            // DA - Device Attributes
            'c' => {
                if intermediates.contains(&b'>') {
                    // Secondary Device Attributes (DA2)
                    self.responses.push(b"\x1b[>0;10;1c".to_vec());
                } else if first_param == 0 {
                    // Primary Device Attributes (DA1) - VT220 response
                    self.responses.push(b"\x1b[?62;c".to_vec());
                }
            }
            // SGR - Select Graphic Rendition
            'm' => self.handle_sgr(params),
            // DECSTBM - Set Scrolling Region
            'r' => {
                let top = flat_params
                    .first()
                    .copied()
                    .unwrap_or(1)
                    .max(1)
                    .saturating_sub(1) as usize;
                let bottom = flat_params
                    .get(1)
                    .copied()
                    .unwrap_or(self.grid.rows as u16)
                    .max(1)
                    .saturating_sub(1) as usize;
                self.grid.set_scroll_region(top, bottom);
            }
            // Save cursor position
            's' => self.grid.save_cursor(),
            // Restore cursor position
            'u' => self.grid.restore_cursor(),
            // Window manipulation: image clients query pixel geometry before placing images.
            't' => match first_param {
                14 => self.responses.push(
                    format!(
                        "\x1b[4;{};{}t",
                        self.viewport_pixels[1], self.viewport_pixels[0]
                    )
                    .into_bytes(),
                ),
                16 => self.responses.push(
                    format!("\x1b[6;{};{}t", self.cell_pixels[1], self.cell_pixels[0]).into_bytes(),
                ),
                18 => self
                    .responses
                    .push(format!("\x1b[8;{};{}t", self.grid.rows, self.grid.cols).into_bytes()),
                _ => {}
            },
            _ => {}
        }
    }
}
