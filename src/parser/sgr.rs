//! Select Graphic Rendition (SGR) text formatting and color attribute processing.

use vte::Params;

use crate::color::Color;
use crate::grid::CellFlags;
use crate::parser::Terminal;

impl Terminal {
    pub(crate) fn handle_sgr(&mut self, params: &Params) {
        let mut param_list_buf: [&[u16]; 32] = [&[]; 32];
        let mut param_count = 0;
        for param in params.iter() {
            if param_count < param_list_buf.len() {
                param_list_buf[param_count] = param;
                param_count += 1;
            }
        }
        let param_list = &param_list_buf[..param_count];
        if param_list.is_empty() {
            self.reset_attributes();
            return;
        }

        let mut i = 0;
        while i < param_list.len() {
            let p = param_list[i];
            let code = p.first().copied().unwrap_or(0);
            match code {
                0 => self.reset_attributes(),
                1 => self.active_flags.insert(CellFlags::BOLD),
                2 => self.active_flags.insert(CellFlags::DIM),
                3 => self.active_flags.insert(CellFlags::ITALIC),
                4 => {
                    self.active_flags.remove(CellFlags::ALL_UNDERLINES);
                    if p.len() > 1 {
                        // Subparameters: e.g. 4:3 (undercurl)
                        match p[1] {
                            0 => {}
                            1 => self.active_flags.insert(CellFlags::UNDERLINE),
                            2 => self
                                .active_flags
                                .insert(CellFlags::UNDERLINE | CellFlags::UNDERLINE_DOUBLE),
                            3 => self
                                .active_flags
                                .insert(CellFlags::UNDERLINE | CellFlags::UNDERLINE_CURLY),
                            4 => self
                                .active_flags
                                .insert(CellFlags::UNDERLINE | CellFlags::UNDERLINE_DOTTED),
                            5 => self
                                .active_flags
                                .insert(CellFlags::UNDERLINE | CellFlags::UNDERLINE_DASHED),
                            _ => self.active_flags.insert(CellFlags::UNDERLINE),
                        }
                    } else {
                        // Plain single underline
                        self.active_flags.insert(CellFlags::UNDERLINE);
                    }
                }
                7 => self.active_flags.insert(CellFlags::REVERSE),
                8 => self.active_flags.insert(CellFlags::HIDDEN),
                9 => self.active_flags.insert(CellFlags::STRIKETHROUGH),
                22 => self.active_flags.remove(CellFlags::BOLD | CellFlags::DIM),
                23 => self.active_flags.remove(CellFlags::ITALIC),
                24 => self.active_flags.remove(CellFlags::ALL_UNDERLINES),
                27 => self.active_flags.remove(CellFlags::REVERSE),
                28 => self.active_flags.remove(CellFlags::HIDDEN),
                29 => self.active_flags.remove(CellFlags::STRIKETHROUGH),
                30..=37 => self.active_fg = Color::Indexed((code - 30) as u8),
                38 => {
                    // Extended foreground: colon subparams or semicolon-separated
                    if p.len() >= 3 && p[1] == 5 {
                        self.active_fg = Color::Indexed(p[2] as u8);
                    } else if p.len() >= 5 && p[1] == 2 {
                        let offset = if p.len() >= 6 { 3 } else { 2 };
                        self.active_fg =
                            Color::Rgb(p[offset] as u8, p[offset + 1] as u8, p[offset + 2] as u8);
                    } else if i + 2 < param_list.len() && param_list[i + 1].first() == Some(&5) {
                        if let Some(&idx) = param_list[i + 2].first() {
                            self.active_fg = Color::Indexed(idx as u8);
                            i += 2;
                        }
                    } else if i + 4 < param_list.len() && param_list[i + 1].first() == Some(&2) {
                        let r = param_list[i + 2].first().copied().unwrap_or(0) as u8;
                        let g = param_list[i + 3].first().copied().unwrap_or(0) as u8;
                        let b = param_list[i + 4].first().copied().unwrap_or(0) as u8;
                        self.active_fg = Color::Rgb(r, g, b);
                        i += 4;
                    }
                }
                39 => self.active_fg = Color::DefaultForeground,
                40..=47 => self.active_bg = Color::Indexed((code - 40) as u8),
                48 => {
                    // Extended background: colon subparams or semicolon-separated
                    if p.len() >= 3 && p[1] == 5 {
                        self.active_bg = Color::Indexed(p[2] as u8);
                    } else if p.len() >= 5 && p[1] == 2 {
                        let offset = if p.len() >= 6 { 3 } else { 2 };
                        self.active_bg =
                            Color::Rgb(p[offset] as u8, p[offset + 1] as u8, p[offset + 2] as u8);
                    } else if i + 2 < param_list.len() && param_list[i + 1].first() == Some(&5) {
                        if let Some(&idx) = param_list[i + 2].first() {
                            self.active_bg = Color::Indexed(idx as u8);
                            i += 2;
                        }
                    } else if i + 4 < param_list.len() && param_list[i + 1].first() == Some(&2) {
                        let r = param_list[i + 2].first().copied().unwrap_or(0) as u8;
                        let g = param_list[i + 3].first().copied().unwrap_or(0) as u8;
                        let b = param_list[i + 4].first().copied().unwrap_or(0) as u8;
                        self.active_bg = Color::Rgb(r, g, b);
                        i += 4;
                    }
                }
                49 => self.active_bg = Color::DefaultBackground,
                58 => {
                    // Extended underline color: colon subparams or semicolon-separated
                    if p.len() >= 3 && p[1] == 5 {
                        self.active_underline_color = Color::Indexed(p[2] as u8);
                    } else if p.len() >= 5 && p[1] == 2 {
                        let offset = if p.len() >= 6 { 3 } else { 2 };
                        self.active_underline_color =
                            Color::Rgb(p[offset] as u8, p[offset + 1] as u8, p[offset + 2] as u8);
                    } else if i + 2 < param_list.len() && param_list[i + 1].first() == Some(&5) {
                        if let Some(&idx) = param_list[i + 2].first() {
                            self.active_underline_color = Color::Indexed(idx as u8);
                            i += 2;
                        }
                    } else if i + 4 < param_list.len() && param_list[i + 1].first() == Some(&2) {
                        let r = param_list[i + 2].first().copied().unwrap_or(0) as u8;
                        let g = param_list[i + 3].first().copied().unwrap_or(0) as u8;
                        let b = param_list[i + 4].first().copied().unwrap_or(0) as u8;
                        self.active_underline_color = Color::Rgb(r, g, b);
                        i += 4;
                    }
                }
                59 => self.active_underline_color = Color::DefaultForeground,
                90..=97 => self.active_fg = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => self.active_bg = Color::Indexed((code - 100 + 8) as u8),
                _ => {}
            }
            i += 1;
        }
    }

    pub(crate) fn reset_attributes(&mut self) {
        self.active_fg = Color::DefaultForeground;
        self.active_bg = Color::DefaultBackground;
        self.active_underline_color = Color::DefaultForeground;
        self.active_flags = CellFlags::empty();
    }
}
