//! Font loading, multi-family fallback chaining, and dynamic text metrics calculation.

pub mod atlas;
pub(crate) mod face;
pub(crate) mod fallback;

#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::io;
use std::path::PathBuf;

use fontconfig::Fontconfig;

use crate::font::fallback::{FallbackCache, fontconfig, load_font_file, match_family};
use crate::grid::CellFlags;

pub use atlas::{CachedGlyph, GlyphAtlas, MAX_ATLAS_SIZE};
pub use face::{Font, LineMetrics, RasterizedGlyph};

/// Cell size and baseline alignment metrics for the active font and font size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellMetrics {
    pub cell_width: u32,
    pub cell_height: u32,
    pub ascent: i32,
}

/// Extracts cell dimensions (width, height, ascent) from raw TrueType/OpenType font bytes in sub-milliseconds.
#[must_use]
pub fn parse_cell_metrics_from_bytes(
    bytes: &[u8],
    collection_index: u32,
    font_size: f32,
) -> Option<CellMetrics> {
    if !font_size.is_finite() || font_size < MIN_FONT_SIZE || font_size > MAX_FONT_SIZE {
        return None;
    }
    let face = ttf_parser::Face::parse(bytes, collection_index).ok()?;
    let units_per_em = face.units_per_em() as f32;
    if units_per_em <= 0.0 {
        return None;
    }
    let factor = font_size / units_per_em;

    let (ascent, descent, line_gap) = (
        face.ascender() as i32,
        face.descender() as i32,
        face.line_gap() as i32,
    );
    let new_line_size = (ascent - descent + line_gap) as f32 * factor;
    let cell_height = new_line_size.ceil().max(1.0) as u32;
    let cell_ascent = (ascent as f32 * factor).ceil() as i32;

    let glyph_id = face.glyph_index('0').unwrap_or(ttf_parser::GlyphId(0));
    let advance_width = face.glyph_hor_advance(glyph_id).unwrap_or(0) as f32 * factor;
    let cell_width = advance_width.ceil().max(1.0) as u32;

    Some(CellMetrics {
        cell_width,
        cell_height,
        ascent: cell_ascent,
    })
}

/// Extracts cell dimensions directly from a font file in sub-milliseconds without parsing vector glyph outlines.
///
/// # Errors
/// Returns [`std::io::Error`] if the font file cannot be read or table parsing fails.
pub fn parse_cell_metrics_from_file(
    path: &std::path::Path,
    collection_index: u32,
    font_size: f32,
) -> std::io::Result<CellMetrics> {
    let bytes = std::fs::read(path)?;
    parse_cell_metrics_from_bytes(&bytes, collection_index, font_size).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "failed to parse font tables for cell metrics",
        )
    })
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
#[derive(Clone)]
struct StyleChain {
    primary: Font,
    fallbacks: Vec<Font>,
}

/// Minimum font size in points/pixels supported by the terminal.
pub const MIN_FONT_SIZE: f32 = 6.0;
/// Maximum font size in points/pixels supported by the terminal.
pub const MAX_FONT_SIZE: f32 = 72.0;

pub(crate) fn style_index(flags: CellFlags) -> usize {
    usize::from(flags.contains(CellFlags::BOLD))
        | (usize::from(flags.contains(CellFlags::ITALIC)) << 1)
}

/// Loads configured font chain and on-demand fallback faces with cell metrics calculation.
pub struct FontManager {
    regular: StyleChain,
    regular_slots: Vec<Option<Font>>,
    chains: RefCell<[Option<StyleChain>; 4]>,
    fallbacks: RefCell<FallbackCache>,
    families: Vec<String>,
    primary_path: PathBuf,
    primary_index: u32,
    font_size: f32,
    pub subpixel: bool,
    pub metrics: CellMetrics,
}

impl FontManager {
    fn ensure_chain(&self, style: u8) {
        let idx = (style as usize).min(3);
        if self.chains.borrow()[idx].is_some() {
            return;
        }
        let fc = fontconfig();
        let chain = match style {
            1 => self.load_styled_chain(fc, true, false, &self.regular.primary),
            2 => self.load_styled_chain(fc, false, true, &self.regular.primary),
            3 => {
                self.ensure_chain(1);
                let binding = self.chains.borrow();
                let bold_primary = binding[1]
                    .as_ref()
                    .map(|c| &c.primary)
                    .unwrap_or(&self.regular.primary);
                self.load_styled_chain(fc, true, true, bold_primary)
            }
            _ => return,
        };
        self.chains.borrow_mut()[idx] = Some(chain);
    }

    fn load_styled_chain(
        &self,
        fc: Option<&Fontconfig>,
        bold: bool,
        italic: bool,
        fallback_primary: &Font,
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
        Self::load_with_families_and_subpixel(families, font_size, false)
    }

    /// Discovers and loads an ordered list of font families with explicit subpixel setting.
    ///
    /// # Errors
    /// Returns an error for an invalid size, missing font, or unreadable font data.
    pub fn load_with_families_and_subpixel(
        families: &[String],
        font_size: f32,
        subpixel: bool,
    ) -> io::Result<Self> {
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
        let (primary_path, primary_index) = match_family(fc, primary_name, false, false)
            .or_else(|| match_family(fc, "monospace", false, false))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no monospace font found"))?;
        let primary_regular = load_font_file(&primary_path, primary_index)?;

        // 2. Compute metrics from the primary Regular font tables
        let metrics = parse_cell_metrics_from_file(&primary_path, primary_index, font_size)
            .unwrap_or_else(|_| {
                let cell_width = primary_regular
                    .glyph_advance_width('0', font_size)
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
                CellMetrics {
                    cell_width,
                    cell_height,
                    ascent,
                }
            });

        // 3. User fallback regular slots maintain 1:1 index alignment with fallback_names.
        let regular_slots: Vec<Option<Font>> = fallback_names
            .iter()
            .map(|name| {
                let (path, index) = match_family(fc, name, false, false)?;
                load_font_file(&path, index).ok()
            })
            .collect();

        let regular_fallbacks: Vec<Font> = regular_slots.iter().flatten().cloned().collect();

        let regular = StyleChain {
            primary: primary_regular,
            fallbacks: regular_fallbacks,
        };

        Ok(Self {
            regular: regular.clone(),
            regular_slots,
            chains: RefCell::new([Some(regular), None, None, None]),
            fallbacks: RefCell::new(FallbackCache::default()),
            families: valid_families,
            primary_path,
            primary_index,
            font_size,
            subpixel,
            metrics,
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

        if let Ok(metrics) =
            parse_cell_metrics_from_file(&self.primary_path, self.primary_index, new_size)
        {
            self.metrics = metrics;
        } else {
            let cell_width = self
                .regular
                .primary
                .glyph_advance_width('0', new_size)
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
        }

        true
    }

    /// Falls back to the regular face if a styled face cannot be loaded.
    #[must_use]
    pub fn font_for_style(&self, flags: CellFlags) -> Font {
        let style = (style_index(flags) as u8).min(3);
        self.ensure_chain(style);
        self.chains.borrow()[style as usize]
            .as_ref()
            .map(|c| c.primary.clone())
            .unwrap_or_else(|| self.regular.primary.clone())
    }

    #[cfg(test)]
    pub(crate) fn regular(&self) -> Font {
        self.regular.primary.clone()
    }

    /// Resolves a character to the face that can actually render it.
    ///
    /// Glyph index `0` is `.notdef`, so a zero index means "no face has this glyph".
    pub(crate) fn face_key(&self, c: char, flags: CellFlags) -> FaceKey {
        let style = (style_index(flags) as u8).min(3);
        self.ensure_chain(style);
        let binding = self.chains.borrow();
        let chain = binding[style as usize].as_ref().unwrap_or(&self.regular);

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

        let num_configured = (1 + chain.fallbacks.len()) as u16;
        drop(binding);

        // Tier 3: Dynamic system fallback discovery
        let mut fallbacks = self.fallbacks.borrow_mut();
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
    pub(crate) fn rasterize(&self, key: FaceKey) -> RasterizedGlyph {
        if key.glyph == 0 {
            return RasterizedGlyph::empty();
        }
        let style = key.style.min(3);
        self.ensure_chain(style);
        let binding = self.chains.borrow();
        let chain = binding[style as usize].as_ref().unwrap_or(&self.regular);
        let num_configured = (1 + chain.fallbacks.len()) as u16;

        if key.face == 0 {
            return chain
                .primary
                .rasterize_indexed(key.glyph, self.font_size, self.subpixel);
        }
        if key.face < num_configured {
            let fallback_idx = (key.face - 1) as usize;
            return chain.fallbacks[fallback_idx].rasterize_indexed(
                key.glyph,
                self.font_size,
                self.subpixel,
            );
        }

        let fallback_idx = (key.face - num_configured) as usize;
        drop(binding);
        let fallbacks = self.fallbacks.borrow();
        match fallbacks.faces.get(fallback_idx) {
            Some(face) => face
                .font
                .rasterize_indexed(key.glyph, self.font_size, self.subpixel),
            None => self
                .regular
                .primary
                .rasterize_indexed(0, self.font_size, self.subpixel),
        }
    }
}
