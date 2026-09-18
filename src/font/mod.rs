//! Font loading, multi-family fallback chaining, and dynamic text metrics calculation.

pub mod atlas;
pub(crate) mod fallback;

#[cfg(test)]
mod tests;

use std::io;
use std::sync::{Arc, Mutex, OnceLock};

use fontconfig::Fontconfig;

use crate::font::fallback::{
    FallbackCache, FallbackFace, fontconfig, load_font_file, match_family,
    query_fontconfig_candidates,
};
use crate::grid::CellFlags;

pub use atlas::{CachedGlyph, GlyphAtlas, MAX_ATLAS_SIZE};

/// Cell size and baseline alignment metrics for the active font and font size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellMetrics {
    pub cell_width: u32,
    pub cell_height: u32,
    pub ascent: i32,
}

/// Stable identity of a rasterized glyph: which face supplied it and in which style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct FaceKey {
    /// `0` is the styled primary face; `1..=num_fallbacks` is a user fallback; subsequent values select dynamic fallbacks.
    pub(crate) face: u16,
    pub(crate) glyph: u16,
    pub(crate) style: u8,
}

/// An immutable chain of font faces for a single style (Regular, Bold, Italic, or BoldItalic).
struct StyleChain {
    primary: fontdue::Font,
    fallbacks: Vec<fontdue::Font>,
}

/// Minimum font size in points/pixels supported by the terminal.
pub const MIN_FONT_SIZE: f32 = 6.0;
/// Maximum font size in points/pixels supported by the terminal.
pub const MAX_FONT_SIZE: f32 = 72.0;

/// Maximum font zoom size supported by the terminal.
///
/// Fontdue optimizes its vector geometry simplification for `FontSettings::scale`.
/// Parsing every face at `MAX_FONT_ZOOM_SCALE` guarantees full outline fidelity
/// across all interactive zoom sizes without outline degradation.
pub const MAX_FONT_ZOOM_SCALE: f32 = MAX_FONT_SIZE;

pub(crate) fn style_index(flags: CellFlags) -> usize {
    usize::from(flags.contains(CellFlags::BOLD))
        | (usize::from(flags.contains(CellFlags::ITALIC)) << 1)
}

/// Loads configured font chain and on-demand fallback faces with cell metrics calculation.
pub struct FontManager {
    regular: StyleChain,
    regular_slots: Vec<Option<fontdue::Font>>,
    bold: OnceLock<StyleChain>,
    italic: OnceLock<StyleChain>,
    bold_italic: OnceLock<StyleChain>,
    fallbacks: Arc<Mutex<FallbackCache>>,
    families: Vec<String>,
    font_size: f32,
    pub metrics: CellMetrics,
}

impl FontManager {
    fn chain_for_style(&self, style: u8) -> &StyleChain {
        match style {
            0 => &self.regular,
            1 => self.bold.get_or_init(|| {
                let fc = fontconfig();
                self.load_styled_chain(fc, true, false, &self.regular.primary)
            }),
            2 => self.italic.get_or_init(|| {
                let fc = fontconfig();
                self.load_styled_chain(fc, false, true, &self.regular.primary)
            }),
            3 => self.bold_italic.get_or_init(|| {
                let fc = fontconfig();
                let bold_chain = self.chain_for_style(1);
                self.load_styled_chain(fc, true, true, &bold_chain.primary)
            }),
            _ => &self.regular,
        }
    }

    fn load_styled_chain(
        &self,
        fc: Option<&Fontconfig>,
        bold: bool,
        italic: bool,
        fallback_primary: &fontdue::Font,
    ) -> StyleChain {
        let primary_name = &self.families[0];
        let fallback_names = &self.families[1..];

        let primary = fc
            .and_then(|fc| match_family(fc, primary_name, bold, italic))
            .and_then(|(path, index)| load_font_file(&path, index).ok())
            .unwrap_or_else(|| fallback_primary.clone());

        let fallbacks = fallback_names
            .iter()
            .enumerate()
            .filter_map(|(i, name)| {
                fc.and_then(|fc| match_family(fc, name, bold, italic))
                    .and_then(|(path, index)| load_font_file(&path, index).ok())
                    .or_else(|| self.regular_slots.get(i).and_then(|opt| opt.clone()))
            })
            .collect();

        StyleChain { primary, fallbacks }
    }

    /// Discovers and loads an ordered list of font families at a size in pixels per em.
    ///
    /// # Errors
    /// Returns an error for an invalid size, missing font, or unreadable font data.
    pub fn load_with_families(families: &[String], font_size: f32) -> io::Result<Self> {
        if !font_size.is_finite() || font_size < MIN_FONT_SIZE || font_size > MAX_FONT_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("font size must be between {MIN_FONT_SIZE} and {MAX_FONT_SIZE}"),
            ));
        }
        let fc = fontconfig()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "fontconfig not available"))?;

        let valid_families: Vec<String> = if families.is_empty() {
            vec!["monospace".to_string()]
        } else {
            families.to_vec()
        };

        let primary_name = &valid_families[0];
        let fallback_names = &valid_families[1..];

        // 1. Load the primary Regular font (determines CellMetrics)
        let primary_regular = match_family(fc, primary_name, false, false)
            .and_then(|(path, index)| load_font_file(&path, index).ok())
            .or_else(|| {
                let (path, index) = match_family(fc, "monospace", false, false)?;
                load_font_file(&path, index).ok()
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no monospace font found"))?;

        // 2. Compute metrics from the primary Regular font ('0' advance width & horizontal line metrics)
        let cell_width = primary_regular
            .metrics('0', font_size)
            .advance_width
            .ceil()
            .max(1.0) as u32;

        let (cell_height, ascent) = primary_regular
            .horizontal_line_metrics(font_size)
            .map(|line| {
                (
                    line.new_line_size.ceil().max(1.0) as u32,
                    line.ascent.ceil() as i32,
                )
            })
            .unwrap_or((font_size.ceil().max(1.0) as u32, font_size.ceil() as i32));

        // 3. User fallback regular slots maintain 1:1 index alignment with fallback_names.
        let regular_slots: Vec<Option<fontdue::Font>> = fallback_names
            .iter()
            .map(|name| {
                let (path, index) = match_family(fc, name, false, false)?;
                load_font_file(&path, index).ok()
            })
            .collect();

        let regular_fallbacks: Vec<fontdue::Font> =
            regular_slots.iter().flatten().cloned().collect();

        let regular = StyleChain {
            primary: primary_regular,
            fallbacks: regular_fallbacks,
        };

        let fallbacks = Arc::new(Mutex::new(FallbackCache::default()));
        let fallbacks_prewarm = Arc::clone(&fallbacks);
        let preferred = valid_families[0].clone();
        std::thread::Builder::new()
            .name("font-prewarm".to_string())
            .spawn(move || {
                let Some(fc) = fontconfig() else {
                    return;
                };
                for style in [0u8, 1u8] {
                    let bold = (style & 1) != 0;
                    if let Some(candidates) =
                        query_fontconfig_candidates(fc, &preferred, bold, false, '中')
                    {
                        for (path, index) in candidates.into_iter().take(1) {
                            if let Ok(font) = load_font_file(&path, index) {
                                let mut cache = fallbacks_prewarm
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                cache.insert_face(FallbackFace {
                                    path,
                                    index,
                                    style,
                                    font,
                                });
                            }
                        }
                    }
                }
            })
            .ok();

        Ok(Self {
            regular,
            regular_slots,
            bold: OnceLock::new(),
            italic: OnceLock::new(),
            bold_italic: OnceLock::new(),
            fallbacks,
            families: valid_families,
            font_size,
            metrics: CellMetrics {
                cell_width,
                cell_height,
                ascent,
            },
        })
    }

    /// Discovers and loads a font face by family name and size in pixels per em.
    ///
    /// # Errors
    /// Returns an error for an invalid size, missing font, or unreadable font data.
    pub fn load_with_family(family: &str, font_size: f32) -> io::Result<Self> {
        Self::load_with_families(&[family.to_string()], font_size)
    }

    /// Discovers and loads a system monospace face at a size in pixels per em.
    ///
    /// # Errors
    /// Returns an error for an invalid size, missing font, or unreadable font data.
    pub fn load(font_size: f32) -> io::Result<Self> {
        Self::load_with_families(&["monospace".to_string()], font_size)
    }

    #[must_use]
    pub fn family(&self) -> &str {
        self.families
            .first()
            .map(String::as_str)
            .unwrap_or("monospace")
    }

    #[must_use]
    pub fn families(&self) -> &[String] {
        &self.families
    }

    #[must_use]
    pub fn font_size(&self) -> f32 {
        self.font_size
    }

    /// Dynamically scales the font size and recalculates cell metrics in-memory.
    ///
    /// Because `fontdue::Font` maintains vector outlines and supports arbitrary
    /// scale factors during rasterization, this avoids re-reading font files from
    /// disk or re-querying Fontconfig on runtime zoom. Returns `true` if the size changed.
    pub fn set_font_size(&mut self, new_size: f32) -> bool {
        if !new_size.is_finite() || new_size < MIN_FONT_SIZE || new_size > MAX_FONT_SIZE {
            return false;
        }
        if (self.font_size - new_size).abs() < f32::EPSILON {
            return false;
        }
        self.font_size = new_size;

        let cell_width = self
            .regular
            .primary
            .metrics('0', new_size)
            .advance_width
            .ceil()
            .max(1.0) as u32;

        let (cell_height, ascent) = self
            .regular
            .primary
            .horizontal_line_metrics(new_size)
            .map(|line| {
                (
                    line.new_line_size.ceil().max(1.0) as u32,
                    line.ascent.ceil() as i32,
                )
            })
            .unwrap_or((new_size.ceil().max(1.0) as u32, new_size.ceil() as i32));

        self.metrics = CellMetrics {
            cell_width,
            cell_height,
            ascent,
        };

        true
    }

    /// Falls back to the regular face if a styled face cannot be loaded.
    #[must_use]
    pub fn font_for_style(&self, flags: CellFlags) -> &fontdue::Font {
        let style = style_index(flags) as u8;
        &self.chain_for_style(style).primary
    }

    #[cfg(test)]
    pub(crate) fn regular(&self) -> &fontdue::Font {
        &self.regular.primary
    }

    fn lock_fallbacks(&self) -> std::sync::MutexGuard<'_, FallbackCache> {
        self.fallbacks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Resolves a character to the face that can actually render it.
    ///
    /// Glyph index `0` is `.notdef`, so a zero index means "no face has this glyph".
    pub(crate) fn face_key(&self, c: char, flags: CellFlags) -> FaceKey {
        let style = style_index(flags) as u8;
        let chain = self.chain_for_style(style);

        // Tier 1: Primary font for this style
        let glyph = chain.primary.lookup_glyph_index(c);
        if glyph != 0 {
            return FaceKey {
                face: 0,
                glyph,
                style,
            };
        }

        // Tier 2: User-configured fallback fonts for this style
        for (idx, fallback) in chain.fallbacks.iter().enumerate() {
            let glyph = fallback.lookup_glyph_index(c);
            if glyph != 0 {
                return FaceKey {
                    face: (idx + 1) as u16,
                    glyph,
                    style,
                };
            }
        }

        // Tier 3: Dynamic system fallback discovery
        let num_configured = (1 + chain.fallbacks.len()) as u16;
        let mut fallbacks = self.lock_fallbacks();
        let preferred = self.family();

        match fallbacks.resolve(c, style, preferred) {
            Some((face_idx, glyph)) => FaceKey {
                face: num_configured + face_idx,
                glyph,
                style,
            },
            None => FaceKey {
                face: 0,
                glyph: 0,
                style,
            },
        }
    }

    /// Rasterizes a resolved glyph.
    pub(crate) fn rasterize(&self, key: FaceKey) -> (fontdue::Metrics, Vec<u8>) {
        let chain = self.chain_for_style(key.style);
        let num_configured = (1 + chain.fallbacks.len()) as u16;

        if key.face == 0 {
            return chain.primary.rasterize_indexed(key.glyph, self.font_size);
        }
        if key.face < num_configured {
            let fallback_idx = (key.face - 1) as usize;
            return chain.fallbacks[fallback_idx].rasterize_indexed(key.glyph, self.font_size);
        }

        let fallback_idx = (key.face - num_configured) as usize;
        let fallbacks = self.lock_fallbacks();
        match fallbacks.faces.get(fallback_idx) {
            Some(face) => face.font.rasterize_indexed(key.glyph, self.font_size),
            None => chain.primary.rasterize_indexed(0, self.font_size),
        }
    }
}
