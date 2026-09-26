//! Font loading, multi-family fallback chaining, and dynamic text metrics calculation.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::hash::Hasher;
use std::io::{self, Cursor, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::SystemTime;

use fontconfig::{CharSet, Fontconfig, Pattern};
use freetype::RenderMode;
use freetype::face::LoadFlag;

use crate::grid::CellFlags;

// --- Atlas ---

pub const MAX_ATLAS_SIZE: u32 = 1024;
pub const INITIAL_ATLAS_SIZE: u32 = 512;

/// Pixel coordinates stay valid when the atlas grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachedGlyph {
    pub position: [u32; 2],
    pub width: u32,
    pub height: u32,
    pub offset_x: i32,
    pub offset_y: i32,
}

#[derive(Default)]
#[doc(hidden)]
pub struct Shelf {
    pub x: u32,
    pub y: u32,
    pub height: u32,
}

impl Shelf {
    // Include a transparent pixel on every side; failed allocations leave the shelf alone.
    #[doc(hidden)]
    pub fn allocate(&mut self, width: u32, height: u32, bounds: [u32; 2]) -> Option<[u32; 2]> {
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
    #[doc(hidden)]
    pub width: u32,
    #[doc(hidden)]
    pub height: u32,
    #[doc(hidden)]
    pub pixels: Vec<u8>,
    #[doc(hidden)]
    pub dirty: bool,
    #[doc(hidden)]
    pub full_upload: bool,
    #[doc(hidden)]
    pub dirty_rect: Option<[u32; 4]>,
    shelf: Shelf,
    // Fast L1 array cache for ASCII characters (0..127) across 4 styles (0..3).
    #[doc(hidden)]
    pub ascii_cache: [Option<CachedGlyph>; 128 * 4],
    #[doc(hidden)]
    pub cache: HashMap<(char, u8), CachedGlyph>,
}

impl Default for GlyphAtlas {
    fn default() -> Self {
        Self::new(INITIAL_ATLAS_SIZE, INITIAL_ATLAS_SIZE)
    }
}

impl GlyphAtlas {
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        let width = width.clamp(4, MAX_ATLAS_SIZE);
        let height = height.clamp(4, MAX_ATLAS_SIZE);
        Self {
            width,
            height,
            pixels: vec![0; (width * height * 4) as usize],
            dirty: true,
            full_upload: true,
            dirty_rect: None,
            shelf: Shelf::default(),
            ascii_cache: [None; 128 * 4],
            cache: HashMap::new(),
        }
    }

    #[doc(hidden)]
    pub fn clear(&mut self) {
        self.pixels.fill(0);
        self.cache.clear();
        self.ascii_cache = [None; 128 * 4];
        self.shelf = Shelf::default();
        self.dirty = true;
        self.full_upload = true;
        self.dirty_rect = None;
    }

    fn grow(&mut self) -> bool {
        let width = (self.width * 2).min(MAX_ATLAS_SIZE);
        let height = (self.height * 2).min(MAX_ATLAS_SIZE);
        if width == self.width && height == self.height {
            return false;
        }
        let mut pixels = vec![0; (width * height * 4) as usize];
        let old_row_bytes = (self.width * 4) as usize;
        let new_row_bytes = (width * 4) as usize;
        for (old_row, new_row) in self
            .pixels
            .chunks(old_row_bytes)
            .zip(pixels.chunks_mut(new_row_bytes))
        {
            new_row[..old_row.len()].copy_from_slice(old_row);
        }
        self.width = width;
        self.height = height;
        self.pixels = pixels;
        self.dirty = true;
        self.full_upload = true;
        self.dirty_rect = None;
        true
    }

    fn insert_bitmap(&mut self, glyph: &RasterizedGlyph) -> Option<CachedGlyph> {
        let width = glyph.width;
        let height = glyph.height;
        let mut cached = CachedGlyph {
            position: [0, 0],
            width,
            height,
            offset_x: glyph.offset_x,
            offset_y: glyph.offset_y,
        };
        if width == 0 || height == 0 {
            return Some(cached);
        }
        if width > MAX_ATLAS_SIZE - 2 || height > MAX_ATLAS_SIZE - 2 {
            return None;
        }
        let grew = if let Some(position) =
            self.shelf
                .allocate(width, height, [self.width, self.height])
        {
            cached.position = position;
            false
        } else if self.grow() {
            let position = self
                .shelf
                .allocate(width, height, [self.width, self.height])?;
            cached.position = position;
            true
        } else {
            return None;
        };
        let [x, y] = cached.position;
        let row_bytes = (width * 4) as usize;
        let pitch = glyph.pitch.max(row_bytes);
        for row in 0..height {
            let src = (row as usize) * pitch;
            let dst = (((y + row) * self.width + x) * 4) as usize;
            if src + row_bytes <= glyph.pixels.len() {
                self.pixels[dst..dst + row_bytes]
                    .copy_from_slice(&glyph.pixels[src..src + row_bytes]);
            }
        }
        self.dirty = true;
        if !grew && !self.full_upload {
            let max_x = x + width;
            let max_y = y + height;
            self.dirty_rect = match self.dirty_rect {
                Some([x0, y0, x1, y1]) => {
                    Some([x0.min(x), y0.min(y), x1.max(max_x), y1.max(max_y)])
                }
                None => Some([x, y, max_x, max_y]),
            };
        }
        Some(cached)
    }

    /// Looks up a glyph without modifying the atlas, for use after frame preparation.
    #[doc(hidden)]
    pub fn get(&self, c: char, flags: CellFlags, _fonts: &FontManager) -> Option<CachedGlyph> {
        let style = style_index(flags) as u8;
        if c.is_ascii() && (style as usize) < 4 {
            let idx = (c as usize) | ((style as usize) << 7);
            if let Some(glyph) = self.ascii_cache[idx] {
                return Some(glyph);
            }
        }
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
        let is_ascii = c.is_ascii() && (style as usize) < 4;
        let ascii_idx = (c as usize) | ((style as usize) << 7);
        if is_ascii {
            if let Some(glyph) = self.ascii_cache[ascii_idx] {
                return Some(glyph);
            }
        } else if let Some(glyph) = self.cache.get(&(c, style)) {
            return Some(*glyph);
        }

        let key = fonts.face_key(c, flags);
        let rasterized = fonts.rasterize(key);
        let glyph = self.insert_bitmap(&rasterized)?;
        if glyph.width == 0 && glyph.height == 0 && !c.is_whitespace() {
            return None;
        }
        if is_ascii {
            self.ascii_cache[ascii_idx] = Some(glyph);
        } else {
            self.cache.insert((c, style), glyph);
        }
        Some(glyph)
    }
}

// --- Face ---

fn ft_global_lock() -> MutexGuard<'static, ()> {
    static FT_LOCK: Mutex<()> = Mutex::new(());
    FT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn ft_library() -> io::Result<&'static freetype::Library> {
    static LIB: OnceLock<Result<freetype::Library, String>> = OnceLock::new();
    let res = LIB.get_or_init(|| {
        let lib = freetype::Library::init().map_err(|e| format!("{e:?}"))?;
        let _ = lib.set_lcd_filter(freetype::LcdFilter::LcdFilterDefault);
        Ok(lib)
    });
    match res {
        Ok(lib) => Ok(lib),
        Err(e) => Err(io::Error::other(format!(
            "initialize FreeType library: {e}"
        ))),
    }
}

/// Horizontal line metrics for cell height and baseline positioning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineMetrics {
    pub new_line_size: f32,
    pub ascent: f32,
}

/// Output of a lazily rasterized glyph bitmap with normalized top-to-bottom scanlines.
#[derive(Debug, Clone)]
pub struct RasterizedGlyph {
    pub width: u32,
    pub height: u32,
    pub offset_x: i32,
    pub offset_y: i32,
    pub pitch: usize,
    pub pixels: Vec<u8>,
}

impl RasterizedGlyph {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            width: 0,
            height: 0,
            offset_x: 0,
            offset_y: 0,
            pitch: 0,
            pixels: Vec::new(),
        }
    }
}

struct FaceWrapper {
    face: Option<freetype::Face>,
}

impl Drop for FaceWrapper {
    fn drop(&mut self) {
        // FreeType requires face destruction sharing an FT_Library to be serialized.
        let _guard = ft_global_lock();
        self.face.take();
    }
}

/// Thread-local handle to a FreeType face with lazy outline rasterization.
#[derive(Clone)]
pub struct Font {
    inner: Rc<RefCell<FaceWrapper>>,
}

/// Converts font typography points to nominal pixels at standard 96 DPI (72 pt == 96 px).
#[inline]
#[must_use]
#[doc(hidden)]
pub fn points_to_pixels(points: f32) -> f32 {
    points * (96.0 / 72.0)
}

impl Font {
    /// Opens a font face from a file path in sub-milliseconds without parsing glyph outlines.
    ///
    /// # Errors
    /// Returns [`std::io::Error`] if the file cannot be opened or FreeType fails to parse headers.
    pub fn from_file(path: &Path, collection_index: u32) -> io::Result<Self> {
        let lib = ft_library()?;
        let face = {
            let _guard = ft_global_lock();
            lib.new_face(path, collection_index as isize)
        }
        .map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("FreeType new_face failed: {e:?}"),
            )
        })?;
        Ok(Self {
            inner: Rc::new(RefCell::new(FaceWrapper { face: Some(face) })),
        })
    }

    /// Opens a font face from in-memory font bytes in sub-milliseconds without parsing glyph outlines.
    ///
    /// # Errors
    /// Returns [`std::io::Error`] if FreeType fails to parse font tables from the bytes.
    pub fn from_bytes(bytes: &[u8], collection_index: u32) -> io::Result<Self> {
        let lib = ft_library()?;
        let face = {
            let _guard = ft_global_lock();
            lib.new_memory_face(bytes.to_vec(), collection_index as isize)
        }
        .map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("FreeType new_memory_face failed: {e:?}"),
            )
        })?;
        Ok(Self {
            inner: Rc::new(RefCell::new(FaceWrapper { face: Some(face) })),
        })
    }

    /// Looks up the glyph index in the font's character map (`cmap`).
    ///
    /// Returns `0` if the character is not mapped (.notdef).
    #[must_use]
    pub fn lookup_glyph_index(&self, c: char) -> u16 {
        let guard = self.inner.borrow();
        guard
            .face
            .as_ref()
            .and_then(|f| f.get_char_index(c as usize))
            .unwrap_or(0) as u16
    }

    /// Sets the active character size on the face using 26.6 fractional units at standard 96 DPI.
    /// In accordance with industry standards (WezTerm, Foot, Fontconfig), font_size represents
    /// typography points (e.g. 9.0 pt = 12.0 px at 96 DPI).
    fn set_font_size(face: &freetype::Face, font_size: f32) {
        let size_in_26_6 = (font_size * 64.0).round().max(64.0) as isize;
        let _ = face.set_char_size(0, size_in_26_6, 96, 96);
    }

    /// Retrieves line height and baseline ascent metrics.
    #[must_use]
    pub fn horizontal_line_metrics(&self, font_size: f32) -> Option<LineMetrics> {
        let guard = self.inner.borrow();
        let face = guard.face.as_ref()?;
        Self::set_font_size(face, font_size);
        let metrics = face.size_metrics()?;
        let ascent = (metrics.ascender as f32) / 64.0;
        let descent = (metrics.descender as f32) / 64.0;
        let height = (metrics.height as f32) / 64.0;
        let new_line_size = if height > 0.0 {
            height
        } else {
            (ascent - descent).max(1.0)
        };
        Some(LineMetrics {
            new_line_size,
            ascent,
        })
    }

    /// Retrieves advance width for a character.
    #[must_use]
    pub fn glyph_advance_width(&self, c: char, font_size: f32) -> f32 {
        let guard = self.inner.borrow();
        if let Some(face) = &guard.face {
            Self::set_font_size(face, font_size);
            let glyph_idx = face.get_char_index(c as usize).unwrap_or(0);
            if face.load_glyph(glyph_idx, LoadFlag::NO_HINTING).is_ok() {
                let advance = face.glyph().advance().x;
                if advance > 0 {
                    return (advance as f32) / 64.0;
                }
            }
        }
        let nominal_px = points_to_pixels(font_size);
        (nominal_px * 0.6).round().max(1.0)
    }

    /// Computes cell dimensions (cell_width, cell_height, ascent) for the active font at the given size.
    #[must_use]
    pub fn compute_cell_metrics(&self, font_size: f32) -> crate::font::CellMetrics {
        let cell_width = self.glyph_advance_width('0', font_size).round().max(1.0) as u32;
        let nominal_px = points_to_pixels(font_size);
        let (cell_height, ascent) = self
            .horizontal_line_metrics(font_size)
            .map(|line| {
                (
                    line.new_line_size.round().max(1.0) as u32,
                    line.ascent.round() as i32,
                )
            })
            .unwrap_or((
                nominal_px.round().max(1.0) as u32,
                nominal_px.round() as i32,
            ));

        crate::font::CellMetrics {
            cell_width,
            cell_height,
            ascent,
        }
    }

    /// Rasterizes an indexed glyph on-demand into an alpha mask or LCD subpixel bitmap, normalizing pitch to top-to-bottom.
    #[must_use]
    pub fn rasterize_indexed(
        &self,
        glyph_index: u16,
        font_size: f32,
        subpixel: bool,
        bgr: bool,
    ) -> RasterizedGlyph {
        let guard = self.inner.borrow();
        let Some(face) = &guard.face else {
            return RasterizedGlyph::empty();
        };
        Self::set_font_size(face, font_size);
        let load_flags = LoadFlag::DEFAULT | LoadFlag::TARGET_LIGHT;
        let render_mode = if subpixel {
            RenderMode::Lcd
        } else {
            RenderMode::Normal
        };
        if face.load_glyph(glyph_index as u32, load_flags).is_err() {
            return RasterizedGlyph::empty();
        }
        let slot = face.glyph();
        if slot.render_glyph(render_mode).is_err() {
            return RasterizedGlyph::empty();
        }
        let bmp = slot.bitmap();
        let raw_width = bmp.width().max(0) as u32;
        let height = bmp.rows().max(0) as u32;
        if raw_width == 0 || height == 0 {
            return RasterizedGlyph::empty();
        }
        let offset_x = slot.bitmap_left();
        let offset_y = slot.bitmap_top() - height as i32;
        let pitch = bmp.pitch();
        let abs_pitch = pitch.unsigned_abs() as usize;
        let buffer = bmp.buffer();

        if bmp.pixel_mode() == Ok(freetype::bitmap::PixelMode::Lcd) {
            // FreeType LCD bitmaps produce 3 horizontal subpixel coverage bytes per logical pixel.
            // Following Foot's fcft engine: raw geometric coverage is preserved directly across
            // both RGB stripes and Alpha channel, eliminating artificial sRGB over-darkening and bloated strokes.
            let logical_width = (raw_width / 3).max(1);
            let mut pixels = vec![0u8; (logical_width * height * 4) as usize];
            for y in 0..height {
                let src_y = if pitch < 0 {
                    (height - 1 - y) as usize
                } else {
                    y as usize
                };
                let src_row = src_y * abs_pitch;
                let dst_row = (y as usize) * (logical_width as usize) * 4;
                for x in 0..logical_width {
                    let src_idx = src_row + (x as usize) * 3;
                    if src_idx + 2 < buffer.len() {
                        let raw_r = buffer[src_idx];
                        let raw_g = buffer[src_idx + 1];
                        let raw_b = buffer[src_idx + 2];
                        let linear_alpha = raw_r.max(raw_g).max(raw_b);
                        let (r_out, b_out) = if bgr { (raw_b, raw_r) } else { (raw_r, raw_b) };
                        let dst_idx = dst_row + (x as usize) * 4;
                        pixels[dst_idx..dst_idx + 4].copy_from_slice(&[
                            r_out,
                            raw_g,
                            b_out,
                            linear_alpha,
                        ]);
                    }
                }
            }
            RasterizedGlyph {
                width: logical_width,
                height,
                offset_x,
                offset_y,
                pitch: (logical_width * 4) as usize,
                pixels,
            }
        } else if bmp.pixel_mode() == Ok(freetype::bitmap::PixelMode::Mono) {
            // FreeType 1-bit monochrome bitmaps pack 8 pixels per byte, MSB-first.
            let row_pixels = raw_width as usize;
            let mut pixels = vec![0u8; (raw_width * height * 4) as usize];
            for y in 0..height {
                let src_y = if pitch < 0 {
                    (height - 1 - y) as usize
                } else {
                    y as usize
                };
                let src_row = src_y * abs_pitch;
                let dst_offset = (y as usize) * row_pixels * 4;
                for x in 0..row_pixels {
                    let byte_idx = src_row + (x / 8);
                    let bit_val = if byte_idx < buffer.len() {
                        (buffer[byte_idx] & (0x80 >> (x % 8))) != 0
                    } else {
                        false
                    };
                    let v = if bit_val { 255 } else { 0 };
                    let dst_idx = dst_offset + x * 4;
                    pixels[dst_idx..dst_idx + 4].copy_from_slice(&[v, v, v, v]);
                }
            }
            RasterizedGlyph {
                width: raw_width,
                height,
                offset_x,
                offset_y,
                pitch: row_pixels * 4,
                pixels,
            }
        } else {
            let row_pixels = raw_width as usize;
            let mut pixels = vec![0u8; (raw_width * height * 4) as usize];
            for y in 0..height {
                let src_y = if pitch < 0 {
                    (height - 1 - y) as usize
                } else {
                    y as usize
                };
                let src_offset = src_y * abs_pitch;
                let dst_offset = (y as usize) * row_pixels * 4;
                for x in 0..row_pixels {
                    if src_offset + x < buffer.len() {
                        let linear_gray = buffer[src_offset + x];
                        let dst_idx = dst_offset + x * 4;
                        pixels[dst_idx..dst_idx + 4].copy_from_slice(&[
                            linear_gray,
                            linear_gray,
                            linear_gray,
                            linear_gray,
                        ]);
                    }
                }
            }
            RasterizedGlyph {
                width: raw_width,
                height,
                offset_x,
                offset_y,
                pitch: row_pixels * 4,
                pixels,
            }
        }
    }
}

// --- Fallback ---

pub(crate) const MAX_FALLBACK_FACES: usize = 64;
#[doc(hidden)]
pub const MAX_RESOLVED_CACHE: usize = 4096;

#[doc(hidden)]
pub struct FallbackFace {
    pub path: PathBuf,
    pub index: u32,
    pub style: u8,
    pub font: Font,
}

#[derive(Default)]
#[doc(hidden)]
pub struct FallbackCache {
    #[doc(hidden)]
    pub faces: Vec<FallbackFace>,
    #[doc(hidden)]
    pub resolved: HashMap<(char, u8), Option<(u16, u16)>>,
    #[doc(hidden)]
    pub resolved_order: VecDeque<(char, u8)>,
}

impl FallbackCache {
    pub(crate) fn insert_face(&mut self, face: FallbackFace) -> usize {
        if let Some(pos) = self
            .faces
            .iter()
            .position(|f| f.path == face.path && f.index == face.index && f.style == face.style)
        {
            return pos;
        }
        if self.faces.len() >= MAX_FALLBACK_FACES {
            self.faces.remove(0);
            self.resolved.clear();
            self.resolved_order.clear();
        }
        self.faces.push(face);
        self.faces.len() - 1
    }

    #[doc(hidden)]
    pub fn resolve(&mut self, c: char, style: u8, preferred_family: &str) -> Option<(u16, u16)> {
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
        // Step 1: Check if an already loaded face matching the exact requested style covers `c`
        for (pos, face) in self.faces.iter().enumerate() {
            if face.style == style {
                let glyph = face.font.lookup_glyph_index(c);
                if glyph != 0 {
                    return Some((pos as u16, glyph));
                }
            }
        }

        // Step 2: Attempt Fontconfig discovery for the specific requested style (bold/italic)
        let bold = (style & 1) != 0;
        let italic = (style & 2) != 0;
        if let Some(fc) = fontconfig()
            && let Some(candidates) =
                query_fontconfig_candidates(fc, preferred_family, bold, italic, c)
        {
            for (path, index) in candidates {
                let position = match self.faces.iter().position(|face| {
                    face.path == path && face.index == index && face.style == style
                }) {
                    Some(position) => position,
                    None => {
                        let Ok(font) = load_font_file(&path, index) else {
                            continue;
                        };
                        self.insert_face(FallbackFace {
                            path,
                            index,
                            style,
                            font,
                        })
                    }
                };

                let glyph = self.faces[position].font.lookup_glyph_index(c);
                if glyph != 0 {
                    return Some((position as u16, glyph));
                }
            }
        }

        // Step 3: Graceful fallback: If no style-specific face was discovered (e.g. the font has no
        // bold/italic variant installed), reuse an already loaded face of any other style covering `c`
        for (pos, face) in self.faces.iter().enumerate() {
            let glyph = face.font.lookup_glyph_index(c);
            if glyph != 0 {
                return Some((pos as u16, glyph));
            }
        }

        None
    }
}

#[doc(hidden)]
pub fn fontconfig() -> Option<&'static Fontconfig> {
    static FC: OnceLock<Option<Fontconfig>> = OnceLock::new();
    FC.get_or_init(Fontconfig::new).as_ref()
}

#[doc(hidden)]
pub fn match_family(
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

pub(crate) fn query_fontconfig_candidates(
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

    if let Ok(matched) = pattern.font_match()
        && let Ok(filename) = matched.filename()
        && let Ok(face_index) = matched.face_index()
        && let Ok(index) = u32::try_from(face_index)
    {
        return Some(vec![(PathBuf::from(filename), index)]);
    }

    if let Ok(font_set) = pattern.sort_fonts(fontconfig::UnicodeCoverage::Trim) {
        let mut candidates = Vec::new();
        for p in font_set.iter().take(2) {
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

    None
}

#[doc(hidden)]
pub fn load_font_bytes(bytes: &[u8], collection_index: u32) -> io::Result<Font> {
    Font::from_bytes(bytes, collection_index)
}

#[doc(hidden)]
pub fn load_font_file(path: &Path, collection_index: u32) -> io::Result<Font> {
    Font::from_file(path, collection_index)
}

// --- Cache ---

const CACHE_MAGIC: &[u8; 8] = b"FTTYFONT";
const CACHE_VERSION: u32 = 7;

/// Maximum number of font family names supported in a single cache entry.
pub const MAX_CACHED_FAMILIES: usize = 64;
/// Maximum number of fallback font entries supported in a single cache entry.
pub const MAX_CACHED_FALLBACKS: usize = 64;

/// File metadata tracking a resolved font on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedFontFile {
    pub path: PathBuf,
    pub index: u32,
    pub mtime_secs: i64,
    pub mtime_nanos: u32,
    pub file_size: u64,
}

impl CachedFontFile {
    /// Inspects the file at `path` and builds a cache record if readable.
    #[must_use]
    pub fn from_path_and_index(path: PathBuf, index: u32) -> Option<Self> {
        let meta = std::fs::metadata(&path).ok()?;
        let (mtime_secs, mtime_nanos) = match meta.modified() {
            Ok(time) => match time.duration_since(SystemTime::UNIX_EPOCH) {
                Ok(dur) => (dur.as_secs() as i64, dur.subsec_nanos()),
                Err(err) => {
                    let dur = err.duration();
                    (-(dur.as_secs() as i64), dur.subsec_nanos())
                }
            },
            Err(_) => (0, 0),
        };
        Some(Self {
            path,
            index,
            mtime_secs,
            mtime_nanos,
            file_size: meta.len(),
        })
    }

    /// Verifies that the font file still exists on disk with matching size and modification time.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return false;
        };
        if meta.len() != self.file_size {
            return false;
        }
        let (mtime_secs, mtime_nanos) = match meta.modified() {
            Ok(time) => match time.duration_since(SystemTime::UNIX_EPOCH) {
                Ok(dur) => (dur.as_secs() as i64, dur.subsec_nanos()),
                Err(err) => {
                    let dur = err.duration();
                    (-(dur.as_secs() as i64), dur.subsec_nanos())
                }
            },
            Err(_) => (0, 0),
        };
        mtime_secs == self.mtime_secs && mtime_nanos == self.mtime_nanos
    }
}

/// Serialized font cache holding resolved primary font, metrics, and fallback paths.
#[derive(Debug, Clone, PartialEq)]
pub struct FontCacheData {
    pub families: Vec<String>,
    pub font_size: f32,
    pub subpixel: bool,
    pub dirs_fingerprint: u64,
    pub primary: CachedFontFile,
    pub metrics: CellMetrics,
    pub fallbacks: Vec<Option<CachedFontFile>>,
}

/// Generates a fingerprint of font system directories to detect font installations or config changes.
#[must_use]
pub fn font_system_fingerprint() -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let dirs = [
        Path::new("/etc/fonts"),
        Path::new("/usr/share/fonts"),
        Path::new("/usr/local/share/fonts"),
    ];
    for d in &dirs {
        if let Ok(meta) = std::fs::metadata(d)
            && let Ok(mtime) = meta.modified()
            && let Ok(dur) = mtime.duration_since(SystemTime::UNIX_EPOCH)
        {
            hasher.write_u64(dur.as_secs());
            hasher.write_u32(dur.subsec_nanos());
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home_path = PathBuf::from(home);
        for sub in [".config/fontconfig", ".local/share/fonts", ".fonts"] {
            let path = home_path.join(sub);
            if let Ok(meta) = std::fs::metadata(&path)
                && let Ok(mtime) = meta.modified()
                && let Ok(dur) = mtime.duration_since(SystemTime::UNIX_EPOCH)
            {
                hasher.write_u64(dur.as_secs());
                hasher.write_u32(dur.subsec_nanos());
            }
        }
    }
    hasher.finish()
}

/// Locates the persistent font cache file path in `$XDG_CACHE_HOME/ftty/font_cache.bin`.
#[must_use]
pub fn cache_file_path() -> Option<PathBuf> {
    let cache_dir = if let Some(val) = std::env::var_os("XDG_CACHE_HOME") {
        if !val.is_empty() {
            PathBuf::from(val)
        } else {
            dirs_cache_dir()?
        }
    } else {
        dirs_cache_dir()?
    };
    Some(cache_dir.join("ftty").join("font_cache.bin"))
}

fn dirs_cache_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join(".cache"))
}

/// Attempts to load and validate cached font metadata matching requested configuration.
#[must_use]
pub fn try_load_cache(
    families: &[String],
    font_size: f32,
    subpixel: bool,
) -> Option<FontCacheData> {
    let cache_path = cache_file_path()?;
    let bytes = std::fs::read(cache_path).ok()?;
    let data = deserialize_cache(&bytes)?;

    if data.families != families
        || (data.font_size - font_size).abs() > f32::EPSILON
        || data.subpixel != subpixel
    {
        return None;
    }

    if data.dirs_fingerprint != font_system_fingerprint() {
        return None;
    }

    if !data.primary.is_valid() {
        return None;
    }
    for fb in data.fallbacks.iter().flatten() {
        if !fb.is_valid() {
            return None;
        }
    }

    Some(data)
}

/// Atomically persists resolved font metadata and cell metrics to cache file.
pub fn save_cache(
    families: &[String],
    font_size: f32,
    subpixel: bool,
    primary_path: &Path,
    primary_index: u32,
    metrics: CellMetrics,
    fallback_entries: &[Option<(PathBuf, u32)>],
) {
    if families.len() > MAX_CACHED_FAMILIES || fallback_entries.len() > MAX_CACHED_FALLBACKS {
        return;
    }

    let Some(cache_path) = cache_file_path() else {
        return;
    };
    let Some(parent) = cache_path.parent() else {
        return;
    };
    let _ = std::fs::create_dir_all(parent);

    let Some(primary) =
        CachedFontFile::from_path_and_index(primary_path.to_path_buf(), primary_index)
    else {
        return;
    };
    let mut fallbacks = Vec::with_capacity(fallback_entries.len());
    for entry in fallback_entries {
        match entry {
            Some((path, index)) => {
                fallbacks.push(CachedFontFile::from_path_and_index(path.clone(), *index));
            }
            None => {
                fallbacks.push(None);
            }
        }
    }

    let payload = serialize_cache(&FontCacheData {
        families: families.to_vec(),
        font_size,
        subpixel,
        dirs_fingerprint: font_system_fingerprint(),
        primary,
        metrics,
        fallbacks,
    });

    let tmp_path = parent.join(format!("font_cache.bin.tmp.{}", std::process::id()));
    if std::fs::write(&tmp_path, payload).is_ok() {
        let _ = std::fs::rename(&tmp_path, &cache_path);
    }
}

/// Serializes font cache data to compact binary buffer.
#[must_use]
pub fn serialize_cache(data: &FontCacheData) -> Vec<u8> {
    let mut buf = Vec::with_capacity(512);
    buf.extend_from_slice(CACHE_MAGIC);
    buf.extend_from_slice(&CACHE_VERSION.to_le_bytes());

    buf.extend_from_slice(&(data.families.len() as u32).to_le_bytes());
    for family in &data.families {
        let bytes = family.as_bytes();
        buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(bytes);
    }

    buf.extend_from_slice(&data.font_size.to_bits().to_le_bytes());
    buf.push(u8::from(data.subpixel));
    buf.extend_from_slice(&data.dirs_fingerprint.to_le_bytes());

    write_cached_file(&mut buf, &data.primary);

    buf.extend_from_slice(&data.metrics.cell_width.to_le_bytes());
    buf.extend_from_slice(&data.metrics.cell_height.to_le_bytes());
    buf.extend_from_slice(&data.metrics.ascent.to_le_bytes());

    buf.extend_from_slice(&(data.fallbacks.len() as u32).to_le_bytes());
    for fb in &data.fallbacks {
        match fb {
            Some(file) => {
                buf.push(1);
                write_cached_file(&mut buf, file);
            }
            None => {
                buf.push(0);
            }
        }
    }

    buf
}

fn write_cached_file(buf: &mut Vec<u8>, file: &CachedFontFile) {
    let raw_bytes = file.path.as_os_str().as_bytes();
    buf.extend_from_slice(&(raw_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(raw_bytes);
    buf.extend_from_slice(&file.index.to_le_bytes());
    buf.extend_from_slice(&file.mtime_secs.to_le_bytes());
    buf.extend_from_slice(&file.mtime_nanos.to_le_bytes());
    buf.extend_from_slice(&file.file_size.to_le_bytes());
}

/// Deserializes font cache data from compact binary buffer.
#[must_use]
pub fn deserialize_cache(bytes: &[u8]) -> Option<FontCacheData> {
    if bytes.len() < 8 + 4 {
        return None;
    }
    if &bytes[..8] != CACHE_MAGIC {
        return None;
    }
    let mut cursor = Cursor::new(&bytes[8..]);
    let version = read_u32(&mut cursor).ok()?;
    if version != CACHE_VERSION {
        return None;
    }

    let num_families = read_u32(&mut cursor).ok()? as usize;
    if num_families > MAX_CACHED_FAMILIES {
        return None;
    }
    let mut families = Vec::with_capacity(num_families);
    for _ in 0..num_families {
        let str_len = read_u32(&mut cursor).ok()? as usize;
        if str_len > 1024 {
            return None;
        }
        let mut s_bytes = vec![0u8; str_len];
        cursor.read_exact(&mut s_bytes).ok()?;
        let family = String::from_utf8(s_bytes).ok()?;
        families.push(family);
    }

    let font_size = f32::from_bits(read_u32(&mut cursor).ok()?);
    if !font_size.is_finite() || font_size < MIN_FONT_SIZE || font_size > MAX_FONT_SIZE {
        return None;
    }

    let mut subpixel_byte = [0u8; 1];
    cursor.read_exact(&mut subpixel_byte).ok()?;
    let subpixel = subpixel_byte[0] != 0;

    let dirs_fingerprint = read_u64(&mut cursor).ok()?;

    let primary = read_cached_file(&mut cursor).ok()?;

    let cell_width = read_u32(&mut cursor).ok()?;
    let cell_height = read_u32(&mut cursor).ok()?;
    let ascent = read_i32(&mut cursor).ok()?;

    if !(1..=256).contains(&cell_width) || !(1..=256).contains(&cell_height) {
        return None;
    }
    if ascent < -(cell_height as i32) || ascent > (cell_height as i32 * 2) {
        return None;
    }

    let metrics = CellMetrics {
        cell_width,
        cell_height,
        ascent,
    };

    let num_fallbacks = read_u32(&mut cursor).ok()? as usize;
    if num_fallbacks > MAX_CACHED_FALLBACKS {
        return None;
    }
    let mut fallbacks = Vec::with_capacity(num_fallbacks);
    for _ in 0..num_fallbacks {
        let mut tag = [0u8; 1];
        cursor.read_exact(&mut tag).ok()?;
        if tag[0] == 1 {
            let fb = read_cached_file(&mut cursor).ok()?;
            fallbacks.push(Some(fb));
        } else {
            fallbacks.push(None);
        }
    }

    Some(FontCacheData {
        families,
        font_size,
        subpixel,
        dirs_fingerprint,
        primary,
        metrics,
        fallbacks,
    })
}

fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_i32<R: Read>(r: &mut R) -> io::Result<i32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(i32::from_le_bytes(b))
}

fn read_i64<R: Read>(r: &mut R) -> io::Result<i64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(i64::from_le_bytes(b))
}

fn read_u64<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn read_cached_file<R: Read>(r: &mut R) -> io::Result<CachedFontFile> {
    let len = read_u32(r)? as usize;
    if len > 4096 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "path too long"));
    }
    let mut path_bytes = vec![0u8; len];
    r.read_exact(&mut path_bytes)?;
    let path = PathBuf::from(std::ffi::OsString::from_vec(path_bytes));
    let index = read_u32(r)?;
    let mtime_secs = read_i64(r)?;
    let mtime_nanos = read_u32(r)?;
    let file_size = read_u64(r)?;

    Ok(CachedFontFile {
        path,
        index,
        mtime_secs,
        mtime_nanos,
        file_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_font_cache_roundtrip() {
        let original = FontCacheData {
            families: vec!["monospace".to_string(), "JoyPixels".to_string()],
            font_size: 14.5,
            subpixel: true,
            dirs_fingerprint: 0x1234_5678_9ABC_DEF0,
            primary: CachedFontFile {
                path: PathBuf::from("/usr/share/fonts/TTF/DejaVuSansMono.ttf"),
                index: 0,
                mtime_secs: 1_700_000_000,
                mtime_nanos: 123_456,
                file_size: 334_128,
            },
            metrics: CellMetrics {
                cell_width: 9,
                cell_height: 18,
                ascent: 14,
            },
            fallbacks: vec![
                None,
                Some(CachedFontFile {
                    path: PathBuf::from("/usr/share/fonts/JoyPixels.ttf"),
                    index: 1,
                    mtime_secs: 1_700_000_100,
                    mtime_nanos: 0,
                    file_size: 20_000_000,
                }),
            ],
        };

        let serialized = serialize_cache(&original);
        let deserialized = deserialize_cache(&serialized);
        assert_eq!(deserialized, Some(original));
    }

    #[test]
    fn test_corrupted_cache_handled_safely() {
        assert!(deserialize_cache(b"").is_none());
        assert!(deserialize_cache(b"INVALID_HEADER_DATA").is_none());
        assert!(deserialize_cache(b"FTTYFONT\x01\x00\x00\x00").is_none()); // Version 1 mismatch
        assert!(deserialize_cache(b"FTTYFONT\x02\x00\x00\x00").is_none()); // Version 2 mismatch
        assert!(deserialize_cache(b"FTTYFONT\x03\x00\x00\x00").is_none()); // Version 3 mismatch
        assert!(deserialize_cache(b"FTTYFONT\x04\x00\x00\x00").is_none()); // Version 4 mismatch
        assert!(deserialize_cache(b"FTTYFONT\x05\x00\x00\x00").is_none()); // Version 5 mismatch
        assert!(deserialize_cache(b"FTTYFONT\x06\x00\x00\x00").is_none()); // Version 6 mismatch
    }

    #[test]
    fn test_invalid_cell_metrics_rejected() {
        let mut data = FontCacheData {
            families: vec!["monospace".to_string()],
            font_size: 14.0,
            subpixel: false,
            dirs_fingerprint: 1,
            primary: CachedFontFile {
                path: PathBuf::from("/nonexistent.ttf"),
                index: 0,
                mtime_secs: 0,
                mtime_nanos: 0,
                file_size: 0,
            },
            metrics: CellMetrics {
                cell_width: 0, // Invalid zero width
                cell_height: 18,
                ascent: 14,
            },
            fallbacks: Vec::new(),
        };
        let bytes = serialize_cache(&data);
        assert!(deserialize_cache(&bytes).is_none());

        data.metrics.cell_width = 10;
        data.metrics.cell_height = 0; // Invalid zero height
        let bytes2 = serialize_cache(&data);
        assert!(deserialize_cache(&bytes2).is_none());
    }
}

// --- Font Manager ---

/// Cell size and baseline alignment metrics for the active font and font size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellMetrics {
    pub cell_width: u32,
    pub cell_height: u32,
    pub ascent: i32,
}

/// Stable identity of a rasterized glyph: which face supplied it and in which style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[doc(hidden)]
pub struct FaceKey {
    /// `0` is the styled primary face; `1..=num_fallbacks` is a user fallback; subsequent values select dynamic fallbacks.
    pub face: u16,
    pub glyph: u16,
    pub style: u8,
}

/// An immutable chain of font faces for a single style (Regular, Bold, Italic, or BoldItalic).
#[derive(Clone)]
#[doc(hidden)]
pub struct StyleChain {
    pub primary: Font,
    pub fallbacks: Vec<Font>,
}

/// Minimum font size in points/pixels supported by the terminal.
pub const MIN_FONT_SIZE: f32 = 6.0;
/// Maximum user-facing font size in points/pixels supported by the terminal.
pub const MAX_FONT_SIZE: f32 = 72.0;
/// Maximum physical raster font size accounting for display scale factors (up to 4x HiDPI).
pub const MAX_RASTER_FONT_SIZE: f32 = MAX_FONT_SIZE * 4.0;

pub(crate) fn style_index(flags: CellFlags) -> usize {
    usize::from(flags.contains(CellFlags::BOLD))
        | (usize::from(flags.contains(CellFlags::ITALIC)) << 1)
}

/// Loads configured font chain and on-demand fallback faces with cell metrics calculation.
pub struct FontManager {
    #[doc(hidden)]
    pub regular: StyleChain,
    regular_slots: Vec<Option<Font>>,
    chains: RefCell<[Option<StyleChain>; 4]>,
    fallbacks: RefCell<FallbackCache>,
    families: Vec<String>,
    #[doc(hidden)]
    pub font_size: f32,
    pub subpixel: bool,
    pub bgr: bool,
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
        Self::load_with_families_and_subpixel(families, font_size, true)
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
        let valid_families: Vec<String> = if families.is_empty() {
            vec!["monospace".to_string()]
        } else {
            families.to_vec()
        };

        if let Some(cached) = try_load_cache(&valid_families, font_size, subpixel)
            && let Ok(primary_regular) = load_font_file(&cached.primary.path, cached.primary.index)
        {
            let regular_slots: Vec<Option<Font>> = cached
                .fallbacks
                .iter()
                .map(|opt| {
                    opt.as_ref()
                        .and_then(|f| load_font_file(&f.path, f.index).ok())
                })
                .collect();
            let regular_fallbacks: Vec<Font> = regular_slots.iter().flatten().cloned().collect();
            let regular = StyleChain {
                primary: primary_regular,
                fallbacks: regular_fallbacks,
            };
            return Ok(Self {
                regular: regular.clone(),
                regular_slots,
                chains: RefCell::new([Some(regular), None, None, None]),
                fallbacks: RefCell::new(FallbackCache::default()),
                families: valid_families,
                font_size,
                subpixel,
                bgr: false,
                metrics: cached.metrics,
            });
        }

        let fc = fontconfig()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "fontconfig not available"))?;

        let primary_name = &valid_families[0];
        let fallback_names = &valid_families[1..];

        // 1. Load the primary Regular font (determines CellMetrics)
        let (primary_path, primary_index) = match_family(fc, primary_name, false, false)
            .or_else(|| match_family(fc, "monospace", false, false))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no monospace font found"))?;
        let primary_regular = load_font_file(&primary_path, primary_index)?;

        // 2. Compute metrics from the primary Regular font tables
        let metrics = primary_regular.compute_cell_metrics(font_size);

        // 3. User fallback regular slots maintain 1:1 index alignment with fallback_names.
        let mut fallback_entries = Vec::with_capacity(fallback_names.len());
        let regular_slots: Vec<Option<Font>> = fallback_names
            .iter()
            .map(|name| {
                let matched = match_family(fc, name, false, false);
                fallback_entries.push(matched.clone());
                matched.and_then(|(path, index)| load_font_file(&path, index).ok())
            })
            .collect();

        let regular_fallbacks: Vec<Font> = regular_slots.iter().flatten().cloned().collect();

        save_cache(
            &valid_families,
            font_size,
            subpixel,
            &primary_path,
            primary_index,
            metrics,
            &fallback_entries,
        );

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
            font_size,
            subpixel,
            bgr: false,
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
        if !new_size.is_finite() || new_size < MIN_FONT_SIZE || new_size > MAX_RASTER_FONT_SIZE {
            return false;
        }
        if (self.font_size - new_size).abs() < f32::EPSILON {
            return false;
        }
        self.font_size = new_size;
        self.metrics = self.regular.primary.compute_cell_metrics(new_size);
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

    #[doc(hidden)]
    pub fn regular(&self) -> Font {
        self.regular.primary.clone()
    }

    /// Resolves a character to the face that can actually render it.
    ///
    /// Glyph index `0` is `.notdef`, so a zero index means "no face has this glyph".
    #[doc(hidden)]
    pub fn face_key(&self, c: char, flags: CellFlags) -> FaceKey {
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
    #[doc(hidden)]
    pub fn rasterize(&self, key: FaceKey) -> RasterizedGlyph {
        if key.glyph == 0 {
            return RasterizedGlyph::empty();
        }
        let style = key.style.min(3);
        self.ensure_chain(style);
        let binding = self.chains.borrow();
        let chain = binding[style as usize].as_ref().unwrap_or(&self.regular);
        let num_configured = (1 + chain.fallbacks.len()) as u16;

        if key.face == 0 {
            return chain.primary.rasterize_indexed(
                key.glyph,
                self.font_size,
                self.subpixel,
                self.bgr,
            );
        }
        if key.face < num_configured {
            let fallback_idx = (key.face - 1) as usize;
            return chain.fallbacks[fallback_idx].rasterize_indexed(
                key.glyph,
                self.font_size,
                self.subpixel,
                self.bgr,
            );
        }

        let fallback_idx = (key.face - num_configured) as usize;
        drop(binding);
        let fallbacks = self.fallbacks.borrow();
        match fallbacks.faces.get(fallback_idx) {
            Some(face) => {
                face.font
                    .rasterize_indexed(key.glyph, self.font_size, self.subpixel, self.bgr)
            }
            None => {
                self.regular
                    .primary
                    .rasterize_indexed(0, self.font_size, self.subpixel, self.bgr)
            }
        }
    }
}

pub mod atlas {
    pub use super::{CachedGlyph, GlyphAtlas, INITIAL_ATLAS_SIZE, MAX_ATLAS_SIZE, Shelf};
}

pub mod fallback {
    pub use super::{
        FallbackCache, MAX_RESOLVED_CACHE, fontconfig, load_font_bytes, load_font_file,
        match_family,
    };
}

pub mod face {
    pub use super::{Font, LineMetrics, RasterizedGlyph, points_to_pixels};
}
