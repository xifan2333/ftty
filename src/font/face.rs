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

        match bmp.pixel_mode() {
            Ok(freetype::bitmap::PixelMode::Lcd) => unpack_lcd_glyph(&bmp, bgr, offset_x, offset_y),
            Ok(freetype::bitmap::PixelMode::Mono) => unpack_mono_glyph(&bmp, offset_x, offset_y),
            Ok(freetype::bitmap::PixelMode::Bgra) => unpack_bgra_glyph(&bmp, font_size),
            _ => unpack_gray_glyph(&bmp, offset_x, offset_y),
        }
    }
}

struct SourceBitmap<'a> {
    data: &'a [u8],
    width: u32,
    height: u32,
    pitch: i32,
    abs_pitch: usize,
}

impl<'a> SourceBitmap<'a> {
    #[inline]
    fn row(&self, y: u32) -> &[u8] {
        let src_y = if self.pitch < 0 {
            (self.height - 1 - y) as usize
        } else {
            y as usize
        };
        let start = src_y * self.abs_pitch;
        if start < self.data.len() {
            &self.data[start..]
        } else {
            &[]
        }
    }
}

fn unpack_lcd_row(src_row: &[u8], dst_row: &mut [[u8; 4]], bgr: bool) {
    for (src_chunk, dst_chunk) in src_row.as_chunks::<3>().0.iter().zip(dst_row.iter_mut()) {
        let (raw_r, raw_g, raw_b) = (src_chunk[0], src_chunk[1], src_chunk[2]);
        let linear_alpha = raw_r.max(raw_g).max(raw_b);
        let (r_out, b_out) = if bgr { (raw_b, raw_r) } else { (raw_r, raw_b) };
        *dst_chunk = [r_out, raw_g, b_out, linear_alpha];
    }
}

fn unpack_lcd_glyph(
    bmp: &freetype::Bitmap,
    bgr: bool,
    offset_x: i32,
    offset_y: i32,
) -> RasterizedGlyph {
    // FreeType LCD bitmaps produce 3 horizontal subpixel coverage bytes per logical pixel.
    // Following Foot's fcft engine: raw geometric coverage is preserved directly across
    // both RGB stripes and Alpha channel, eliminating artificial sRGB over-darkening and bloated strokes.
    let raw_width = bmp.width().max(0) as u32;
    let height = bmp.rows().max(0) as u32;
    let pitch = bmp.pitch();
    let abs_pitch = pitch.unsigned_abs() as usize;
    let buffer = bmp.buffer();

    let logical_width = (raw_width / 3).max(1);
    let mut pixels = vec![0u8; (logical_width * height * 4) as usize];
    let src = SourceBitmap {
        data: buffer,
        width: raw_width,
        height,
        pitch,
        abs_pitch,
    };
    for (y, dst_row) in pixels
        .as_chunks_mut::<4>()
        .0
        .chunks_exact_mut(logical_width as usize)
        .enumerate()
    {
        let src_row = src.row(y as u32);
        unpack_lcd_row(src_row, dst_row, bgr);
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
}

fn unpack_mono_row(src_row: &[u8], dst_row: &mut [[u8; 4]]) {
    for (x, dst_chunk) in dst_row.iter_mut().enumerate() {
        let byte_idx = x / 8;
        let bit_val = byte_idx < src_row.len() && (src_row[byte_idx] & (0x80 >> (x % 8))) != 0;
        let v = if bit_val { 255 } else { 0 };
        *dst_chunk = [v, v, v, v];
    }
}

fn unpack_mono_glyph(bmp: &freetype::Bitmap, offset_x: i32, offset_y: i32) -> RasterizedGlyph {
    // FreeType 1-bit monochrome bitmaps pack 8 pixels per byte, MSB-first.
    let raw_width = bmp.width().max(0) as u32;
    let height = bmp.rows().max(0) as u32;
    let pitch = bmp.pitch();
    let abs_pitch = pitch.unsigned_abs() as usize;
    let buffer = bmp.buffer();

    let row_pixels = raw_width as usize;
    let mut pixels = vec![0u8; (raw_width * height * 4) as usize];
    let src = SourceBitmap {
        data: buffer,
        width: raw_width,
        height,
        pitch,
        abs_pitch,
    };
    for (y, dst_row) in pixels
        .as_chunks_mut::<4>()
        .0
        .chunks_exact_mut(row_pixels)
        .enumerate()
    {
        let src_row = src.row(y as u32);
        unpack_mono_row(src_row, dst_row);
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

fn unpack_bgra_glyph(bmp: &freetype::Bitmap, font_size: f32) -> RasterizedGlyph {
    let raw_width = bmp.width().max(0) as u32;
    let height = bmp.rows().max(0) as u32;
    let pitch = bmp.pitch();
    let abs_pitch = pitch.unsigned_abs() as usize;
    let buffer = bmp.buffer();

    let nominal_px = points_to_pixels(font_size);
    let max_height = (nominal_px * 1.15).round().max(1.0) as u32;
    let max_width = (nominal_px * 2.0).round().max(1.0) as u32;
    let cell_ascent = (nominal_px * 0.9).round() as i32;

    let (final_w, final_h, pixels) = if raw_width > max_width || height > max_height {
        let scale = (max_width as f32 / raw_width as f32).min(max_height as f32 / height as f32);
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
}

fn unpack_gray_row(src_row: &[u8], dst_row: &mut [[u8; 4]]) {
    for (src_byte, dst_chunk) in src_row.iter().zip(dst_row.iter_mut()) {
        let v = *src_byte;
        *dst_chunk = [v, v, v, v];
    }
}

fn unpack_gray_glyph(bmp: &freetype::Bitmap, offset_x: i32, offset_y: i32) -> RasterizedGlyph {
    let raw_width = bmp.width().max(0) as u32;
    let height = bmp.rows().max(0) as u32;
    let pitch = bmp.pitch();
    let abs_pitch = pitch.unsigned_abs() as usize;
    let buffer = bmp.buffer();

    let row_pixels = raw_width as usize;
    let mut pixels = vec![0u8; (raw_width * height * 4) as usize];
    let src = SourceBitmap {
        data: buffer,
        width: raw_width,
        height,
        pitch,
        abs_pitch,
    };
    for (y, dst_row) in pixels
        .as_chunks_mut::<4>()
        .0
        .chunks_exact_mut(row_pixels)
        .enumerate()
    {
        let src_row = src.row(y as u32);
        unpack_gray_row(src_row, dst_row);
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

struct BoxAccumulator {
    sum_b: u64,
    sum_g: u64,
    sum_r: u64,
    sum_a: u64,
    count: u64,
}

impl BoxAccumulator {
    fn new() -> Self {
        Self {
            sum_b: 0,
            sum_g: 0,
            sum_r: 0,
            sum_a: 0,
            count: 0,
        }
    }

    fn add_row(&mut self, src_row: &[u8], sx_start: usize, sx_end: usize) {
        let end = sx_end.min(src_row.len() / 4);
        if sx_start >= end {
            return;
        }
        for chunk in src_row[sx_start * 4..end * 4].as_chunks::<4>().0 {
            self.sum_b += chunk[0] as u64;
            self.sum_g += chunk[1] as u64;
            self.sum_r += chunk[2] as u64;
            self.sum_a += chunk[3] as u64;
            self.count += 1;
        }
    }

    fn finish(self) -> [u8; 4] {
        let Some(avg_a_val) = self.sum_a.checked_div(self.count) else {
            return [0, 0, 0, 0];
        };
        let avg_a = avg_a_val as u8;
        if avg_a == 0 {
            return [0, 0, 0, 0];
        }
        let avg_b = (self.sum_b.checked_div(self.count).unwrap_or(0)) as u32;
        let avg_g = (self.sum_g.checked_div(self.count).unwrap_or(0)) as u32;
        let avg_r = (self.sum_r.checked_div(self.count).unwrap_or(0)) as u32;
        let a_u32 = avg_a as u32;
        let r = ((avg_r * 255 + a_u32 / 2) / a_u32).min(255) as u8;
        let g = ((avg_g * 255 + a_u32 / 2) / a_u32).min(255) as u8;
        let b = ((avg_b * 255 + a_u32 / 2) / a_u32).min(255) as u8;
        [r, g, b, avg_a]
    }
}

fn sample_box_pixel(src: &SourceBitmap, sx_range: (usize, usize), sy_range: (u32, u32)) -> [u8; 4] {
    let mut acc = BoxAccumulator::new();
    for sy in sy_range.0..sy_range.1 {
        let row = src.row(sy);
        acc.add_row(row, sx_range.0, sx_range.1);
    }
    acc.finish()
}

fn downsample_row(src: &SourceBitmap, x_scale: f64, sy_range: (u32, u32), dst_row: &mut [[u8; 4]]) {
    for (dx, dst_chunk) in dst_row.iter_mut().enumerate() {
        let sx_start = ((dx as f64) * x_scale).floor() as usize;
        let sx_end = (((dx + 1) as f64) * x_scale).ceil().min(src.width as f64) as usize;
        *dst_chunk = sample_box_pixel(src, (sx_start, sx_end), sy_range);
    }
}

fn downsample_bgra_to_rgba(
    src_bytes: &[u8],
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
    let pitch = if pitch_negative {
        -(abs_pitch as i32)
    } else {
        abs_pitch as i32
    };
    let src = SourceBitmap {
        data: src_bytes,
        width: src_width,
        height: src_height,
        pitch,
        abs_pitch,
    };

    for (dy, dst_row) in dst
        .as_chunks_mut::<4>()
        .0
        .chunks_exact_mut(dst_width as usize)
        .enumerate()
    {
        let sy_start = ((dy as f64) * y_scale).floor() as u32;
        let sy_end = (((dy + 1) as f64) * y_scale).ceil().min(src_height as f64) as u32;
        downsample_row(&src, x_scale, (sy_start, sy_end), dst_row);
    }
    dst
}

fn bgra_to_rgba_row(src_row: &[u8], dst_row: &mut [[u8; 4]]) {
    for (src_chunk, dst_chunk) in src_row.as_chunks::<4>().0.iter().zip(dst_row.iter_mut()) {
        let b = src_chunk[0] as u32;
        let g = src_chunk[1] as u32;
        let r = src_chunk[2] as u32;
        let a = src_chunk[3] as u32;
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
        *dst_chunk = dst_px;
    }
}

fn bgra_to_rgba(
    src_bytes: &[u8],
    width: u32,
    height: u32,
    abs_pitch: usize,
    pitch_negative: bool,
) -> Vec<u8> {
    let mut dst = vec![0u8; (width * height * 4) as usize];
    let pitch = if pitch_negative {
        -(abs_pitch as i32)
    } else {
        abs_pitch as i32
    };
    let src = SourceBitmap {
        data: src_bytes,
        width,
        height,
        pitch,
        abs_pitch,
    };
    for (y, dst_row) in dst
        .as_chunks_mut::<4>()
        .0
        .chunks_exact_mut(width as usize)
        .enumerate()
    {
        let src_row = src.row(y as u32);
        bgra_to_rgba_row(src_row, dst_row);
    }
    dst
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_source_bitmap_row_indexing() {
        let data = [1, 2, 3, 4, 5, 6];
        let pos_bmp = SourceBitmap {
            data: &data,
            width: 2,
            height: 3,
            pitch: 2,
            abs_pitch: 2,
        };
        assert_eq!(pos_bmp.row(0), &[1, 2, 3, 4, 5, 6]);
        assert_eq!(pos_bmp.row(1), &[3, 4, 5, 6]);
        assert_eq!(pos_bmp.row(2), &[5, 6]);
        assert_eq!(pos_bmp.row(3), &[]);

        let neg_bmp = SourceBitmap {
            data: &data,
            width: 2,
            height: 3,
            pitch: -2,
            abs_pitch: 2,
        };
        assert_eq!(neg_bmp.row(0), &[5, 6]);
        assert_eq!(neg_bmp.row(2), &[1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn test_unpack_row_formats() {
        let lcd_data = [10, 20, 30];
        let mut dst_rgb = [[0u8; 4]];
        unpack_lcd_row(&lcd_data, &mut dst_rgb, false);
        assert_eq!(dst_rgb[0], [10, 20, 30, 30]);

        let mut dst_bgr = [[0u8; 4]];
        unpack_lcd_row(&lcd_data, &mut dst_bgr, true);
        assert_eq!(dst_bgr[0], [30, 20, 10, 30]);

        let mono_byte = [0b1000_0001];
        let mut dst_mono = [[0u8; 4]; 8];
        unpack_mono_row(&mono_byte, &mut dst_mono);
        assert_eq!(dst_mono[0], [255, 255, 255, 255]);
        assert_eq!(dst_mono[1], [0, 0, 0, 0]);
        assert_eq!(dst_mono[7], [255, 255, 255, 255]);

        let gray_data = [42, 128];
        let mut dst_gray = [[0u8; 4]; 2];
        unpack_gray_row(&gray_data, &mut dst_gray);
        assert_eq!(dst_gray[0], [42, 42, 42, 42]);
        assert_eq!(dst_gray[1], [128, 128, 128, 128]);
    }

    #[test]
    fn test_box_accumulator_and_bgra_to_rgba() {
        let acc = BoxAccumulator::new();
        assert_eq!(acc.finish(), [0, 0, 0, 0]);

        let mut acc2 = BoxAccumulator::new();
        acc2.add_row(&[100, 150, 200, 255], 0, 1);
        let out = acc2.finish();
        assert_eq!(out[3], 255);
        assert!(out[0] > 0);

        let bgra = [255, 128, 64, 255];
        let mut dst_rgba = [[0u8; 4]];
        bgra_to_rgba_row(&bgra, &mut dst_rgba);
        assert_eq!(dst_rgba[0][0], 64);
        assert_eq!(dst_rgba[0][1], 128);
        assert_eq!(dst_rgba[0][2], 255);
        assert_eq!(dst_rgba[0][3], 255);
    }
}
