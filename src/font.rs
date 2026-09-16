//! System monospace fonts, cell metrics, and a growing grayscale glyph atlas.

use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use fontconfig::{CharSet, Fontconfig, Pattern};

use crate::grid::CellFlags;

// OpenGL ES 2 guarantees textures of at least this size. Bound the CPU copy to 4 MiB.
const MAX_ATLAS_SIZE: u32 = 2048;

/// Font metrics in pixels, shared by the renderer and PTY grid sizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellMetrics {
    pub cell_width: u32,
    pub cell_height: u32,
    pub ascent: i32,
}

/// Pixel coordinates stay valid when the atlas grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachedGlyph {
    pub position: [u32; 2],
    pub width: u32,
    pub height: u32,
    pub offset_x: i32,
    pub offset_y: i32,
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

/// A face fontconfig reported as covering a character the primary and user fallback font chain lacks.
struct FallbackFace {
    path: PathBuf,
    index: u32,
    style: u8,
    font: fontdue::Font,
}

const MAX_FALLBACK_FACES: usize = 64;
const MAX_RESOLVED_CACHE: usize = 4096;

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

/// Faces discovered on demand, keyed by (character, style) pairs.
///
/// Nothing is parsed until a frame renders a character the configured faces cannot draw, so
/// startup stays independent of how many fonts are installed.
#[derive(Default)]
struct FallbackCache {
    faces: Vec<FallbackFace>,
    /// Maps `(c, style)` to `Some((face_index, glyph_index))` or `None` if no installed font covers it.
    resolved: HashMap<(char, u8), Option<(u16, u16)>>,
    /// Tracks insertion order for deterministic FIFO eviction at capacity.
    resolved_order: VecDeque<(char, u8)>,
}

impl FallbackCache {
    /// Returns the index of a face covering `c` and the glyph index, loading the face on first use.
    fn resolve(&mut self, c: char, style: u8, preferred_family: &str) -> Option<(u16, u16)> {
        let key = (c, style);
        if let Some(cached) = self.resolved.get(&key) {
            return *cached;
        }
        let resolved = self.discover(c, style, preferred_family);
        if self.resolved.len() >= MAX_RESOLVED_CACHE
            && let Some(oldest) = self.resolved_order.pop_front()
        {
            self.resolved.remove(&oldest);
        }
        self.resolved_order.push_back(key);
        self.resolved.insert(key, resolved);
        resolved
    }

    fn discover(&mut self, c: char, style: u8, preferred_family: &str) -> Option<(u16, u16)> {
        // Fast path: check if any already loaded fallback face for this style covers `c`
        for (pos, face) in self.faces.iter().enumerate() {
            if face.style == style {
                let glyph = face.font.lookup_glyph_index(c);
                if glyph != 0 {
                    return Some((pos as u16, glyph));
                }
            }
        }

        let fc = fontconfig()?;
        let bold = (style & 1) != 0;
        let italic = (style & 2) != 0;
        let candidates = query_fontconfig_candidates(fc, preferred_family, bold, italic, c)?;

        for (path, index) in candidates {
            let position =
                match self.faces.iter().position(|face| {
                    face.path == path && face.index == index && face.style == style
                }) {
                    Some(position) => position,
                    None => {
                        let Ok(font) = load_font_file(&path, index) else {
                            continue;
                        };
                        if self.faces.len() >= MAX_FALLBACK_FACES {
                            self.faces.remove(0);
                            self.resolved.clear();
                            self.resolved_order.clear();
                        }
                        self.faces.push(FallbackFace {
                            path,
                            index,
                            style,
                            font,
                        });
                        self.faces.len() - 1
                    }
                };

            let glyph = self.faces[position].font.lookup_glyph_index(c);
            if glyph != 0 {
                return Some((position as u16, glyph));
            }
        }

        None
    }
}

/// Global shared Fontconfig handle initialized once per process.
pub(crate) fn fontconfig() -> Option<&'static Fontconfig> {
    static FC: OnceLock<Option<Fontconfig>> = OnceLock::new();
    FC.get_or_init(Fontconfig::new).as_ref()
}

/// Queries fontconfig to find a matching font file for a family and style.
fn match_family(
    fc: &Fontconfig,
    family_name: &str,
    bold: bool,
    italic: bool,
) -> Option<(PathBuf, u32)> {
    let mut pat = Pattern::new(fc).ok()?;
    let trimmed = family_name.trim();
    if !trimmed.is_empty()
        && !trimmed.eq_ignore_ascii_case("monospace")
        && let Ok(c_family) = CString::new(trimmed)
    {
        pat.add_string(fontconfig::FC_FAMILY, &c_family).ok()?;
    }
    if let Ok(c_mono) = CString::new("monospace") {
        pat.add_string(fontconfig::FC_FAMILY, &c_mono).ok()?;
    }

    if bold {
        pat.add_integer(fontconfig::FC_WEIGHT, fontconfig::FC_WEIGHT_BOLD)
            .ok()?;
    }
    if italic {
        pat.add_integer(fontconfig::FC_SLANT, fontconfig::FC_SLANT_ITALIC)
            .ok()?;
    }

    let matched = pat.font_match().ok()?;
    let filename = matched.filename().ok()?;
    let face_index = matched.face_index().ok()?;
    let index = u32::try_from(face_index).ok()?;
    Some((PathBuf::from(filename), index))
}

/// Asks fontconfig for ordered candidate faces covering `c`, taking style and monospace preferences into account.
fn query_fontconfig_candidates(
    fc: &Fontconfig,
    preferred_family: &str,
    bold: bool,
    italic: bool,
    c: char,
) -> Option<Vec<(PathBuf, u32)>> {
    let mut pattern = Pattern::new(fc).ok()?;
    let mut charset = CharSet::new(fc).ok()?;
    charset.add_char(c).ok()?;
    pattern.add_charset(charset).ok()?;

    if bold {
        pattern
            .add_integer(fontconfig::FC_WEIGHT, fontconfig::FC_WEIGHT_BOLD)
            .ok()?;
    }
    if italic {
        pattern
            .add_integer(fontconfig::FC_SLANT, fontconfig::FC_SLANT_ITALIC)
            .ok()?;
    }

    let trimmed = preferred_family.trim();
    if !trimmed.is_empty()
        && !trimmed.eq_ignore_ascii_case("monospace")
        && let Ok(c_family) = CString::new(trimmed)
    {
        pattern.add_string(fontconfig::FC_FAMILY, &c_family).ok()?;
    }
    if let Ok(c_mono) = CString::new("monospace") {
        pattern.add_string(fontconfig::FC_FAMILY, &c_mono).ok()?;
    }

    if let Ok(font_set) = pattern.sort_fonts(fontconfig::UnicodeCoverage::Trim) {
        let mut candidates = Vec::new();
        for p in font_set.iter().take(8) {
            if let Ok(filename) = p.filename()
                && let Ok(face_index) = p.face_index()
                && let Ok(index) = u32::try_from(face_index)
            {
                candidates.push((PathBuf::from(filename), index));
            }
        }
        if !candidates.is_empty() {
            return Some(candidates);
        }
    }

    let matched = pattern.font_match().ok()?;
    let path = PathBuf::from(matched.filename().ok()?);
    let index = u32::try_from(matched.face_index().ok()?).ok()?;
    Some(vec![(path, index)])
}

fn load_font_bytes(bytes: &[u8], collection_index: u32) -> io::Result<fontdue::Font> {
    fontdue::Font::from_bytes(
        bytes,
        fontdue::FontSettings {
            collection_index,
            scale: MAX_FONT_ZOOM_SCALE,
            load_substitutions: false,
        },
    )
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn load_font_file(path: &Path, collection_index: u32) -> io::Result<fontdue::Font> {
    let bytes = fs::read(path)?;
    load_font_bytes(&bytes, collection_index)
}

fn style_index(flags: CellFlags) -> usize {
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
    fallbacks: Mutex<FallbackCache>,
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
                let fallback = self
                    .bold
                    .get()
                    .map(|b| &b.primary)
                    .unwrap_or(&self.regular.primary);
                self.load_styled_chain(fc, true, true, fallback)
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

        Ok(Self {
            regular,
            regular_slots,
            bold: OnceLock::new(),
            italic: OnceLock::new(),
            bold_italic: OnceLock::new(),
            fallbacks: Mutex::new(FallbackCache::default()),
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

#[derive(Default)]
struct Shelf {
    x: u32,
    y: u32,
    height: u32,
}

impl Shelf {
    // Include a transparent pixel on every side; failed allocations leave the shelf alone.
    fn allocate(&mut self, width: u32, height: u32, bounds: [u32; 2]) -> Option<[u32; 2]> {
        let padded_width = width.checked_add(2)?;
        let padded_height = height.checked_add(2)?;
        if padded_width > bounds[0] || padded_height > bounds[1] {
            return None;
        }
        let (x, y, shelf_height) = if self.x + padded_width > bounds[0] {
            (0, self.y + self.height, 0)
        } else {
            (self.x, self.y, self.height)
        };
        if y + padded_height > bounds[1] {
            return None;
        }
        self.x = x + padded_width;
        self.y = y;
        self.height = shelf_height.max(padded_height);
        Some([x + 1, y + 1])
    }
}

/// A shelf-packed atlas with bounded growth. Clear it between frames to evict old glyphs.
pub struct GlyphAtlas {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Vec<u8>,
    pub(crate) dirty: bool,
    shelf: Shelf,
    // Fast O(1) cache directly keyed by (char, style_index).
    cache: HashMap<(char, u8), CachedGlyph>,
}

impl GlyphAtlas {
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        let width = width.clamp(4, MAX_ATLAS_SIZE);
        let height = height.clamp(4, MAX_ATLAS_SIZE);
        Self {
            width,
            height,
            pixels: vec![0; (width * height) as usize],
            dirty: true,
            shelf: Shelf::default(),
            cache: HashMap::new(),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.pixels.fill(0);
        self.cache.clear();
        self.shelf = Shelf::default();
        self.dirty = true;
    }

    fn grow(&mut self) -> bool {
        let width = (self.width * 2).min(MAX_ATLAS_SIZE);
        let height = (self.height * 2).min(MAX_ATLAS_SIZE);
        if width == self.width && height == self.height {
            return false;
        }
        let mut pixels = vec![0; (width * height) as usize];
        for (old_row, new_row) in self
            .pixels
            .chunks(self.width as usize)
            .zip(pixels.chunks_mut(width as usize))
        {
            new_row[..old_row.len()].copy_from_slice(old_row);
        }
        self.width = width;
        self.height = height;
        self.pixels = pixels;
        self.dirty = true;
        true
    }

    fn insert_bitmap(&mut self, metrics: fontdue::Metrics, bitmap: &[u8]) -> Option<CachedGlyph> {
        let width = u32::try_from(metrics.width).ok()?;
        let height = u32::try_from(metrics.height).ok()?;
        let mut glyph = CachedGlyph {
            position: [0, 0],
            width,
            height,
            offset_x: metrics.xmin,
            offset_y: metrics.ymin,
        };
        if width == 0 || height == 0 {
            return Some(glyph);
        }
        if width > MAX_ATLAS_SIZE - 2 || height > MAX_ATLAS_SIZE - 2 {
            return None;
        }
        glyph.position = loop {
            if let Some(position) = self
                .shelf
                .allocate(width, height, [self.width, self.height])
            {
                break position;
            }
            if !self.grow() {
                return None;
            }
        };
        let [x, y] = glyph.position;
        for row in 0..height {
            let src = (row * width) as usize;
            let dst = ((y + row) * self.width + x) as usize;
            self.pixels[dst..dst + width as usize]
                .copy_from_slice(&bitmap[src..src + width as usize]);
        }
        self.dirty = true;
        Some(glyph)
    }

    /// Looks up a glyph without modifying the atlas, for use after frame preparation.
    pub(crate) fn get(
        &self,
        c: char,
        flags: CellFlags,
        _fonts: &FontManager,
    ) -> Option<CachedGlyph> {
        let style = style_index(flags) as u8;
        self.cache.get(&(c, style)).copied()
    }

    /// Rasterizes once per character and style. Returns None when the atlas cannot fit it.
    pub fn get_or_insert(
        &mut self,
        c: char,
        flags: CellFlags,
        fonts: &FontManager,
    ) -> Option<CachedGlyph> {
        let style = style_index(flags) as u8;
        if let Some(glyph) = self.cache.get(&(c, style)) {
            return Some(*glyph);
        }
        let key = fonts.face_key(c, flags);
        let (metrics, bitmap) = fonts.rasterize(key);
        let glyph = self.insert_bitmap(metrics, &bitmap)?;
        self.cache.insert((c, style), glyph);
        Some(glyph)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fonts() -> &'static FontManager {
        static FONTS: OnceLock<FontManager> = OnceLock::new();
        FONTS.get_or_init(|| FontManager::load(14.0).expect("system monospace font"))
    }

    #[test]
    fn font_metrics_and_rasterization() {
        let fonts = fonts();
        assert!(fonts.metrics.cell_width > 0);
        assert!(fonts.metrics.cell_height >= fonts.metrics.ascent as u32);
        let (metrics, bitmap) = fonts.regular().rasterize('M', fonts.font_size);
        assert_eq!(bitmap.len(), metrics.width * metrics.height);
        assert!(bitmap.iter().any(|&pixel| pixel != 0));
    }

    #[test]
    fn rejects_invalid_font_sizes() {
        for size in [0.0, -1.0, 5.9, 72.1, f32::NAN, f32::INFINITY] {
            assert!(
                matches!(FontManager::load(size), Err(e) if e.kind() == io::ErrorKind::InvalidInput)
            );
        }
    }

    #[test]
    fn honors_collection_index_for_paths_and_bytes() {
        let fc = fontconfig().expect("fontconfig must be initialized");
        let (path, _) = match_family(fc, "monospace", false, false).expect("system monospace");
        let bytes = fs::read(&path).expect("read font bytes");
        assert!(load_font_bytes(&bytes, 0).is_ok());
        assert!(load_font_bytes(&bytes, u32::MAX).is_err());
        assert!(load_font_file(&path, 0).is_ok());
        assert!(load_font_file(&path, u32::MAX).is_err());
    }

    #[test]
    fn fallback_discovery_is_cached_per_character() {
        let mut cache = FallbackCache::default();
        let first = cache.resolve('中', 0, "monospace");
        let loaded = cache.faces.len();
        assert_eq!(cache.resolved.len(), 1);

        // Repeated lookups must reuse both the resolved answer and the parsed face.
        assert_eq!(cache.resolve('中', 0, "monospace"), first);
        assert_eq!(cache.faces.len(), loaded);
        assert_eq!(cache.resolved.len(), 1);

        if let Some((face_idx, glyph)) = first {
            assert_ne!(glyph, 0);
            assert_ne!(
                cache.faces[face_idx as usize].font.lookup_glyph_index('中'),
                0
            );
            let face = &cache.faces[face_idx as usize];
            assert!(load_font_file(&face.path, face.index).is_ok());
        }
    }

    #[test]
    fn unassigned_codepoints_do_not_resolve_to_a_glyph() {
        // U+0378 is unassigned, so no installed face should claim coverage for it.
        let key = fonts().face_key('\u{0378}', CellFlags::empty());
        assert_eq!(key.glyph, 0);
        assert_eq!(key.face, 0);
    }

    #[test]
    fn cjk_glyphs_resolve_through_a_fallback_face() {
        let fonts = fonts();
        assert_eq!(fonts.face_key('A', CellFlags::empty()).face, 0);
        if fonts
            .font_for_style(CellFlags::empty())
            .lookup_glyph_index('中')
            != 0
        {
            return; // The primary face already covers CJK on this system.
        }
        let key = fonts.face_key('中', CellFlags::empty());
        if key.face == 0 {
            eprintln!("skipping: no CJK fallback font installed");
            return;
        }
        let mut atlas = GlyphAtlas::new(256, 256);
        let glyph = atlas
            .get_or_insert('中', CellFlags::empty(), fonts)
            .expect("atlas must fit a CJK glyph");
        assert!(
            glyph.width > 0 && glyph.height > 0,
            "CJK fallback produced an empty bitmap"
        );
        assert!(atlas.pixels.iter().any(|&pixel| pixel != 0));
    }

    #[test]
    fn font_chain_prioritizes_configured_families() {
        let chain = FontManager::load_with_families(
            &["monospace".to_string(), "sans-serif".to_string()],
            14.0,
        )
        .expect("load font chain");
        assert!(!chain.families().is_empty());
        let key = chain.face_key('A', CellFlags::empty());
        assert_eq!(key.face, 0, "A should resolve from primary font");
    }

    #[test]
    fn styled_fallback_caching() {
        let mut cache = FallbackCache::default();
        let regular = cache.resolve('中', 0, "monospace");
        let bold = cache.resolve('中', 1, "monospace");
        if regular.is_some() {
            assert_eq!(cache.resolved.len(), 2);
        }
        if let (Some((r_idx, _)), Some((b_idx, _))) = (regular, bold) {
            assert!(r_idx <= b_idx || r_idx == b_idx);
        }
    }

    #[test]
    fn glyph_cache_ignores_color_attributes_and_caches_spaces() {
        let mut atlas = GlyphAtlas::new(32, 32);
        let glyph = atlas
            .get_or_insert('A', CellFlags::empty(), fonts())
            .unwrap();
        assert!(glyph.width > 0 && glyph.height > 0);
        atlas.dirty = false;
        let cached = atlas.get_or_insert('A', CellFlags::DIM | CellFlags::REVERSE, fonts());
        assert_eq!(cached, Some(glyph));
        assert!(!atlas.dirty);
        let space = atlas
            .get_or_insert(' ', CellFlags::empty(), fonts())
            .unwrap();
        assert_eq!(space.width * space.height, 0);
        assert!(!atlas.dirty);
        assert_eq!(atlas.get(' ', CellFlags::empty(), fonts()), Some(space));
    }

    #[test]
    fn styled_glyphs_have_separate_cache_entries() {
        let mut atlas = GlyphAtlas::new(64, 64);
        for flags in [
            CellFlags::empty(),
            CellFlags::BOLD,
            CellFlags::ITALIC,
            CellFlags::BOLD | CellFlags::ITALIC,
        ] {
            assert!(atlas.get_or_insert('A', flags, fonts()).is_some());
        }
        assert_eq!(atlas.cache.len(), 4);
    }

    #[test]
    fn growth_preserves_glyph_coordinates_pixels_and_padding() {
        let mut atlas = GlyphAtlas::new(32, 32);
        let first = atlas
            .get_or_insert('A', CellFlags::empty(), fonts())
            .unwrap();
        assert!(
            atlas
                .get_or_insert('W', CellFlags::empty(), fonts())
                .is_some()
        );
        assert!(
            atlas
                .get_or_insert('M', CellFlags::empty(), fonts())
                .is_some()
        );
        let relooked = atlas.get('A', CellFlags::empty(), fonts()).unwrap();
        assert_eq!(first, relooked);
        let [x, y] = relooked.position;
        assert!(x > 0 && y > 0);
        assert_eq!(atlas.pixels[(y * atlas.width + x - 1) as usize], 0);
    }

    #[test]
    fn shelf_accepts_exact_fits_and_preserves_padding() {
        let mut shelf = Shelf::default();
        let first = shelf.allocate(10, 10, [32, 32]).unwrap();
        assert_eq!(first, [1, 1]);
        assert_eq!(shelf.x, 12);
        assert_eq!(shelf.height, 12);
        let second = shelf.allocate(10, 10, [32, 32]).unwrap();
        assert_eq!(second, [13, 1]);
        assert_eq!(shelf.x, 24);
    }

    #[test]
    fn failed_allocation_does_not_consume_space() {
        let mut shelf = Shelf::default();
        let _ = shelf.allocate(10, 10, [32, 32]).unwrap();
        let state_before = (shelf.x, shelf.y, shelf.height);
        assert!(shelf.allocate(30, 30, [32, 32]).is_none());
        assert_eq!((shelf.x, shelf.y, shelf.height), state_before);
    }

    #[test]
    fn atlas_growth_and_full_eviction() {
        let mut atlas = GlyphAtlas::new(16, 16);
        let mut full = false;
        for c in 'A'..='Z' {
            if atlas
                .get_or_insert(c, CellFlags::empty(), fonts())
                .is_none()
            {
                full = true;
                break;
            }
        }
        if full {
            atlas.clear();
            assert!(
                atlas
                    .get_or_insert('A', CellFlags::empty(), fonts())
                    .is_some()
            );
        }
    }

    #[test]
    fn full_atlas_can_be_reused_between_frames() {
        let mut atlas = GlyphAtlas::new(16, 16);
        let _ = atlas.get_or_insert('A', CellFlags::empty(), fonts());
        atlas.clear();
        assert!(atlas.cache.is_empty());
        assert_eq!(atlas.pixels.iter().sum::<u8>(), 0);
        assert!(atlas.dirty);
        assert!(
            atlas
                .get_or_insert('A', CellFlags::empty(), fonts())
                .is_some()
        );
    }

    #[test]
    fn test_set_font_size_updates_metrics_in_memory_without_reloading() {
        let mut fonts = FontManager::load(14.0).expect("load monospace");
        let initial_width = fonts.metrics.cell_width;
        let initial_height = fonts.metrics.cell_height;

        // Scale up to 28.0 (double size)
        assert!(fonts.set_font_size(28.0));
        assert_eq!(fonts.font_size(), 28.0);
        assert!(fonts.metrics.cell_width > initial_width);
        assert!(fonts.metrics.cell_height > initial_height);

        // Setting same size returns false
        assert!(!fonts.set_font_size(28.0));

        // Invalid sizes rejected
        assert!(!fonts.set_font_size(0.0));
        assert!(!fonts.set_font_size(-5.0));
        assert!(!fonts.set_font_size(f32::NAN));
        assert!(!fonts.set_font_size(f32::INFINITY));
        assert_eq!(fonts.font_size(), 28.0);

        // Scale back down
        assert!(fonts.set_font_size(14.0));
        assert_eq!(fonts.font_size(), 14.0);
        assert_eq!(fonts.metrics.cell_width, initial_width);
        assert_eq!(fonts.metrics.cell_height, initial_height);
    }

    #[test]
    fn fallback_cache_has_bounded_capacity() {
        let mut cache = FallbackCache::default();
        for i in 0..MAX_RESOLVED_CACHE {
            let c = char::from_u32(0x1000 + i as u32).unwrap_or('A');
            cache.resolved_order.push_back((c, 0));
            cache.resolved.insert((c, 0), None);
        }
        assert_eq!(cache.resolved.len(), MAX_RESOLVED_CACHE);
        let oldest = char::from_u32(0x1000).unwrap();
        assert!(cache.resolved.contains_key(&(oldest, 0)));

        // Resolving a new character evicts the oldest entry (FIFO)
        let _ = cache.resolve('Z', 1, "monospace");
        assert_eq!(cache.resolved.len(), MAX_RESOLVED_CACHE);
        assert!(!cache.resolved.contains_key(&(oldest, 0)));
        assert!(cache.resolved.contains_key(&('Z', 1)));
    }
}
