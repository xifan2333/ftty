//! Recursive include resolution, circular reference detection, and merge helpers.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{Config, KeybindingsConfig, PaletteConfig};

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

pub(crate) fn merge_palette(dst: &mut PaletteConfig, src: PaletteConfig) {
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

pub(crate) fn merge_keybindings(dst: &mut KeybindingsConfig, src: KeybindingsConfig) {
    dst.bindings.extend(src.bindings);
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
