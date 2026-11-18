//! Active color scheme and per-frame render options.

use crate::color::Rgb;
use crate::input::ime::Preedit;
use crate::input::selection::Selection;

/// Active color scheme holding the 256-color palette and default foreground/background.
#[derive(Debug, Clone, Copy)]
pub struct ColorScheme<'a> {
    pub palette: &'a [Rgb; 256],
    pub foreground: Rgb,
    pub background: Rgb,
}

impl<'a> ColorScheme<'a> {
    #[must_use]
    pub fn new(palette: &'a [Rgb; 256], foreground: Rgb, background: Rgb) -> Self {
        Self {
            palette,
            foreground,
            background,
        }
    }
}

impl<'a> From<&'a crate::theme::Theme> for ColorScheme<'a> {
    fn from(theme: &'a crate::theme::Theme) -> Self {
        Self::new(theme.palette(), theme.default_fg(), theme.default_bg())
    }
}

/// Represents the contiguous grid span of a hovered hyperlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HoveredHyperlinkSpan {
    pub line: usize,
    pub start_col: usize,
    pub end_col: usize,
}

/// Options controlling frame layout, window padding, active IME composition, and text selection.
#[derive(Debug, Clone, Copy)]
pub struct RenderOptions<'a> {
    pub padding: [u16; 2],
    pub preedit: Option<&'a Preedit>,
    pub selection: Option<&'a Selection>,
    pub hovered_span: Option<HoveredHyperlinkSpan>,
    /// Enables programming ligatures for adjacent ASCII symbol runs (default: true).
    pub ligatures: bool,
}

impl Default for RenderOptions<'_> {
    fn default() -> Self {
        Self {
            padding: [0, 0],
            preedit: None,
            selection: None,
            hovered_span: None,
            ligatures: true,
        }
    }
}

impl<'a> RenderOptions<'a> {
    #[must_use]
    pub fn new(
        padding: [u16; 2],
        preedit: Option<&'a Preedit>,
        selection: Option<&'a Selection>,
    ) -> Self {
        Self {
            padding,
            preedit,
            selection,
            hovered_span: None,
            ligatures: true,
        }
    }

    #[must_use]
    pub fn with_hovered_span(mut self, hovered_span: Option<HoveredHyperlinkSpan>) -> Self {
        self.hovered_span = hovered_span;
        self
    }

    #[must_use]
    pub fn with_ligatures(mut self, ligatures: bool) -> Self {
        self.ligatures = ligatures;
        self
    }
}
