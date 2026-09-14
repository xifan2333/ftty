//! Declarative TOML configuration and include resolution.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::color::{Rgb, default_256_palette};
use crate::grid::CursorShape;

const DEFAULT_FONT_FAMILY: &str = "monospace";
const DEFAULT_FONT_SIZE: f32 = 14.0;
const DEFAULT_COLUMNS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
const DEFAULT_FOREGROUND: Rgb = Rgb::new(220, 220, 220);
const DEFAULT_BACKGROUND: Rgb = Rgb::new(24, 24, 24);

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

/// Font configuration options.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Default)]
pub struct FontConfig {
    pub family: Option<String>,
    pub size: Option<f32>,
}

/// Initial window dimensions and padding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct WindowConfig {
    pub columns: Option<u16>,
    pub rows: Option<u16>,
    pub padding_x: Option<u16>,
    pub padding_y: Option<u16>,
}

/// Text cursor appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct CursorConfig {
    pub shape: Option<CursorShape>,
}

/// Named standard 16 ANSI colors (0..=15).
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

/// Terminal colors including default foreground/background and color palette.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct ColorsConfig {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub palette: Option<PaletteConfig>,
    pub indexed: Option<HashMap<u8, Rgb>>,
}

/// Scrollback buffer depth and mouse wheel scrolling parameters.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize, Default)]
pub struct ScrollbackConfig {
    pub lines: Option<u32>,
    pub multiplier: Option<f32>,
    pub auto_scroll: Option<bool>,
}

/// Flexible key combinations mapping supporting a single combo or a list of combos.
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
            Self::Multiple(list) => list
                .iter()
                .filter(|s| !s.eq_ignore_ascii_case("none"))
                .map(String::as_str)
                .collect(),
        }
    }
}

/// Action-to-key mapping following foot terminal conventions.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct KeybindingsConfig {
    pub scrollback_up_page: Option<KeyCombos>,
    pub scrollback_down_page: Option<KeyCombos>,
    pub scrollback_up_line: Option<KeyCombos>,
    pub scrollback_down_line: Option<KeyCombos>,
    pub scrollback_home: Option<KeyCombos>,
    pub scrollback_end: Option<KeyCombos>,
    pub font_increase: Option<KeyCombos>,
    pub font_decrease: Option<KeyCombos>,
    pub font_reset: Option<KeyCombos>,
    pub clipboard_copy: Option<KeyCombos>,
    pub clipboard_paste: Option<KeyCombos>,
    pub primary_paste: Option<KeyCombos>,
}

/// Root declarative configuration for `ftty`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Default)]
pub struct Config {
    pub include: Option<Include>,
    #[serde(default)]
    pub font: FontConfig,
    #[serde(default)]
    pub window: WindowConfig,
    #[serde(default)]
    pub cursor: CursorConfig,
    #[serde(default)]
    pub colors: ColorsConfig,
    #[serde(default)]
    pub scrollback: ScrollbackConfig,
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
        Self::load_internal(path, &mut visited)
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

    fn load_internal(path: &Path, visited: &mut HashSet<PathBuf>) -> io::Result<Self> {
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
                let inc_config = Self::load_internal(&resolved, visited)?;
                merged_base.merge(inc_config);
            }
            // Included settings form the base; the current file's explicit settings override them
            merged_base.merge(current);
            current = merged_base;
        }

        visited.remove(&canonical);
        Ok(current)
    }

    /// Merges `other` into `self`, where explicit options in `other` take precedence.
    pub fn merge(&mut self, other: Self) {
        if let Some(family) = other.font.family {
            self.font.family = Some(family);
        }
        if let Some(size) = other.font.size {
            self.font.size = Some(size);
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
            merge_palette(pal, other_pal);
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

        merge_keybindings(&mut self.keybindings, other.keybindings);
    }

    #[must_use]
    pub fn font_family(&self) -> &str {
        self.font.family.as_deref().unwrap_or(DEFAULT_FONT_FAMILY)
    }

    #[must_use]
    pub fn font_size(&self) -> f32 {
        self.font.size.unwrap_or(DEFAULT_FONT_SIZE)
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
        self.scrollback.multiplier.unwrap_or(3.0)
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

        if let Some(p) = self.colors.palette {
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

fn merge_palette(dst: &mut PaletteConfig, src: PaletteConfig) {
    if let Some(c) = src.black {
        dst.black = Some(c);
    }
    if let Some(c) = src.red {
        dst.red = Some(c);
    }
    if let Some(c) = src.green {
        dst.green = Some(c);
    }
    if let Some(c) = src.yellow {
        dst.yellow = Some(c);
    }
    if let Some(c) = src.blue {
        dst.blue = Some(c);
    }
    if let Some(c) = src.magenta {
        dst.magenta = Some(c);
    }
    if let Some(c) = src.cyan {
        dst.cyan = Some(c);
    }
    if let Some(c) = src.white {
        dst.white = Some(c);
    }
    if let Some(c) = src.bright_black {
        dst.bright_black = Some(c);
    }
    if let Some(c) = src.bright_red {
        dst.bright_red = Some(c);
    }
    if let Some(c) = src.bright_green {
        dst.bright_green = Some(c);
    }
    if let Some(c) = src.bright_yellow {
        dst.bright_yellow = Some(c);
    }
    if let Some(c) = src.bright_blue {
        dst.bright_blue = Some(c);
    }
    if let Some(c) = src.bright_magenta {
        dst.bright_magenta = Some(c);
    }
    if let Some(c) = src.bright_cyan {
        dst.bright_cyan = Some(c);
    }
    if let Some(c) = src.bright_white {
        dst.bright_white = Some(c);
    }
}

fn merge_keybindings(dst: &mut KeybindingsConfig, src: KeybindingsConfig) {
    if let Some(c) = src.scrollback_up_page {
        dst.scrollback_up_page = Some(c);
    }
    if let Some(c) = src.scrollback_down_page {
        dst.scrollback_down_page = Some(c);
    }
    if let Some(c) = src.scrollback_up_line {
        dst.scrollback_up_line = Some(c);
    }
    if let Some(c) = src.scrollback_down_line {
        dst.scrollback_down_line = Some(c);
    }
    if let Some(c) = src.scrollback_home {
        dst.scrollback_home = Some(c);
    }
    if let Some(c) = src.scrollback_end {
        dst.scrollback_end = Some(c);
    }
    if let Some(c) = src.font_increase {
        dst.font_increase = Some(c);
    }
    if let Some(c) = src.font_decrease {
        dst.font_decrease = Some(c);
    }
    if let Some(c) = src.font_reset {
        dst.font_reset = Some(c);
    }
    if let Some(c) = src.clipboard_copy {
        dst.clipboard_copy = Some(c);
    }
    if let Some(c) = src.clipboard_paste {
        dst.clipboard_paste = Some(c);
    }
    if let Some(c) = src.primary_paste {
        dst.primary_paste = Some(c);
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

    #[test]
    fn test_default_config_values() {
        let config = Config::default();
        assert_eq!(config.font_family(), "monospace");
        assert_eq!(config.font_size(), 14.0);
        assert_eq!(config.columns(), 80);
        assert_eq!(config.rows(), 24);
        assert_eq!(config.cursor_shape(), CursorShape::Block);
        assert_eq!(config.foreground(), Rgb::new(220, 220, 220));
        assert_eq!(config.background(), Rgb::new(24, 24, 24));
    }

    #[test]
    fn test_parse_basic_toml() {
        let toml_str = r##"
        [font]
        family = "JetBrains Mono"
        size = 16.5

        [window]
        columns = 100
        rows = 30

        [cursor]
        shape = "beam"

        [colors]
        foreground = "#ffffff"
        background = "#000000"

        [colors.palette]
        red = "#ff0000"
        bright_red = "#ff5555"
        "##;

        let config: Config = toml::from_str(toml_str).expect("parse toml");
        assert_eq!(config.font_family(), "JetBrains Mono");
        assert_eq!(config.font_size(), 16.5);
        assert_eq!(config.columns(), 100);
        assert_eq!(config.rows(), 30);
        assert_eq!(config.cursor_shape(), CursorShape::Beam);
        assert_eq!(config.foreground(), Rgb::new(255, 255, 255));
        assert_eq!(config.background(), Rgb::new(0, 0, 0));

        let palette = config.build_palette();
        assert_eq!(palette[1], Rgb::new(255, 0, 0));
        assert_eq!(palette[9], Rgb::new(255, 85, 85));
    }

    #[test]
    fn test_parse_scrollback_and_keybindings() {
        let toml_str = r##"
        [scrollback]
        lines = 5000
        multiplier = 5.5
        auto_scroll = false

        [keybindings]
        scrollback_up_page = ["Shift+PageUp", "Shift+KP_PageUp"]
        scrollback_down_page = "Shift+PageDown"
        clipboard_copy = "Ctrl+Shift+C"
        clipboard_paste = "none"
        "##;

        let config: Config = toml::from_str(toml_str).expect("parse toml");
        assert_eq!(config.scrollback_lines(), 5000);
        assert_eq!(config.scroll_multiplier(), 5.5);
        assert!(!config.auto_scroll());

        let up_combos = config.keybindings.scrollback_up_page.unwrap();
        assert_eq!(
            up_combos.to_combos(),
            vec!["Shift+PageUp", "Shift+KP_PageUp"]
        );

        let paste_combos = config.keybindings.clipboard_paste.unwrap();
        assert_eq!(paste_combos.to_combos(), Vec::<&str>::new());
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

            [colors.palette]
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
}
