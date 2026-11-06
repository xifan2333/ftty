//! FreeType face loading, glyph rasterization, and text-run measurement.

use std::cell::RefCell;
use std::io;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Mutex, MutexGuard, OnceLock};

use freetype::RenderMode;
use freetype::face::LoadFlag;

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
