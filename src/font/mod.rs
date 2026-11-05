//! Font loading, multi-family fallback chaining, and dynamic text metrics calculation.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Mutex, MutexGuard, OnceLock};

use fontconfig::{CharSet, Fontconfig, Pattern};
use freetype::RenderMode;
use freetype::face::LoadFlag;

use crate::grid::CellFlags;

pub mod atlas;
pub mod cache;

pub use atlas::{CachedGlyph, GlyphAtlas, INITIAL_ATLAS_SIZE, MAX_ATLAS_SIZE, Shelf};
pub use cache::{
    CachedFontFile, FontCacheData, MAX_CACHED_FALLBACKS, MAX_CACHED_FAMILIES, cache_file_path,
    deserialize_cache, font_system_fingerprint, save_cache, serialize_cache, try_load_cache,
};

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
    pub colored: bool,
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
            colored: false,
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
        if face.set_char_size(0, size_in_26_6, 96, 96).is_err() && face.has_fixed_sizes() {
            let pixel_sz = points_to_pixels(font_size).round().max(1.0) as u32;
            if face.set_pixel_sizes(pixel_sz, pixel_sz).is_err() {
                let _ = face.select_size(0);
            }
        }
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
            if face
                .load_glyph(glyph_idx, LoadFlag::NO_HINTING | LoadFlag::COLOR)
                .is_ok()
            {
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
        let load_flags = LoadFlag::DEFAULT | LoadFlag::TARGET_LIGHT | LoadFlag::COLOR;
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
                colored: false,
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
                colored: false,
            }
        } else if bmp.pixel_mode() == Ok(freetype::bitmap::PixelMode::Bgra) {
            let nominal_px = points_to_pixels(font_size);
            let max_height = (nominal_px * 1.15).round().max(1.0) as u32;
            let max_width = (nominal_px * 2.0).round().max(1.0) as u32;
            let cell_ascent = (nominal_px * 0.9).round() as i32;

            let (final_w, final_h, pixels) = if raw_width > max_width || height > max_height {
                let scale =
                    (max_width as f32 / raw_width as f32).min(max_height as f32 / height as f32);
                let target_w = ((raw_width as f32 * scale).round() as u32).clamp(1, max_width);
                let target_h = ((height as f32 * scale).round() as u32).clamp(1, max_height);
                let downsampled = downsample_bgra_to_rgba(
                    buffer,
                    raw_width,
                    height,
                    abs_pitch,
                    pitch < 0,
                    target_w,
                    target_h,
                );
                (target_w, target_h, downsampled)
            } else {
                (
                    raw_width,
                    height,
                    bgra_to_rgba(buffer, raw_width, height, abs_pitch, pitch < 0),
                )
            };

            let pad_x = (max_width.saturating_sub(final_w) / 2) as i32;
            let pad_y = (max_height.saturating_sub(final_h) / 2) as i32;
            let off_x = pad_x;
            let off_y = cell_ascent - final_h as i32 - pad_y;

            RasterizedGlyph {
                width: final_w,
                height: final_h,
                offset_x: off_x,
                offset_y: off_y,
                pitch: (final_w * 4) as usize,
                pixels,
                colored: true,
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
                colored: false,
            }
        }
    }
}

fn downsample_bgra_to_rgba(
    src: &[u8],
    src_width: u32,
    src_height: u32,
    abs_pitch: usize,
    pitch_negative: bool,
    dst_width: u32,
    dst_height: u32,
) -> Vec<u8> {
    let mut dst = vec![0u8; (dst_width * dst_height * 4) as usize];
    let x_scale = src_width as f64 / dst_width as f64;
    let y_scale = src_height as f64 / dst_height as f64;

    for dy in 0..dst_height {
        let sy_start = ((dy as f64) * y_scale).floor() as u32;
        let sy_end = (((dy + 1) as f64) * y_scale).ceil().min(src_height as f64) as u32;
        let dst_row_offset = (dy * dst_width * 4) as usize;

        for dx in 0..dst_width {
            let sx_start = ((dx as f64) * x_scale).floor() as u32;
            let sx_end = (((dx + 1) as f64) * x_scale).ceil().min(src_width as f64) as u32;

            let mut sum_b = 0u64;
            let mut sum_g = 0u64;
            let mut sum_r = 0u64;
            let mut sum_a = 0u64;
            let mut count = 0u64;

            for sy in sy_start..sy_end {
                let actual_sy = if pitch_negative {
                    (src_height - 1 - sy) as usize
                } else {
                    sy as usize
                };
                let row_offset = actual_sy * abs_pitch;
                for sx in sx_start..sx_end {
                    let px_offset = row_offset + (sx as usize) * 4;
                    if px_offset + 3 < src.len() {
                        let b = src[px_offset] as u64;
                        let g = src[px_offset + 1] as u64;
                        let r = src[px_offset + 2] as u64;
                        let a = src[px_offset + 3] as u64;
                        sum_b += b;
                        sum_g += g;
                        sum_r += r;
                        sum_a += a;
                        count += 1;
                    }
                }
            }

            let dst_offset = dst_row_offset + (dx * 4) as usize;
            if let Some(avg_a_val) = sum_a.checked_div(count) {
                let avg_a = avg_a_val as u8;
                if avg_a == 0 {
                    dst[dst_offset..dst_offset + 4].copy_from_slice(&[0, 0, 0, 0]);
                } else {
                    let avg_b = (sum_b.checked_div(count).unwrap_or(0)) as u32;
                    let avg_g = (sum_g.checked_div(count).unwrap_or(0)) as u32;
                    let avg_r = (sum_r.checked_div(count).unwrap_or(0)) as u32;
                    let a_u32 = avg_a as u32;
                    let r = ((avg_r * 255 + a_u32 / 2) / a_u32).min(255) as u8;
                    let g = ((avg_g * 255 + a_u32 / 2) / a_u32).min(255) as u8;
                    let b = ((avg_b * 255 + a_u32 / 2) / a_u32).min(255) as u8;
                    dst[dst_offset..dst_offset + 4].copy_from_slice(&[r, g, b, avg_a]);
                }
            }
        }
    }
    dst
}

fn bgra_to_rgba(
    src: &[u8],
    width: u32,
    height: u32,
    abs_pitch: usize,
    pitch_negative: bool,
) -> Vec<u8> {
    let mut dst = vec![0u8; (width * height * 4) as usize];
    for y in 0..height {
        let actual_y = if pitch_negative {
            (height - 1 - y) as usize
        } else {
            y as usize
        };
        let src_row = actual_y * abs_pitch;
        let dst_row = (y as usize) * (width as usize) * 4;
        for x in 0..width {
            let src_idx = src_row + (x as usize) * 4;
            let dst_idx = dst_row + (x as usize) * 4;
            if src_idx + 3 < src.len() {
                let b = src[src_idx] as u32;
                let g = src[src_idx + 1] as u32;
                let r = src[src_idx + 2] as u32;
                let a = src[src_idx + 3] as u32;
                let dst_px = match (r * 255 + a / 2).checked_div(a) {
                    Some(r_scaled) => {
                        let g_scaled = (g * 255 + a / 2) / a;
                        let b_scaled = (b * 255 + a / 2) / a;
                        [
                            r_scaled.min(255) as u8,
                            g_scaled.min(255) as u8,
                            b_scaled.min(255) as u8,
                            a as u8,
                        ]
                    }
                    None => [0, 0, 0, 0],
                };
                dst[dst_idx..dst_idx + 4].copy_from_slice(&dst_px);
            }
        }
    }
    dst
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

pub mod fallback {
    pub use super::{
        FallbackCache, MAX_RESOLVED_CACHE, fontconfig, load_font_bytes, load_font_file,
        match_family,
    };
}

pub mod face {
    pub use super::{Font, LineMetrics, RasterizedGlyph, points_to_pixels};
}
