//! Declarative TOML configuration and include resolution.

pub mod include;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::color::{Rgb, default_256_palette};
use crate::grid::CursorShape;

pub use include::{Include, default_config_path, resolve_path};

const DEFAULT_FONT_FAMILY: &str = "monospace";
const DEFAULT_FONT_SIZE: f32 = 14.0;
const DEFAULT_COLUMNS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
const DEFAULT_FOREGROUND: Rgb = Rgb::new(220, 220, 220);
const DEFAULT_BACKGROUND: Rgb = Rgb::new(24, 24, 24);

/// Font family configuration supporting a single family name or an ordered fallback chain.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum FontFamilies {
    Single(String),
    Multiple(Vec<String>),
}

impl FontFamilies {
    #[must_use]
    pub fn to_vec(&self) -> Vec<String> {
        match self {
            Self::Single(name) => vec![name.clone()],
            Self::Multiple(names) => names.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
pub struct FontConfig {
    #[serde(alias = "families")]
    pub family: Option<FontFamilies>,
    pub size: Option<f32>,
    pub subpixel: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct WindowConfig {
    pub columns: Option<u16>,
    pub rows: Option<u16>,
    pub padding_x: Option<u16>,
    pub padding_y: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct CursorConfig {
    pub shape: Option<CursorShape>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct PaletteConfig {
    pub black: Option<Rgb>,
    pub red: Option<Rgb>,
    pub green: Option<Rgb>,
    pub yellow: Option<Rgb>,
    pub blue: Option<Rgb>,
    pub magenta: Option<Rgb>,
    pub cyan: Option<Rgb>,
    pub white: Option<Rgb>,
    pub bright_black: Option<Rgb>,
    pub bright_red: Option<Rgb>,
    pub bright_green: Option<Rgb>,
    pub bright_yellow: Option<Rgb>,
    pub bright_blue: Option<Rgb>,
    pub bright_magenta: Option<Rgb>,
    pub bright_cyan: Option<Rgb>,
    pub bright_white: Option<Rgb>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct ColorsConfig {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub palette: Option<PaletteConfig>,
    pub indexed: Option<HashMap<u8, Rgb>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize, Default)]
pub struct ScrollbackConfig {
    pub lines: Option<u32>,
    pub multiplier: Option<f32>,
    pub auto_scroll: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct ClipboardConfig {
    pub allow_osc52_read: Option<bool>,
    pub allow_osc52_write: Option<bool>,
}

/// A single key combination string or a list of alternatives.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum KeyCombos {
    Single(String),
    Multiple(Vec<String>),
}

impl KeyCombos {
    #[must_use]
    pub fn to_combos(&self) -> Vec<&str> {
        match self {
            Self::Single(s) => {
                if s.eq_ignore_ascii_case("none") {
                    Vec::new()
                } else {
                    vec![s.as_str()]
                }
            }
            Self::Multiple(v) => v
                .iter()
                .map(String::as_str)
                .filter(|s| !s.eq_ignore_ascii_case("none"))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct KeybindingsConfig {
    pub scrollback_up_page: Option<KeyCombos>,
    pub scrollback_down_page: Option<KeyCombos>,
    pub scrollback_up_line: Option<KeyCombos>,
    pub scrollback_down_line: Option<KeyCombos>,
    pub scrollback_home: Option<KeyCombos>,
    pub scrollback_end: Option<KeyCombos>,
    pub prompt_prev: Option<KeyCombos>,
    pub prompt_next: Option<KeyCombos>,
    pub font_increase: Option<KeyCombos>,
    pub font_decrease: Option<KeyCombos>,
    pub font_reset: Option<KeyCombos>,
    pub clipboard_copy: Option<KeyCombos>,
    pub clipboard_paste: Option<KeyCombos>,
    pub primary_paste: Option<KeyCombos>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
pub struct Config {
    pub include: Option<Include>,
    #[serde(default)]
    pub window: WindowConfig,
    #[serde(default)]
    pub font: FontConfig,
    #[serde(default)]
    pub cursor: CursorConfig,
    #[serde(default)]
    pub colors: ColorsConfig,
    #[serde(default)]
    pub scrollback: ScrollbackConfig,
    #[serde(default)]
    pub clipboard: ClipboardConfig,
    #[serde(default)]
    pub keybindings: KeybindingsConfig,
}

impl Config {
    /// Loads a configuration from a file path, recursively resolving any `include` directives.
    ///
    /// # Errors
    /// Returns [`std::io::Error`] if the file cannot be read, contains syntax errors, or circular includes occur.
    pub fn load(path: &Path) -> io::Result<Self> {
        let mut visited = HashSet::new();
        include::load_internal(path, &mut visited)
    }

    /// Loads the configuration from the given path if provided, or from the default XDG location.
    /// If the default file does not exist, returns `Config::default()`.
    ///
    /// # Errors
    /// Returns [`std::io::Error`] if an explicit path is invalid or if existing config files fail to parse.
    pub fn load_from_path_or_default(custom_path: Option<&Path>) -> io::Result<Self> {
        if let Some(path) = custom_path {
            return Self::load(path);
        }
        if let Some(default_path) = default_config_path()
            && default_path.exists()
        {
            return Self::load(&default_path);
        }
        Ok(Self::default())
    }

    /// Merges `other` into `self`, where explicit options in `other` take precedence.
    pub fn merge(&mut self, other: Self) {
        if let Some(family) = other.font.family {
            self.font.family = Some(family);
        }
        if let Some(size) = other.font.size {
            self.font.size = Some(size);
        }
        if let Some(subpixel) = other.font.subpixel {
            self.font.subpixel = Some(subpixel);
        }

        if let Some(cols) = other.window.columns {
            self.window.columns = Some(cols);
        }
        if let Some(rows) = other.window.rows {
            self.window.rows = Some(rows);
        }
        if let Some(px) = other.window.padding_x {
            self.window.padding_x = Some(px);
        }
        if let Some(py) = other.window.padding_y {
            self.window.padding_y = Some(py);
        }

        if let Some(shape) = other.cursor.shape {
            self.cursor.shape = Some(shape);
        }

        if let Some(fg) = other.colors.foreground {
            self.colors.foreground = Some(fg);
        }
        if let Some(bg) = other.colors.background {
            self.colors.background = Some(bg);
        }

        if let Some(other_pal) = other.colors.palette {
            let pal = self
                .colors
                .palette
                .get_or_insert_with(PaletteConfig::default);
            include::merge_palette(pal, other_pal);
        }

        if let Some(other_indexed) = other.colors.indexed {
            let indexed = self.colors.indexed.get_or_insert_with(HashMap::new);
            for (k, v) in other_indexed {
                indexed.insert(k, v);
            }
        }

        if let Some(lines) = other.scrollback.lines {
            self.scrollback.lines = Some(lines);
        }
        if let Some(mul) = other.scrollback.multiplier {
            self.scrollback.multiplier = Some(mul);
        }
        if let Some(auto) = other.scrollback.auto_scroll {
            self.scrollback.auto_scroll = Some(auto);
        }

        if let Some(r) = other.clipboard.allow_osc52_read {
            self.clipboard.allow_osc52_read = Some(r);
        }
        if let Some(w) = other.clipboard.allow_osc52_write {
            self.clipboard.allow_osc52_write = Some(w);
        }

        include::merge_keybindings(&mut self.keybindings, other.keybindings);
    }

    #[must_use]
    pub fn font_family(&self) -> &str {
        self.font
            .family
            .as_ref()
            .map(|f| match f {
                FontFamilies::Single(s) => s.as_str(),
                FontFamilies::Multiple(v) => {
                    v.first().map(String::as_str).unwrap_or(DEFAULT_FONT_FAMILY)
                }
            })
            .unwrap_or(DEFAULT_FONT_FAMILY)
    }

    #[must_use]
    pub fn font_families(&self) -> Vec<String> {
        self.font
            .family
            .as_ref()
            .map(FontFamilies::to_vec)
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec![DEFAULT_FONT_FAMILY.to_string()])
    }

    #[must_use]
    pub fn font_size(&self) -> f32 {
        self.font.size.unwrap_or(DEFAULT_FONT_SIZE)
    }

    #[must_use]
    pub fn font_subpixel(&self) -> bool {
        self.font.subpixel.unwrap_or(false)
    }

    #[must_use]
    pub fn columns(&self) -> u16 {
        self.window.columns.unwrap_or(DEFAULT_COLUMNS)
    }

    #[must_use]
    pub fn rows(&self) -> u16 {
        self.window.rows.unwrap_or(DEFAULT_ROWS)
    }

    #[must_use]
    pub fn padding_x(&self) -> u16 {
        self.window.padding_x.unwrap_or(0)
    }

    #[must_use]
    pub fn padding_y(&self) -> u16 {
        self.window.padding_y.unwrap_or(0)
    }

    #[must_use]
    pub fn scrollback_lines(&self) -> usize {
        self.scrollback.lines.unwrap_or(1000) as usize
    }

    #[must_use]
    pub fn scroll_multiplier(&self) -> f32 {
        self.scrollback.multiplier.unwrap_or(3.0).clamp(0.1, 100.0)
    }

    #[must_use]
    pub fn allow_osc52_read(&self) -> bool {
        self.clipboard.allow_osc52_read.unwrap_or(false)
    }

    #[must_use]
    pub fn allow_osc52_write(&self) -> bool {
        self.clipboard.allow_osc52_write.unwrap_or(true)
    }

    #[must_use]
    pub fn auto_scroll(&self) -> bool {
        self.scrollback.auto_scroll.unwrap_or(true)
    }

    #[must_use]
    pub fn cursor_shape(&self) -> CursorShape {
        self.cursor.shape.unwrap_or_default()
    }

    #[must_use]
    pub fn foreground(&self) -> Rgb {
        self.colors.foreground.unwrap_or(DEFAULT_FOREGROUND)
    }

    #[must_use]
    pub fn background(&self) -> Rgb {
        self.colors.background.unwrap_or(DEFAULT_BACKGROUND)
    }

    /// Constructs the 256-color palette applying standard ANSI and indexed overrides.
    #[must_use]
    pub fn build_palette(&self) -> [Rgb; 256] {
        let mut palette = default_256_palette();

        if let Some(p) = &self.colors.palette {
            if let Some(c) = p.black {
                palette[0] = c;
            }
            if let Some(c) = p.red {
                palette[1] = c;
            }
            if let Some(c) = p.green {
                palette[2] = c;
            }
            if let Some(c) = p.yellow {
                palette[3] = c;
            }
            if let Some(c) = p.blue {
                palette[4] = c;
            }
            if let Some(c) = p.magenta {
                palette[5] = c;
            }
            if let Some(c) = p.cyan {
                palette[6] = c;
            }
            if let Some(c) = p.white {
                palette[7] = c;
            }
            if let Some(c) = p.bright_black {
                palette[8] = c;
            }
            if let Some(c) = p.bright_red {
                palette[9] = c;
            }
            if let Some(c) = p.bright_green {
                palette[10] = c;
            }
            if let Some(c) = p.bright_yellow {
                palette[11] = c;
            }
            if let Some(c) = p.bright_blue {
                palette[12] = c;
            }
            if let Some(c) = p.bright_magenta {
                palette[13] = c;
            }
            if let Some(c) = p.bright_cyan {
                palette[14] = c;
            }
            if let Some(c) = p.bright_white {
                palette[15] = c;
            }
        }

        if let Some(indexed) = &self.colors.indexed {
            for (&idx, &color) in indexed {
                palette[idx as usize] = color;
            }
        }

        palette
    }
}
