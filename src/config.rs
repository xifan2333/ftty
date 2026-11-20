//! Declarative TOML configuration and include resolution.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::color::{Rgb, default_256_palette};
use crate::grid::CursorShape;
use crate::input::{KeyAction, Modifiers, canonicalize_sym, parse_key_combo};
use xkbcommon::xkb;

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

/// Width model for East Asian Ambiguous characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AmbiguousWidth {
    #[default]
    Narrow,
    Wide,
}

impl AmbiguousWidth {
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::Narrow => 1,
            Self::Wide => 2,
        }
    }
}

impl<'de> Deserialize<'de> for AmbiguousWidth {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = AmbiguousWidth;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str(r#""narrow", "wide", 1, or 2"#)
            }

            fn visit_str<E>(self, value: &str) -> Result<AmbiguousWidth, E>
            where
                E: serde::de::Error,
            {
                if value.eq_ignore_ascii_case("narrow") {
                    Ok(AmbiguousWidth::Narrow)
                } else if value.eq_ignore_ascii_case("wide") {
                    Ok(AmbiguousWidth::Wide)
                } else {
                    Err(E::custom(format!(
                        "invalid ambiguous_width '{value}', expected \"narrow\" or \"wide\""
                    )))
                }
            }

            fn visit_u64<E>(self, value: u64) -> Result<AmbiguousWidth, E>
            where
                E: serde::de::Error,
            {
                match value {
                    1 => Ok(AmbiguousWidth::Narrow),
                    2 => Ok(AmbiguousWidth::Wide),
                    other => Err(E::custom(format!(
                        "invalid ambiguous_width '{other}', expected 1 or 2"
                    ))),
                }
            }

            fn visit_i64<E>(self, value: i64) -> Result<AmbiguousWidth, E>
            where
                E: serde::de::Error,
            {
                match value {
                    1 => Ok(AmbiguousWidth::Narrow),
                    2 => Ok(AmbiguousWidth::Wide),
                    other => Err(E::custom(format!(
                        "invalid ambiguous_width '{other}', expected 1 or 2"
                    ))),
                }
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
pub struct FontConfig {
    #[serde(alias = "families")]
    pub family: Option<FontFamilies>,
    pub size: Option<f32>,
    /// Enables programming ligatures for adjacent symbol runs (default: true).
    pub ligatures: Option<bool>,
    /// Vertical line spacing multiplier applied to the rendered cell height (default: 1.0).
    pub line_height: Option<f32>,
    /// Width model for East Asian Ambiguous characters (default: narrow).
    #[serde(alias = "ambiguous-width")]
    pub ambiguous_width: Option<AmbiguousWidth>,
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
        let Some(p) = self.palette.take() else {
            return;
        };
        fold_color(&mut self.black, p.black);
        fold_color(&mut self.red, p.red);
        fold_color(&mut self.green, p.green);
        fold_color(&mut self.yellow, p.yellow);
        fold_color(&mut self.blue, p.blue);
        fold_color(&mut self.magenta, p.magenta);
        fold_color(&mut self.cyan, p.cyan);
        fold_color(&mut self.white, p.white);
        fold_color(&mut self.bright_black, p.bright_black);
        fold_color(&mut self.bright_red, p.bright_red);
        fold_color(&mut self.bright_green, p.bright_green);
        fold_color(&mut self.bright_yellow, p.bright_yellow);
        fold_color(&mut self.bright_blue, p.bright_blue);
        fold_color(&mut self.bright_magenta, p.bright_magenta);
        fold_color(&mut self.bright_cyan, p.bright_cyan);
        fold_color(&mut self.bright_white, p.bright_white);
    }
}

/// Assigns `src` into `dst` only when the caller did not set it explicitly.
fn fold_color(dst: &mut Option<Rgb>, src: Option<Rgb>) {
    if dst.is_none() {
        *dst = src;
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

// Variant names double as the TOML config surface (`rename_all = "snake_case"`), so
// keeping the `Pipe` prefix preserves configuration compatibility.
#[allow(clippy::enum_variant_names)]
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

/// Lookup table for named keybinding actions (drives both resolution and the
/// known-actions fast path). Kept as a table so `to_resolved_action` stays a
/// flat lookup instead of a many-branch match.
const NAMED_ACTIONS: &[(&str, KeyAction)] = &[
    ("scrollback_up_page", KeyAction::ScrollbackUpPage),
    ("scrollback_down_page", KeyAction::ScrollbackDownPage),
    ("scrollback_up_line", KeyAction::ScrollbackUpLine),
    ("scrollback_down_line", KeyAction::ScrollbackDownLine),
    ("scrollback_home", KeyAction::ScrollbackHome),
    ("scrollback_end", KeyAction::ScrollbackEnd),
    ("prompt_prev", KeyAction::PromptPrev),
    ("prompt_next", KeyAction::PromptNext),
    ("font_increase", KeyAction::FontIncrease),
    ("font_decrease", KeyAction::FontDecrease),
    ("font_reset", KeyAction::FontReset),
    ("clipboard_copy", KeyAction::ClipboardCopy),
    ("clipboard_paste", KeyAction::ClipboardPaste),
    ("primary_paste", KeyAction::PrimaryPaste),
];

/// Resolves a known action name to its [`KeyAction`], or `None` if unrecognized.
fn named_action(name: &str) -> Option<KeyAction> {
    NAMED_ACTIONS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, action)| action.clone())
}

impl ActionDef {
    #[must_use]
    pub fn to_resolved_action(&self) -> ResolvedAction {
        match self {
            Self::Simple(s) => {
                let name = s.to_ascii_lowercase();
                match named_action(&name) {
                    Some(action) => ResolvedAction::Action(action),
                    None if name.is_empty() || name == "none" => ResolvedAction::Unbind,
                    None => {
                        eprintln!("ftty: unrecognized keybinding action '{s}'");
                        ResolvedAction::Invalid
                    }
                }
            }
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
            if named_action(&key_lower).is_some() {
                resolve_known_binding(&mut map, key_lower, def);
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

/// Maps a resolved action onto every combo in a known-action user binding.
fn resolve_known_binding(
    map: &mut HashMap<(Modifiers, xkb::Keysym), KeyAction>,
    key_lower: String,
    def: &ActionDef,
) {
    let action_res = ActionDef::Simple(key_lower).to_resolved_action();
    let ResolvedAction::Action(act) = action_res else {
        return;
    };
    match def {
        ActionDef::Simple(combo) => insert_user_combo(map, combo, act),
        ActionDef::Multiple(combos) => {
            for combo in combos {
                insert_user_combo(map, combo, act.clone());
            }
        }
        ActionDef::Pipe(_) => {}
    }
}

/// Parses and canonicalizes a user combo string, inserting it into the bindings map.
fn insert_user_combo(
    map: &mut HashMap<(Modifiers, xkb::Keysym), KeyAction>,
    combo: &str,
    action: KeyAction,
) {
    if let Some((mods, sym)) = parse_key_combo(combo) {
        let canonical_sym = canonicalize_sym(sym);
        map.insert((mods, canonical_sym), action);
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
        load_internal(path, &mut visited)
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
        if let Some(ligatures) = other.font.ligatures {
            self.font.ligatures = Some(ligatures);
        }
        if let Some(line_height) = other.font.line_height {
            self.font.line_height = Some(line_height);
        }
        if let Some(aw) = other.font.ambiguous_width {
            self.font.ambiguous_width = Some(aw);
        }

        if let Some(p) = other.window.padding {
            self.window.padding = Some(p);
        }

        if let Some(shape) = other.cursor.shape {
            self.cursor.shape = Some(shape);
        }

        merge_colors(&mut self.colors, other.colors);

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

        merge_keybindings(&mut self.keybindings, other.keybindings);
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
    pub fn ligatures(&self) -> bool {
        self.font.ligatures.unwrap_or(true)
    }

    #[must_use]
    pub fn line_height(&self) -> f32 {
        self.font
            .line_height
            .filter(|lh| lh.is_finite() && *lh > 0.0)
            .unwrap_or(1.0)
    }

    #[must_use]
    pub fn ambiguous_width(&self) -> AmbiguousWidth {
        self.font.ambiguous_width.unwrap_or_default()
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
        let legacy = c.palette.as_ref();
        let slots: [(usize, Option<Rgb>, Option<Rgb>); 16] = [
            (0, c.black, legacy.and_then(|p| p.black)),
            (1, c.red, legacy.and_then(|p| p.red)),
            (2, c.green, legacy.and_then(|p| p.green)),
            (3, c.yellow, legacy.and_then(|p| p.yellow)),
            (4, c.blue, legacy.and_then(|p| p.blue)),
            (5, c.magenta, legacy.and_then(|p| p.magenta)),
            (6, c.cyan, legacy.and_then(|p| p.cyan)),
            (7, c.white, legacy.and_then(|p| p.white)),
            (8, c.bright_black, legacy.and_then(|p| p.bright_black)),
            (9, c.bright_red, legacy.and_then(|p| p.bright_red)),
            (10, c.bright_green, legacy.and_then(|p| p.bright_green)),
            (11, c.bright_yellow, legacy.and_then(|p| p.bright_yellow)),
            (12, c.bright_blue, legacy.and_then(|p| p.bright_blue)),
            (13, c.bright_magenta, legacy.and_then(|p| p.bright_magenta)),
            (14, c.bright_cyan, legacy.and_then(|p| p.bright_cyan)),
            (15, c.bright_white, legacy.and_then(|p| p.bright_white)),
        ];
        for (idx, primary, legacy_slot) in slots {
            fold_palette_slot(&mut palette, idx, primary, legacy_slot);
        }

        if let Some(indexed) = &self.colors.indexed {
            for (&idx, &color) in indexed {
                palette[idx as usize] = color;
            }
        }

        palette
    }
}

/// Writes `primary` into `palette[idx]`, falling back to the legacy palette slot.
fn fold_palette_slot(
    palette: &mut [Rgb; 256],
    idx: usize,
    primary: Option<Rgb>,
    legacy: Option<Rgb>,
) {
    if let Some(col) = primary.or(legacy) {
        palette[idx] = col;
    }
}

// --- Include resolution ---

use std::fs;

/// Flexible include directive accepting either a single string or an array of strings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Include {
    Single(String),
    Multiple(Vec<String>),
}

impl Include {
    #[must_use]
    pub fn to_paths(&self) -> Vec<&str> {
        match self {
            Self::Single(path) => vec![path.as_str()],
            Self::Multiple(paths) => paths.iter().map(String::as_str).collect(),
        }
    }
}

pub(crate) fn load_internal(path: &Path, visited: &mut HashSet<PathBuf>) -> io::Result<Config> {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical.clone()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("circular include detected: {}", canonical.display()),
        ));
    }

    let content = fs::read_to_string(path)?;
    let mut current: Config = toml::from_str(&content).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to parse {}: {e}", path.display()),
        )
    })?;

    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));

    // If includes are present, load and merge them in sequence
    if let Some(include) = &current.include {
        let mut merged_base = Config::default();
        for inc_str in include.to_paths() {
            let resolved = resolve_path(inc_str, base_dir);
            let inc_config = load_internal(&resolved, visited)?;
            merged_base.merge(inc_config);
        }
        // Included settings form the base; the current file's explicit settings override them
        merged_base.merge(current);
        current = merged_base;
    }

    visited.remove(&canonical);
    Ok(current)
}

pub(crate) fn merge_colors(dst: &mut ColorsConfig, mut src: ColorsConfig) {
    src.fold_legacy_palette();
    dst.fold_legacy_palette();

    merge_color(&mut dst.foreground, src.foreground);
    merge_color(&mut dst.background, src.background);
    merge_color(&mut dst.black, src.black);
    merge_color(&mut dst.red, src.red);
    merge_color(&mut dst.green, src.green);
    merge_color(&mut dst.yellow, src.yellow);
    merge_color(&mut dst.blue, src.blue);
    merge_color(&mut dst.magenta, src.magenta);
    merge_color(&mut dst.cyan, src.cyan);
    merge_color(&mut dst.white, src.white);
    merge_color(&mut dst.bright_black, src.bright_black);
    merge_color(&mut dst.bright_red, src.bright_red);
    merge_color(&mut dst.bright_green, src.bright_green);
    merge_color(&mut dst.bright_yellow, src.bright_yellow);
    merge_color(&mut dst.bright_blue, src.bright_blue);
    merge_color(&mut dst.bright_magenta, src.bright_magenta);
    merge_color(&mut dst.bright_cyan, src.bright_cyan);
    merge_color(&mut dst.bright_white, src.bright_white);

    if let Some(other_indexed) = src.indexed {
        let indexed = dst.indexed.get_or_insert_with(HashMap::new);
        indexed.extend(other_indexed);
    }
}

/// Overwrites `dst` with `src` when the source slot is set.
fn merge_color(dst: &mut Option<Rgb>, src: Option<Rgb>) {
    if let Some(c) = src {
        *dst = Some(c);
    }
}

pub(crate) fn merge_keybindings(dst: &mut KeybindingsConfig, src: KeybindingsConfig) {
    for (src_key, src_val) in src.bindings {
        if let Some(src_parsed) = crate::input::parse_key_combo(&src_key) {
            dst.bindings
                .retain(|dst_key, _| crate::input::parse_key_combo(dst_key) != Some(src_parsed));
        }
        dst.bindings.insert(src_key, src_val);
    }
}

/// Resolves path strings, expanding leading `~` to the home directory and resolving relative paths.
#[must_use]
pub fn resolve_path(path_str: &str, base_dir: &Path) -> PathBuf {
    if let Some(stripped) = path_str.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(stripped);
    }
    let p = Path::new(path_str);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base_dir.join(p)
    }
}

/// Returns the default XDG configuration path `$XDG_CONFIG_HOME/ftty/ftty.toml` or `~/.config/ftty/ftty.toml`.
#[must_use]
pub fn default_config_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("ftty").join("ftty.toml"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Some(
            PathBuf::from(home)
                .join(".config")
                .join("ftty")
                .join("ftty.toml"),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use xkbcommon::xkb;

    use crate::color::Rgb;

    use crate::grid::CursorShape;

    #[test]
    fn test_default_config_values() {
        let config = Config::default();
        assert_eq!(config.font_family(), "monospace");
        assert_eq!(config.font_size(), 9.0);
        assert_eq!(config.padding(), [0, 0]);
        assert_eq!(config.cursor_shape(), CursorShape::Block);
        assert_eq!(config.foreground(), Rgb::new(220, 220, 220));
        assert_eq!(config.background(), Rgb::new(24, 24, 24));
    }

    #[test]
    fn test_parse_basic_toml() {
        let toml_str = r##"
    [font]
    family = ["JetBrains Mono", "Noto Sans CJK SC", "Noto Color Emoji"]
    size = 16.5

    [window]
    padding = [4, 6]

    [cursor]
    shape = "beam"

    [colors]
    foreground = "#ffffff"
    background = "#000000"
    red = "#ff0000"
    bright_red = "#ff5555"
    "##;

        let config: Config = toml::from_str(toml_str).expect("parse toml");
        assert_eq!(config.font_family(), "JetBrains Mono");
        assert_eq!(
            config.font_families(),
            vec![
                "JetBrains Mono".to_string(),
                "Noto Sans CJK SC".to_string(),
                "Noto Color Emoji".to_string(),
            ]
        );
        assert_eq!(config.font_size(), 16.5);
        assert_eq!(config.padding(), [4, 6]);
        assert_eq!(config.padding_x(), 4);
        assert_eq!(config.padding_y(), 6);
        assert_eq!(config.cursor_shape(), CursorShape::Beam);
        assert_eq!(config.foreground(), Rgb::new(255, 255, 255));
        assert_eq!(config.background(), Rgb::new(0, 0, 0));

        let palette = config.build_palette();
        assert_eq!(palette[1], Rgb::new(255, 0, 0));
        assert_eq!(palette[9], Rgb::new(255, 85, 85));
    }

    #[test]
    fn test_parse_padding_scalar() {
        let toml_str = r#"
    [window]
    padding = 8
    "#;
        let config: Config = toml::from_str(toml_str).expect("parse padding scalar");
        assert_eq!(config.padding(), [8, 8]);
        assert_eq!(config.padding_x(), 8);
        assert_eq!(config.padding_y(), 8);
    }

    #[test]
    fn test_parse_font_chain() {
        let toml_str = r#"
    [font]
    families = ["Fira Code", "Symbols Nerd Font", "Noto Sans CJK SC"]
    size = 15.0
    "#;
        let config: Config = toml::from_str(toml_str).expect("parse toml with font chain");
        assert_eq!(config.font_family(), "Fira Code");
        assert_eq!(
            config.font_families(),
            vec!["Fira Code", "Symbols Nerd Font", "Noto Sans CJK SC"]
        );
        assert_eq!(config.font_size(), 15.0);
    }

    #[test]
    fn test_parse_scrollback_and_keybindings() {
        let toml_str = r##"
    [scrollback]
    lines = 5000
    multiplier = 5.5
    auto_scroll = false

    [keybindings]
    "Shift+PageUp" = "scrollback_up_page"
    "Shift+KP_PageUp" = "scrollback_up_page"
    "Shift+PageDown" = "scrollback_down_page"
    "Ctrl+Shift+C" = "clipboard_copy"
    "Ctrl+Shift+V" = "none"
    "Ctrl+Shift+U" = { pipe_visible = ["urlscan"] }
    "Ctrl+Shift+F" = { pipe_scrollback = ["sh", "-c", "fzf | wl-copy"] }
    "Ctrl+Shift+Y" = { pipe_selection = "wl-copy" }
    "##;

        let config: Config = toml::from_str(toml_str).expect("parse toml");
        assert_eq!(config.scrollback_lines(), 5000);
        assert_eq!(config.scroll_multiplier(), 5.5);
        assert!(!config.auto_scroll());

        let resolved = config.keybindings.resolve_bindings();
        let pipe_u = resolved.iter().find(|((_, sym), _)| {
            *sym == xkb::Keysym::new(xkb::keysyms::KEY_u)
                || *sym == xkb::Keysym::new(xkb::keysyms::KEY_U)
        });
        assert!(pipe_u.is_some());
        assert_eq!(
            pipe_u.unwrap().1,
            crate::input::KeyAction::PipeVisible(vec!["urlscan".to_string()])
        );

        let pipe_y = resolved.iter().find(|((_, sym), _)| {
            *sym == xkb::Keysym::new(xkb::keysyms::KEY_y)
                || *sym == xkb::Keysym::new(xkb::keysyms::KEY_Y)
        });
        assert!(pipe_y.is_some());
        assert_eq!(
            pipe_y.unwrap().1,
            crate::input::KeyAction::PipeSelection(vec![
                "sh".to_string(),
                "-c".to_string(),
                "wl-copy".to_string()
            ])
        );

        // Verify unbind "none" removed Ctrl+Shift+V
        let paste = resolved.iter().find(|((_, sym), _)| {
            *sym == xkb::Keysym::new(xkb::keysyms::KEY_v)
                || *sym == xkb::Keysym::new(xkb::keysyms::KEY_V)
        });
        assert!(paste.is_none());
    }

    #[test]
    fn test_include_merging_and_override() {
        let temp_dir =
            std::env::temp_dir().join(format!("ftty_test_include_{}", std::process::id()));
        fs::create_dir_all(&temp_dir).unwrap();

        let theme_path = temp_dir.join("theme.toml");
        fs::write(
            &theme_path,
            r##"
        [colors]
        foreground = "#aaaaaa"
        background = "#111111"
        blue = "#0000ff"
        "##,
        )
        .unwrap();

        let main_path = temp_dir.join("ftty.toml");
        fs::write(
            &main_path,
            r##"
        include = "theme.toml"

        [font]
        size = 18.0

        [colors]
        background = "#222222"
        "##,
        )
        .unwrap();

        let config = Config::load(&main_path).expect("load config with include");
        assert_eq!(config.font_size(), 18.0);
        assert_eq!(config.foreground(), Rgb::new(170, 170, 170)); // from theme.toml
        assert_eq!(config.background(), Rgb::new(34, 34, 34)); // overridden by main
        let palette = config.build_palette();
        assert_eq!(palette[4], Rgb::new(0, 0, 255)); // from theme.toml

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_circular_include_rejection() {
        let temp_dir = std::env::temp_dir().join(format!("ftty_test_circ_{}", std::process::id()));
        fs::create_dir_all(&temp_dir).unwrap();

        let a_path = temp_dir.join("a.toml");
        let b_path = temp_dir.join("b.toml");

        fs::write(&a_path, "include = 'b.toml'").unwrap();
        fs::write(&b_path, "include = 'a.toml'").unwrap();

        let res = Config::load(&a_path);
        assert!(res.is_err());

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_parse_font_size_and_family() {
        let toml_default = r#"
        [font]
        size = 14.0
    "#;
        let config: Config = toml::from_str(toml_default).unwrap();
        assert_eq!(config.font_size(), 14.0);

        let mut base = Config::default();
        let mut inc = Config::default();
        inc.font.size = Some(16.0);
        base.merge(inc);
        assert_eq!(base.font_size(), 16.0);
    }

    #[test]
    fn test_include_keybindings_prompt_navigation() {
        let mut base = Config::default();
        let mut inc = Config::default();
        inc.keybindings.bindings.insert(
            "Ctrl+Shift+K".to_string(),
            crate::config::ActionDef::Simple("prompt_prev".to_string()),
        );
        inc.keybindings.bindings.insert(
            "Ctrl+Shift+J".to_string(),
            crate::config::ActionDef::Simple("prompt_next".to_string()),
        );
        base.merge(inc);
        let resolved = base.keybindings.resolve_bindings();
        let prev = resolved.iter().find(|((_, sym), _)| {
            *sym == xkb::Keysym::new(xkb::keysyms::KEY_k)
                || *sym == xkb::Keysym::new(xkb::keysyms::KEY_K)
        });
        assert!(prev.is_some());
        assert_eq!(prev.unwrap().1, crate::input::KeyAction::PromptPrev);
    }

    #[test]
    fn test_clipboard_security_config() {
        let toml_str = r#"
        [clipboard]
        allow_osc52_read = true
        allow_osc52_write = false
    "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.allow_osc52_read());
        assert!(!config.allow_osc52_write());

        let default_config = Config::default();
        assert!(!default_config.allow_osc52_read());
        assert!(default_config.allow_osc52_write());

        let mut base = Config::default();
        let mut inc = Config::default();
        inc.clipboard.allow_osc52_read = Some(true);
        base.merge(inc);
        assert!(base.allow_osc52_read());
    }

    #[test]
    fn test_flattened_colors_all_ansi_and_indexed() {
        let toml_str = r##"
    [colors]
    foreground = "#dcdcdc"
    background = "#181818"
    black = "#000000"
    red = "#cc0403"
    green = "#19cb00"
    yellow = "#cecb00"
    blue = "#0d73cc"
    magenta = "#cb1ed1"
    cyan = "#0dcdcd"
    white = "#e5e5e5"
    bright_black = "#767676"
    bright_red = "#f2201f"
    bright_green = "#23fd00"
    bright_yellow = "#fffd00"
    bright_blue = "#1a8fff"
    bright_magenta = "#fd28ff"
    bright_cyan = "#14ffff"
    bright_white = "#ffffff"

    [colors.indexed]
    16 = "#ff0088"
    255 = "#112233"
    "##;

        let config: Config = toml::from_str(toml_str).expect("parse flat colors toml");
        assert_eq!(config.foreground(), Rgb::new(0xdc, 0xdc, 0xdc));
        assert_eq!(config.background(), Rgb::new(0x18, 0x18, 0x18));

        let palette = config.build_palette();
        assert_eq!(palette[0], Rgb::new(0, 0, 0));
        assert_eq!(palette[1], Rgb::new(0xcc, 0x04, 0x03));
        assert_eq!(palette[2], Rgb::new(0x19, 0xcb, 0x00));
        assert_eq!(palette[3], Rgb::new(0xce, 0xcb, 0x00));
        assert_eq!(palette[4], Rgb::new(0x0d, 0x73, 0xcc));
        assert_eq!(palette[5], Rgb::new(0xcb, 0x1e, 0xd1));
        assert_eq!(palette[6], Rgb::new(0x0d, 0xcd, 0xcd));
        assert_eq!(palette[7], Rgb::new(0xe5, 0xe5, 0xe5));
        assert_eq!(palette[8], Rgb::new(0x76, 0x76, 0x76));
        assert_eq!(palette[9], Rgb::new(0xf2, 0x20, 0x1f));
        assert_eq!(palette[10], Rgb::new(0x23, 0xfd, 0x00));
        assert_eq!(palette[11], Rgb::new(0xff, 0xfd, 0x00));
        assert_eq!(palette[12], Rgb::new(0x1a, 0x8f, 0xff));
        assert_eq!(palette[13], Rgb::new(0xfd, 0x28, 0xff));
        assert_eq!(palette[14], Rgb::new(0x14, 0xff, 0xff));
        assert_eq!(palette[15], Rgb::new(0xff, 0xff, 0xff));
        assert_eq!(palette[16], Rgb::new(0xff, 0x00, 0x88));
        assert_eq!(palette[255], Rgb::new(0x11, 0x22, 0x33));
    }

    #[test]
    fn test_legacy_palette_subtable_backward_compatibility() {
        let toml_str = r##"
    [colors]
    foreground = "#ffffff"
    background = "#000000"

    [colors.palette]
    red = "#ff0000"
    blue = "#0000ff"
    "##;

        let config: Config = toml::from_str(toml_str).expect("parse legacy palette toml");
        let palette = config.build_palette();
        assert_eq!(palette[1], Rgb::new(255, 0, 0));
        assert_eq!(palette[4], Rgb::new(0, 0, 255));
    }

    #[test]
    fn test_font_ambiguous_width_config_parsing() {
        // Default is narrow (1 cell)
        let default_cfg = Config::default();
        assert_eq!(default_cfg.ambiguous_width(), AmbiguousWidth::Narrow);
        assert_eq!(default_cfg.ambiguous_width().to_u8(), 1);

        // String "wide"
        let toml_wide = r#"
        [font]
        ambiguous_width = "wide"
        "#;
        let cfg: Config = toml::from_str(toml_wide).expect("parse wide");
        assert_eq!(cfg.ambiguous_width(), AmbiguousWidth::Wide);
        assert_eq!(cfg.ambiguous_width().to_u8(), 2);

        // String "narrow"
        let toml_narrow = r#"
        [font]
        ambiguous_width = "narrow"
        "#;
        let cfg: Config = toml::from_str(toml_narrow).expect("parse narrow");
        assert_eq!(cfg.ambiguous_width(), AmbiguousWidth::Narrow);

        // Alias "ambiguous-width"
        let toml_kebab = r#"
        [font]
        ambiguous-width = "wide"
        "#;
        let cfg: Config = toml::from_str(toml_kebab).expect("parse kebab alias");
        assert_eq!(cfg.ambiguous_width(), AmbiguousWidth::Wide);

        // Integer 2
        let toml_int2 = r#"
        [font]
        ambiguous_width = 2
        "#;
        let cfg: Config = toml::from_str(toml_int2).expect("parse int 2");
        assert_eq!(cfg.ambiguous_width(), AmbiguousWidth::Wide);

        // Integer 1
        let toml_int1 = r#"
        [font]
        ambiguous_width = 1
        "#;
        let cfg: Config = toml::from_str(toml_int1).expect("parse int 1");
        assert_eq!(cfg.ambiguous_width(), AmbiguousWidth::Narrow);

        // Case-insensitivity
        let toml_case = r#"
        [font]
        ambiguous_width = "WiDe"
        "#;
        let cfg: Config = toml::from_str(toml_case).expect("parse mixed case");
        assert_eq!(cfg.ambiguous_width(), AmbiguousWidth::Wide);

        // Invalid string rejected
        let toml_invalid_str = r#"
        [font]
        ambiguous_width = "double"
        "#;
        assert!(toml::from_str::<Config>(toml_invalid_str).is_err());

        // Invalid int rejected
        let toml_invalid_int = r#"
        [font]
        ambiguous_width = 3
        "#;
        assert!(toml::from_str::<Config>(toml_invalid_int).is_err());
    }

    #[test]
    fn test_font_ambiguous_width_merge() {
        let mut base = Config::default();
        assert_eq!(base.ambiguous_width(), AmbiguousWidth::Narrow);

        let override_toml = r#"
        [font]
        ambiguous_width = "wide"
        "#;
        let over: Config = toml::from_str(override_toml).expect("parse override");
        base.merge(over);
        assert_eq!(base.ambiguous_width(), AmbiguousWidth::Wide);
    }
}
