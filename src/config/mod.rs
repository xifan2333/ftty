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
use crate::input::{KeyAction, Modifiers, canonicalize_sym, parse_key_combo};
use xkbcommon::xkb;

pub use include::{Include, default_config_path, resolve_path};

const DEFAULT_FONT_FAMILY: &str = "monospace";
const DEFAULT_FONT_SIZE: f32 = 9.0;
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

/// FreeType hinting and load target options (aligned with WezTerm).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FreeTypeLoadTarget {
    Normal,
    #[default]
    Light,
    Mono,
    HorizontalLcd,
}

impl<'de> Deserialize<'de> for FreeTypeLoadTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let norm: String = s
            .chars()
            .filter(|c| *c != '_' && *c != '-')
            .flat_map(char::to_lowercase)
            .collect();
        match norm.as_str() {
            "normal" => Ok(Self::Normal),
            "light" => Ok(Self::Light),
            "mono" => Ok(Self::Mono),
            "horizontallcd" => Ok(Self::HorizontalLcd),
            _ => Err(serde::de::Error::custom(format!(
                "invalid freetype_load_target: '{s}', expected 'Normal', 'Light', 'Mono', or 'HorizontalLcd'"
            ))),
        }
    }
}

impl Serialize for FreeTypeLoadTarget {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Normal => serializer.serialize_str("Normal"),
            Self::Light => serializer.serialize_str("Light"),
            Self::Mono => serializer.serialize_str("Mono"),
            Self::HorizontalLcd => serializer.serialize_str("HorizontalLcd"),
        }
    }
}

/// FreeType rasterization render target options (aligned with WezTerm).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FreeTypeRenderTarget {
    Normal,
    Light,
    Mono,
    #[default]
    HorizontalLcd,
}

impl<'de> Deserialize<'de> for FreeTypeRenderTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let norm: String = s
            .chars()
            .filter(|c| *c != '_' && *c != '-')
            .flat_map(char::to_lowercase)
            .collect();
        match norm.as_str() {
            "normal" => Ok(Self::Normal),
            "light" => Ok(Self::Light),
            "mono" => Ok(Self::Mono),
            "horizontallcd" => Ok(Self::HorizontalLcd),
            _ => Err(serde::de::Error::custom(format!(
                "invalid freetype_render_target: '{s}', expected 'Normal', 'Light', 'Mono', or 'HorizontalLcd'"
            ))),
        }
    }
}

impl Serialize for FreeTypeRenderTarget {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Normal => serializer.serialize_str("Normal"),
            Self::Light => serializer.serialize_str("Light"),
            Self::Mono => serializer.serialize_str("Mono"),
            Self::HorizontalLcd => serializer.serialize_str("HorizontalLcd"),
        }
    }
}

/// FreeType load flags controlling hinting algorithms (aligned with WezTerm).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FreeTypeLoadFlags {
    #[default]
    Default,
    NoHinting,
}

impl<'de> Deserialize<'de> for FreeTypeLoadFlags {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let norm: String = s
            .chars()
            .filter(|c| *c != '_' && *c != '-')
            .flat_map(char::to_lowercase)
            .collect();
        match norm.as_str() {
            "default" => Ok(Self::Default),
            "nohinting" => Ok(Self::NoHinting),
            _ => Err(serde::de::Error::custom(format!(
                "invalid freetype_load_flags: '{s}', expected 'DEFAULT' or 'NO_HINTING'"
            ))),
        }
    }
}

impl Serialize for FreeTypeLoadFlags {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Default => serializer.serialize_str("DEFAULT"),
            Self::NoHinting => serializer.serialize_str("NO_HINTING"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
pub struct FontConfig {
    #[serde(alias = "families")]
    pub family: Option<FontFamilies>,
    pub size: Option<f32>,
    pub freetype_load_target: Option<FreeTypeLoadTarget>,
    pub freetype_render_target: Option<FreeTypeRenderTarget>,
    pub freetype_load_flags: Option<FreeTypeLoadFlags>,
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct LegacyPaletteConfig {
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
    pub indexed: Option<HashMap<u8, Rgb>>,

    /// Backward compatibility: accept legacy `[colors.palette]` table and fold into flat fields.
    #[serde(default)]
    pub palette: Option<LegacyPaletteConfig>,
}

impl ColorsConfig {
    pub fn fold_legacy_palette(&mut self) {
        if let Some(p) = self.palette.take() {
            if self.black.is_none() {
                self.black = p.black;
            }
            if self.red.is_none() {
                self.red = p.red;
            }
            if self.green.is_none() {
                self.green = p.green;
            }
            if self.yellow.is_none() {
                self.yellow = p.yellow;
            }
            if self.blue.is_none() {
                self.blue = p.blue;
            }
            if self.magenta.is_none() {
                self.magenta = p.magenta;
            }
            if self.cyan.is_none() {
                self.cyan = p.cyan;
            }
            if self.white.is_none() {
                self.white = p.white;
            }
            if self.bright_black.is_none() {
                self.bright_black = p.bright_black;
            }
            if self.bright_red.is_none() {
                self.bright_red = p.bright_red;
            }
            if self.bright_green.is_none() {
                self.bright_green = p.bright_green;
            }
            if self.bright_yellow.is_none() {
                self.bright_yellow = p.bright_yellow;
            }
            if self.bright_blue.is_none() {
                self.bright_blue = p.bright_blue;
            }
            if self.bright_magenta.is_none() {
                self.bright_magenta = p.bright_magenta;
            }
            if self.bright_cyan.is_none() {
                self.bright_cyan = p.bright_cyan;
            }
            if self.bright_white.is_none() {
                self.bright_white = p.bright_white;
            }
        }
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedAction {
    Action(KeyAction),
    Unbind,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ActionDef {
    Simple(String),
    Multiple(Vec<String>),
    Pipe(PipeActionDef),
}

impl ActionDef {
    #[must_use]
    pub fn to_resolved_action(&self) -> ResolvedAction {
        match self {
            Self::Simple(s) => match s.to_ascii_lowercase().as_str() {
                "scrollback_up_page" => ResolvedAction::Action(KeyAction::ScrollbackUpPage),
                "scrollback_down_page" => ResolvedAction::Action(KeyAction::ScrollbackDownPage),
                "scrollback_up_line" => ResolvedAction::Action(KeyAction::ScrollbackUpLine),
                "scrollback_down_line" => ResolvedAction::Action(KeyAction::ScrollbackDownLine),
                "scrollback_home" => ResolvedAction::Action(KeyAction::ScrollbackHome),
                "scrollback_end" => ResolvedAction::Action(KeyAction::ScrollbackEnd),
                "prompt_prev" => ResolvedAction::Action(KeyAction::PromptPrev),
                "prompt_next" => ResolvedAction::Action(KeyAction::PromptNext),
                "font_increase" => ResolvedAction::Action(KeyAction::FontIncrease),
                "font_decrease" => ResolvedAction::Action(KeyAction::FontDecrease),
                "font_reset" => ResolvedAction::Action(KeyAction::FontReset),
                "clipboard_copy" => ResolvedAction::Action(KeyAction::ClipboardCopy),
                "clipboard_paste" => ResolvedAction::Action(KeyAction::ClipboardPaste),
                "primary_paste" => ResolvedAction::Action(KeyAction::PrimaryPaste),
                "none" | "" => ResolvedAction::Unbind,
                _ => {
                    eprintln!("ftty: unrecognized keybinding action '{s}'");
                    ResolvedAction::Invalid
                }
            },
            Self::Multiple(_) => ResolvedAction::Invalid,
            Self::Pipe(pipe) => match pipe {
                PipeActionDef::PipeVisible(cmd) => {
                    ResolvedAction::Action(KeyAction::PipeVisible(cmd.clone().into_vec()))
                }
                PipeActionDef::PipeScrollback(cmd) => {
                    ResolvedAction::Action(KeyAction::PipeScrollback(cmd.clone().into_vec()))
                }
                PipeActionDef::PipeSelection(cmd) => {
                    ResolvedAction::Action(KeyAction::PipeSelection(cmd.clone().into_vec()))
                }
            },
        }
    }
}

const KNOWN_ACTIONS: &[&str] = &[
    "scrollback_up_page",
    "scrollback_down_page",
    "scrollback_up_line",
    "scrollback_down_line",
    "scrollback_home",
    "scrollback_end",
    "prompt_prev",
    "prompt_next",
    "font_increase",
    "font_decrease",
    "font_reset",
    "clipboard_copy",
    "clipboard_paste",
    "primary_paste",
];

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(transparent)]
pub struct KeybindingsConfig {
    pub bindings: HashMap<String, ActionDef>,
}

impl KeybindingsConfig {
    #[must_use]
    pub fn resolve_bindings_map(&self) -> HashMap<(Modifiers, xkb::Keysym), KeyAction> {
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
            if let Some((mods, sym)) = parse_key_combo(combo_str) {
                let canonical_sym = canonicalize_sym(sym);
                map.insert((mods, canonical_sym), action);
            }
        }

        for (key_str, def) in &self.bindings {
            let key_lower = key_str.to_ascii_lowercase();
            if KNOWN_ACTIONS.contains(&key_lower.as_str()) {
                let action_res = ActionDef::Simple(key_lower).to_resolved_action();
                if let ResolvedAction::Action(act) = action_res {
                    match def {
                        ActionDef::Simple(c) => {
                            if let Some((mods, sym)) = parse_key_combo(c) {
                                let canonical_sym = canonicalize_sym(sym);
                                map.insert((mods, canonical_sym), act);
                            }
                        }
                        ActionDef::Multiple(combos) => {
                            for c in combos {
                                if let Some((mods, sym)) = parse_key_combo(c) {
                                    let canonical_sym = canonicalize_sym(sym);
                                    map.insert((mods, canonical_sym), act.clone());
                                }
                            }
                        }
                        ActionDef::Pipe(_) => {}
                    }
                }
                continue;
            }

            if let Some((mods, sym)) = parse_key_combo(key_str) {
                let canonical_sym = canonicalize_sym(sym);
                match def.to_resolved_action() {
                    ResolvedAction::Action(action) => {
                        map.insert((mods, canonical_sym), action);
                    }
                    ResolvedAction::Unbind => {
                        map.remove(&(mods, canonical_sym));
                    }
                    ResolvedAction::Invalid => {}
                }
            }
        }

        map
    }

    #[must_use]
    pub fn resolve_bindings(&self) -> Vec<((Modifiers, xkb::Keysym), KeyAction)> {
        self.resolve_bindings_map().into_iter().collect()
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
        if let Some(load_target) = other.font.freetype_load_target {
            self.font.freetype_load_target = Some(load_target);
        }
        if let Some(render_target) = other.font.freetype_render_target {
            self.font.freetype_render_target = Some(render_target);
        }
        if let Some(load_flags) = other.font.freetype_load_flags {
            self.font.freetype_load_flags = Some(load_flags);
        }

        if let Some(p) = other.window.padding {
            self.window.padding = Some(p);
        }

        if let Some(shape) = other.cursor.shape {
            self.cursor.shape = Some(shape);
        }

        include::merge_colors(&mut self.colors, other.colors);

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
    pub fn freetype_load_target(&self) -> FreeTypeLoadTarget {
        self.font.freetype_load_target.unwrap_or_default()
    }

    #[must_use]
    pub fn freetype_render_target(&self) -> FreeTypeRenderTarget {
        self.font.freetype_render_target.unwrap_or_default()
    }

    #[must_use]
    pub fn freetype_load_flags(&self) -> FreeTypeLoadFlags {
        self.font.freetype_load_flags.unwrap_or_default()
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

        let c = &self.colors;
        let leg = c.palette.as_ref();
        if let Some(col) = c.black.or_else(|| leg.and_then(|p| p.black)) {
            palette[0] = col;
        }
        if let Some(col) = c.red.or_else(|| leg.and_then(|p| p.red)) {
            palette[1] = col;
        }
        if let Some(col) = c.green.or_else(|| leg.and_then(|p| p.green)) {
            palette[2] = col;
        }
        if let Some(col) = c.yellow.or_else(|| leg.and_then(|p| p.yellow)) {
            palette[3] = col;
        }
        if let Some(col) = c.blue.or_else(|| leg.and_then(|p| p.blue)) {
            palette[4] = col;
        }
        if let Some(col) = c.magenta.or_else(|| leg.and_then(|p| p.magenta)) {
            palette[5] = col;
        }
        if let Some(col) = c.cyan.or_else(|| leg.and_then(|p| p.cyan)) {
            palette[6] = col;
        }
        if let Some(col) = c.white.or_else(|| leg.and_then(|p| p.white)) {
            palette[7] = col;
        }
        if let Some(col) = c.bright_black.or_else(|| leg.and_then(|p| p.bright_black)) {
            palette[8] = col;
        }
        if let Some(col) = c.bright_red.or_else(|| leg.and_then(|p| p.bright_red)) {
            palette[9] = col;
        }
        if let Some(col) = c.bright_green.or_else(|| leg.and_then(|p| p.bright_green)) {
            palette[10] = col;
        }
        if let Some(col) = c
            .bright_yellow
            .or_else(|| leg.and_then(|p| p.bright_yellow))
        {
            palette[11] = col;
        }
        if let Some(col) = c.bright_blue.or_else(|| leg.and_then(|p| p.bright_blue)) {
            palette[12] = col;
        }
        if let Some(col) = c
            .bright_magenta
            .or_else(|| leg.and_then(|p| p.bright_magenta))
        {
            palette[13] = col;
        }
        if let Some(col) = c.bright_cyan.or_else(|| leg.and_then(|p| p.bright_cyan)) {
            palette[14] = col;
        }
        if let Some(col) = c.bright_white.or_else(|| leg.and_then(|p| p.bright_white)) {
            palette[15] = col;
        }

        if let Some(indexed) = &self.colors.indexed {
            for (&idx, &color) in indexed {
                palette[idx as usize] = color;
            }
        }

        palette
    }
}
