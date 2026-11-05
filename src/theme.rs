//! Single source of truth for the terminal palette and default colors.

use crate::color::Rgb;

/// Snapshot of the configured colors, used by the OSC 104/110/111 reset sequences.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfiguredColors {
    palette: [Rgb; 256],
    default_fg: Rgb,
    default_bg: Rgb,
}

/// Owns the 256-entry palette, the default foreground/background, and the configured snapshot
/// that reset sequences restore. Every mutation bumps a monotonic revision, so consumers detect
/// changes by comparing revisions instead of maintaining their own dirty flags.
#[derive(Debug, Clone)]
pub struct Theme {
    palette: [Rgb; 256],
    default_fg: Rgb,
    default_bg: Rgb,
    configured: ConfiguredColors,
    revision: u64,
}

impl Theme {
    /// Creates a theme from a configured palette and default colors.
    #[must_use]
    pub fn new(palette: [Rgb; 256], default_fg: Rgb, default_bg: Rgb) -> Self {
        Self {
            palette,
            default_fg,
            default_bg,
            configured: ConfiguredColors {
                palette,
                default_fg,
                default_bg,
            },
            revision: 0,
        }
    }

    /// Returns the active palette.
    #[must_use]
    pub fn palette(&self) -> &[Rgb; 256] {
        &self.palette
    }

    /// Returns the palette entry at `index`, or `None` when out of range.
    #[must_use]
    pub fn palette_color(&self, index: usize) -> Option<Rgb> {
        self.palette.get(index).copied()
    }

    /// Returns the configured (reset) palette entry at `index`.
    #[must_use]
    pub fn configured_palette_color(&self, index: usize) -> Option<Rgb> {
        self.configured.palette.get(index).copied()
    }

    /// Returns the active default foreground color.
    #[must_use]
    pub fn default_fg(&self) -> Rgb {
        self.default_fg
    }

    /// Returns the active default background color.
    #[must_use]
    pub fn default_bg(&self) -> Rgb {
        self.default_bg
    }

    /// Returns the configured (reset) default foreground color.
    #[must_use]
    pub fn configured_default_fg(&self) -> Rgb {
        self.configured.default_fg
    }

    /// Returns the configured (reset) default background color.
    #[must_use]
    pub fn configured_default_bg(&self) -> Rgb {
        self.configured.default_bg
    }

    /// Returns the monotonic revision, bumped on every mutation.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Applies a configured theme, also replacing the snapshot that reset sequences restore.
    pub fn apply(&mut self, palette: [Rgb; 256], default_fg: Rgb, default_bg: Rgb) {
        self.palette = palette;
        self.default_fg = default_fg;
        self.default_bg = default_bg;
        self.configured = ConfiguredColors {
            palette,
            default_fg,
            default_bg,
        };
        self.bump();
    }

    /// Sets the default foreground without touching the configured snapshot.
    pub fn set_default_fg(&mut self, color: Rgb) {
        self.default_fg = color;
        self.bump();
    }

    /// Sets the default background without touching the configured snapshot.
    pub fn set_default_bg(&mut self, color: Rgb) {
        self.default_bg = color;
        self.bump();
    }

    /// Sets one palette entry without touching the configured snapshot.
    pub fn set_indexed(&mut self, index: usize, color: Rgb) {
        if let Some(entry) = self.palette.get_mut(index) {
            *entry = color;
            self.bump();
        }
    }

    /// Restores the configured default foreground (OSC 110).
    pub fn reset_default_fg(&mut self) {
        self.default_fg = self.configured.default_fg;
        self.bump();
    }

    /// Restores the configured default background (OSC 111).
    pub fn reset_default_bg(&mut self) {
        self.default_bg = self.configured.default_bg;
        self.bump();
    }

    /// Restores the configured palette, wholly (OSC 104) or for a single entry.
    pub fn reset_palette(&mut self, index: Option<usize>) {
        match index {
            Some(index) => {
                if let (Some(entry), Some(configured)) = (
                    self.palette.get_mut(index),
                    self.configured.palette.get(index),
                ) {
                    *entry = *configured;
                }
            }
            None => self.palette = self.configured.palette,
        }
        self.bump();
    }

    fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::new(
            crate::color::default_256_palette(),
            Rgb::new(220, 220, 220),
            Rgb::new(24, 24, 24),
        )
    }
}
