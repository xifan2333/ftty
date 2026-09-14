//! System monospace fonts, cell metrics, and a growing grayscale glyph atlas.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use font_kit::family_name::FamilyName;
use font_kit::handle::Handle;
use font_kit::properties::{Properties, Style, Weight};
use font_kit::source::SystemSource;
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
///
/// The atlas is keyed by this instead of the bare glyph index because glyph index 0
/// (`.notdef`) is shared by every face that lacks a character, and two different faces
/// can use the same index for different outlines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct FaceKey {
    /// `0` is the styled primary face, `n >= 1` selects `fallbacks[n - 1]`.
    pub(crate) face: u16,
    pub(crate) glyph: u16,
    pub(crate) style: u8,
}

/// A face fontconfig reported as covering a character the primary face lacks.
struct FallbackFace {
    path: PathBuf,
    index: u32,
    font: fontdue::Font,
}

/// Faces discovered on demand, keyed by the characters that needed them.
///
/// Nothing is parsed until a frame renders a character the primary face cannot draw, so
/// startup stays independent of how many fonts are installed.
#[derive(Default)]
struct FallbackCache {
    faces: Vec<FallbackFace>,
    /// `None` records that no installed face covers the character.
    resolved: HashMap<char, Option<u16>>,
}

impl FallbackCache {
    /// Returns the index of a face covering `c`, loading it on first use.
    fn resolve(&mut self, c: char, font_size: f32) -> Option<u16> {
        if let Some(cached) = self.resolved.get(&c) {
            return *cached;
        }
        let resolved = self.discover(c, font_size);
        self.resolved.insert(c, resolved);
        resolved
    }

    fn discover(&mut self, c: char, font_size: f32) -> Option<u16> {
        let (path, index) = query_fontconfig(c)?;
        let position = match self
            .faces
            .iter()
            .position(|face| face.path == path && face.index == index)
        {
            Some(position) => position,
            None => {
                let handle = Handle::Path {
                    path: path.clone(),
                    font_index: index,
                };
                let font = load_handle(&handle, font_size).ok()?;
                self.faces.push(FallbackFace { path, index, font });
                self.faces.len() - 1
            }
        };
        // Fontconfig always answers with *some* font, which is not necessarily one that
        // covers the character, so coverage is verified before the face is accepted.
        let glyph = self.faces[position].font.lookup_glyph_index(c);
        (glyph != 0).then_some(position as u16)
    }
}

/// Asks fontconfig which installed face covers `c`, equivalent to
/// `fc-match ':charset=<codepoint>'`, so no font family is hard-coded.
fn query_fontconfig(c: char) -> Option<(PathBuf, u32)> {
    let fc = Fontconfig::new()?;
    let mut pattern = Pattern::new(&fc).ok()?;
    let mut charset = CharSet::new(&fc).ok()?;
    charset.add_char(c).ok()?;
    pattern.add_charset(charset).ok()?;
    let matched = pattern.font_match().ok()?;
    let path = PathBuf::from(matched.filename().ok()?);
    let index = u32::try_from(matched.face_index().ok()?).ok()?;
    Some((path, index))
}

/// Loads styled faces on first use rather than during terminal startup.
pub struct FontManager {
    regular: fontdue::Font,
    styles: [OnceLock<Option<fontdue::Font>>; 3],
    fallbacks: Mutex<FallbackCache>,
    family: String,
    font_size: f32,
    pub metrics: CellMetrics,
}

fn style_index(flags: CellFlags) -> usize {
    usize::from(flags.contains(CellFlags::BOLD))
        | (usize::from(flags.contains(CellFlags::ITALIC)) << 1)
}

fn load_handle(handle: &Handle, font_size: f32) -> io::Result<fontdue::Font> {
    let (bytes, collection_index): (Cow<'_, [u8]>, _) = match handle {
        Handle::Path { path, font_index } => (Cow::Owned(fs::read(path)?), *font_index),
        Handle::Memory { bytes, font_index } => (Cow::Borrowed(bytes.as_slice()), *font_index),
    };
    fontdue::Font::from_bytes(
        bytes,
        fontdue::FontSettings {
            collection_index,
            scale: font_size,
            load_substitutions: false,
        },
    )
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn primary_families(family: &str) -> Vec<FamilyName> {
    if family.eq_ignore_ascii_case("monospace") || family.trim().is_empty() {
        vec![FamilyName::Monospace]
    } else {
        vec![FamilyName::Title(family.to_string()), FamilyName::Monospace]
    }
}

fn load_family(
    families: &[FamilyName],
    style: usize,
    font_size: f32,
) -> io::Result<(Handle, fontdue::Font)> {
    let mut properties = Properties::new();
    if style & 1 != 0 {
        properties.weight(Weight::BOLD);
    }
    if style & 2 != 0 {
        properties.style(Style::Italic);
    }
    let handle = SystemSource::new()
        .select_best_match(families, &properties)
        .map_err(|e| io::Error::new(io::ErrorKind::NotFound, e))?;
    let font = load_handle(&handle, font_size)?;
    Ok((handle, font))
}

fn load_font_face(family: &str, style: usize, font_size: f32) -> io::Result<fontdue::Font> {
    load_family(&primary_families(family), style, font_size).map(|(_, font)| font)
}

impl FontManager {
    /// Discovers and loads a font face by family name and size in pixels per em.
    ///
    /// # Errors
    /// Returns an error for an invalid size, missing font, or unreadable font data.
    pub fn load_with_family(family: &str, font_size: f32) -> io::Result<Self> {
        if !font_size.is_finite() || font_size <= 0.0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid font size",
            ));
        }
        let regular = load_font_face(family, 0, font_size)?;
        let cell_width = regular
            .metrics('M', font_size)
            .advance_width
            .ceil()
            .max(1.0) as u32;
        let (cell_height, ascent) = regular
            .horizontal_line_metrics(font_size)
            .map(|line| {
                (
                    line.new_line_size.ceil().max(1.0) as u32,
                    line.ascent.ceil() as i32,
                )
            })
            .unwrap_or((font_size.ceil().max(1.0) as u32, font_size.ceil() as i32));
        Ok(Self {
            regular,
            styles: std::array::from_fn(|_| OnceLock::new()),
            fallbacks: Mutex::new(FallbackCache::default()),
            family: family.to_string(),
            font_size,
            metrics: CellMetrics {
                cell_width,
                cell_height,
                ascent,
            },
        })
    }

    /// Discovers and loads a system monospace face at a size in pixels per em.
    ///
    /// # Errors
    /// Returns an error for an invalid size, missing font, or unreadable font data.
    pub fn load(font_size: f32) -> io::Result<Self> {
        Self::load_with_family("monospace", font_size)
    }

    #[must_use]
    pub fn family(&self) -> &str {
        &self.family
    }

    #[must_use]
    pub fn font_size(&self) -> f32 {
        self.font_size
    }

    /// Falls back to the regular face if a styled face cannot be loaded.
    #[must_use]
    pub fn font_for_style(&self, flags: CellFlags) -> &fontdue::Font {
        self.face_for_style(style_index(flags))
    }

    fn face_for_style(&self, style: usize) -> &fontdue::Font {
        if style == 0 {
            return &self.regular;
        }
        self.styles[style - 1]
            .get_or_init(|| load_font_face(&self.family, style, self.font_size).ok())
            .as_ref()
            .unwrap_or(&self.regular)
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
        let style = style_index(flags);
        let glyph = self.face_for_style(style).lookup_glyph_index(c);
        if glyph != 0 {
            return FaceKey {
                face: 0,
                glyph,
                style: style as u8,
            };
        }
        let mut fallbacks = self.lock_fallbacks();
        match fallbacks.resolve(c, self.font_size) {
            Some(index) => match fallbacks.faces.get(index as usize) {
                Some(face) => FaceKey {
                    face: index + 1,
                    glyph: face.font.lookup_glyph_index(c),
                    style: style as u8,
                },
                None => FaceKey {
                    face: 0,
                    glyph: 0,
                    style: style as u8,
                },
            },
            None => FaceKey {
                face: 0,
                glyph: 0,
                style: style as u8,
            },
        }
    }

    /// Rasterizes a resolved glyph. The fallback cache is locked only long enough to own
    /// the bitmap, so no borrow of it escapes this call.
    pub(crate) fn rasterize(&self, key: FaceKey) -> (fontdue::Metrics, Vec<u8>) {
        if key.face == 0 {
            return self
                .face_for_style(key.style as usize)
                .rasterize_indexed(key.glyph, self.font_size);
        }
        let fallbacks = self.lock_fallbacks();
        match fallbacks.faces.get(key.face as usize - 1) {
            Some(face) => face.font.rasterize_indexed(key.glyph, self.font_size),
            None => self
                .face_for_style(key.style as usize)
                .rasterize_indexed(0, self.font_size),
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
    // Cache resolved glyphs so unsupported Unicode characters share the missing-glyph bitmap.
    cache: HashMap<FaceKey, CachedGlyph>,
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
        fonts: &FontManager,
    ) -> Option<CachedGlyph> {
        self.cache.get(&fonts.face_key(c, flags)).copied()
    }

    /// Rasterizes once per face and glyph. Returns None when the atlas cannot fit it.
    pub fn get_or_insert(
        &mut self,
        c: char,
        flags: CellFlags,
        fonts: &FontManager,
    ) -> Option<CachedGlyph> {
        let key = fonts.face_key(c, flags);
        if let Some(glyph) = self.cache.get(&key) {
            return Some(*glyph);
        }
        let (metrics, bitmap) = fonts.rasterize(key);
        let glyph = self.insert_bitmap(metrics, &bitmap)?;
        self.cache.insert(key, glyph);
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
        let (metrics, bitmap) = fonts.regular.rasterize('M', fonts.font_size);
        assert_eq!(bitmap.len(), metrics.width * metrics.height);
        assert!(bitmap.iter().any(|&pixel| pixel != 0));
    }

    #[test]
    fn rejects_invalid_font_sizes() {
        for size in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert!(
                matches!(FontManager::load(size), Err(e) if e.kind() == io::ErrorKind::InvalidInput)
            );
        }
    }

    #[test]
    fn honors_collection_index_for_paths_and_memory() {
        let handle = SystemSource::new()
            .select_best_match(&[FamilyName::Monospace], &Properties::new())
            .unwrap();
        let bytes = match handle {
            Handle::Path { path, .. } => {
                let bytes = fs::read(&path).unwrap();
                let invalid = Handle::Path {
                    path,
                    font_index: u32::MAX,
                };
                assert!(load_handle(&invalid, 14.0).is_err());
                bytes
            }
            Handle::Memory { bytes, .. } => (*bytes).clone(),
        };
        let invalid = Handle::Memory {
            bytes: std::sync::Arc::new(bytes),
            font_index: u32::MAX,
        };
        assert!(load_handle(&invalid, 14.0).is_err());
    }

    #[test]
    fn fallback_discovery_is_cached_per_character() {
        let mut cache = FallbackCache::default();
        let first = cache.resolve('中', 14.0);
        let loaded = cache.faces.len();
        assert_eq!(cache.resolved.len(), 1);

        // Repeated lookups must reuse both the resolved answer and the parsed face.
        assert_eq!(cache.resolve('中', 14.0), first);
        assert_eq!(cache.faces.len(), loaded);
        assert_eq!(cache.resolved.len(), 1);

        if let Some(index) = first {
            assert_ne!(cache.faces[index as usize].font.lookup_glyph_index('中'), 0);
            // The cached face path/index must round-trip through the handle loader.
            let face = &cache.faces[index as usize];
            let handle = Handle::Path {
                path: face.path.clone(),
                font_index: face.index,
            };
            assert!(load_handle(&handle, 14.0).is_ok());
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
            atlas.get_or_insert('M', flags, fonts()).unwrap();
        }
        assert_eq!(atlas.cache.len(), 4);
    }

    #[test]
    fn shelf_accepts_exact_fits_and_preserves_padding() {
        let mut shelf = Shelf::default();
        assert_eq!(shelf.allocate(2, 2, [8, 8]), Some([1, 1]));
        assert_eq!(shelf.allocate(2, 2, [8, 8]), Some([5, 1]));
        assert_eq!(shelf.allocate(6, 2, [8, 8]), Some([1, 5]));
        assert_eq!(shelf.allocate(1, 1, [8, 8]), None);
    }

    #[test]
    fn failed_allocation_does_not_consume_space() {
        let mut shelf = Shelf::default();
        assert_eq!(shelf.allocate(7, 1, [8, 8]), None);
        assert_eq!(shelf.allocate(1, 7, [8, 8]), None);
        assert_eq!(shelf.allocate(u32::MAX, 1, [8, 8]), None);
        assert_eq!(shelf.allocate(6, 6, [8, 8]), Some([1, 1]));
    }

    #[test]
    fn growth_preserves_glyph_coordinates_pixels_and_padding() {
        let mut atlas = GlyphAtlas::new(4, 4);
        let metrics = fontdue::Metrics {
            width: 2,
            height: 2,
            ..Default::default()
        };
        let first = atlas.insert_bitmap(metrics, &[1, 2, 3, 4]).unwrap();
        let second = atlas.insert_bitmap(metrics, &[5, 6, 7, 8]).unwrap();
        assert_eq!([atlas.width, atlas.height], [8, 8]);
        assert_eq!(first.position, [1, 1]);
        assert_eq!(second.position, [5, 1]);
        assert_eq!(&atlas.pixels[9..11], &[1, 2]);
        assert_eq!(&atlas.pixels[17..19], &[3, 4]);
        assert_eq!(&atlas.pixels[13..15], &[5, 6]);
        assert_eq!(&atlas.pixels[21..23], &[7, 8]);
        assert_eq!(&atlas.pixels[11..13], &[0, 0]);
    }

    #[test]
    fn full_atlas_can_be_reused_between_frames() {
        let mut atlas = GlyphAtlas::new(MAX_ATLAS_SIZE, MAX_ATLAS_SIZE);
        let metrics = fontdue::Metrics {
            width: 2046,
            height: 2046,
            ..Default::default()
        };
        let bitmap = vec![255; metrics.width * metrics.height];
        assert!(atlas.insert_bitmap(metrics, &bitmap).is_some());
        assert!(atlas.insert_bitmap(metrics, &bitmap).is_none());
        atlas.clear();
        assert!(atlas.pixels.iter().all(|&pixel| pixel == 0));
        assert!(atlas.insert_bitmap(metrics, &bitmap).is_some());
    }
}
