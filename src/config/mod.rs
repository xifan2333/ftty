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
use crate::input::{KeyAction, Modifiers, parse_key_combo};
use xkbcommon::xkb;

pub use include::{Include, default_config_path, resolve_path};

const DEFAULT_FONT_FAMILY: &str = "monospace";
const DEFAULT_FONT_SIZE: f32 = 14.0;
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
}

/// Padding configuration: either a uniform scalar `padding = 4` or an array `padding = [4, 2]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Padding {
    Uniform(u16),
    Axes([u16; 2]),
}

impl Padding {
    #[must_use]
    pub const fn to_axes(self) -> [u16; 2] {
        match self {
            Self::Uniform(v) => [v, v],
            Self::Axes([x, y]) => [x, y],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct WindowConfig {
    pub padding: Option<Padding>,
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

/// Security policies controlling OSC 52 remote clipboard access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct ClipboardConfig {
    /// Whether applications running in the terminal are permitted to query/read the system clipboard.
    ///
    /// Defaults to `false` for security (preventing untrusted remote SSH processes from silently
    /// exfiltrating local clipboard tokens or passwords).
    pub allow_osc52_read: Option<bool>,
    /// Whether applications running in the terminal are permitted to set or clear the system clipboard.
    ///
    /// Defaults to `true` for standard terminal compatibility.
    pub allow_osc52_write: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum CommandDef {
    Single(String),
    List(Vec<String>),
}

impl CommandDef {
    #[must_use]
    pub fn into_vec(self) -> Vec<String> {
        match self {
            Self::Single(s) => vec!["sh".to_string(), "-c".to_string(), s],
            Self::List(v) => v,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PipeActionDef {
    PipeVisible(CommandDef),
    PipeScrollback(CommandDef),
    PipeSelection(CommandDef),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ActionDef {
    Simple(String),
    Pipe(PipeActionDef),
}

impl ActionDef {
    #[must_use]
    pub fn to_key_action(&self) -> Option<KeyAction> {
        match self {
            Self::Simple(s) => match s.to_ascii_lowercase().as_str() {
                "scrollback_up_page" => Some(KeyAction::ScrollbackUpPage),
                "scrollback_down_page" => Some(KeyAction::ScrollbackDownPage),
                "scrollback_up_line" => Some(KeyAction::ScrollbackUpLine),
                "scrollback_down_line" => Some(KeyAction::ScrollbackDownLine),
                "scrollback_home" => Some(KeyAction::ScrollbackHome),
                "scrollback_end" => Some(KeyAction::ScrollbackEnd),
                "prompt_prev" => Some(KeyAction::PromptPrev),
                "prompt_next" => Some(KeyAction::PromptNext),
                "font_increase" => Some(KeyAction::FontIncrease),
                "font_decrease" => Some(KeyAction::FontDecrease),
                "font_reset" => Some(KeyAction::FontReset),
                "clipboard_copy" => Some(KeyAction::ClipboardCopy),
                "clipboard_paste" => Some(KeyAction::ClipboardPaste),
                "primary_paste" => Some(KeyAction::PrimaryPaste),
                "none" | "" => None,
                _ => None,
            },
            Self::Pipe(pipe) => match pipe {
                PipeActionDef::PipeVisible(cmd) => {
                    Some(KeyAction::PipeVisible(cmd.clone().into_vec()))
                }
                PipeActionDef::PipeScrollback(cmd) => {
                    Some(KeyAction::PipeScrollback(cmd.clone().into_vec()))
                }
                PipeActionDef::PipeSelection(cmd) => {
                    Some(KeyAction::PipeSelection(cmd.clone().into_vec()))
                }
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(transparent)]
pub struct KeybindingsConfig {
    pub bindings: HashMap<String, ActionDef>,
}

impl KeybindingsConfig {
    #[must_use]
    pub fn resolve_bindings(&self) -> Vec<((Modifiers, xkb::Keysym), KeyAction)> {
        let mut map = HashMap::new();

        let defaults = [
            ("Shift+Page_Up", KeyAction::ScrollbackUpPage),
            ("Shift+KP_Page_Up", KeyAction::ScrollbackUpPage),
            ("Shift+Page_Down", KeyAction::ScrollbackDownPage),
            ("Shift+KP_Page_Down", KeyAction::ScrollbackDownPage),
            ("Ctrl+Shift+Up", KeyAction::ScrollbackUpLine),
            ("Ctrl+Shift+Down", KeyAction::ScrollbackDownLine),
            ("Shift+Home", KeyAction::ScrollbackHome),
            ("Shift+End", KeyAction::ScrollbackEnd),
            ("Ctrl+Shift+Z", KeyAction::PromptPrev),
            ("Ctrl+Shift+X", KeyAction::PromptNext),
            ("Ctrl+plus", KeyAction::FontIncrease),
            ("Ctrl+equal", KeyAction::FontIncrease),
            ("Ctrl+minus", KeyAction::FontDecrease),
            ("Ctrl+0", KeyAction::FontReset),
            ("Ctrl+Shift+c", KeyAction::ClipboardCopy),
            ("Ctrl+Insert", KeyAction::ClipboardCopy),
            ("Ctrl+Shift+v", KeyAction::ClipboardPaste),
            ("Shift+Insert", KeyAction::PrimaryPaste),
        ];

        for (combo_str, action) in defaults {
            if let Some(parsed) = parse_key_combo(combo_str) {
                map.insert(parsed, action);
            }
        }

        for (combo_str, def) in &self.bindings {
            if let Some(parsed) = parse_key_combo(combo_str) {
                if let Some(action) = def.to_key_action() {
                    map.insert(parsed, action);
                } else {
                    map.remove(&parsed);
                }
            }
        }

        map.into_iter().collect()
    }
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

        if let Some(p) = other.window.padding {
            self.window.padding = Some(p);
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
    pub fn padding(&self) -> [u16; 2] {
        self.window.padding.map_or([0, 0], Padding::to_axes)
    }

    #[must_use]
    pub fn padding_x(&self) -> u16 {
        self.padding()[0]
    }

    #[must_use]
    pub fn padding_y(&self) -> u16 {
        self.padding()[1]
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
