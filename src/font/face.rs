//! Lazy FreeType font face handle and on-demand glyph rasterization.

use std::cell::RefCell;
use std::io;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Mutex, MutexGuard, OnceLock};

use freetype::RenderMode;
use freetype::face::LoadFlag;

use crate::config::{FreeTypeLoadFlags, FreeTypeLoadTarget, FreeTypeRenderTarget};

/// Unified FreeType rasterization configuration aligned with WezTerm options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreeTypeConfig {
    pub load_target: FreeTypeLoadTarget,
    pub render_target: FreeTypeRenderTarget,
    pub load_flags: FreeTypeLoadFlags,
}

impl Default for FreeTypeConfig {
    fn default() -> Self {
        Self {
            load_target: FreeTypeLoadTarget::Light,
            render_target: FreeTypeRenderTarget::HorizontalLcd,
            load_flags: FreeTypeLoadFlags::Default,
        }
    }
}

impl FreeTypeConfig {
    #[must_use]
    pub fn compute_load_flags(self) -> LoadFlag {
        // WezTerm always folds the load target into the load flags, even when hinting is
        // disabled: FT_LOAD_TARGET_LIGHT still governs sub-pixel outline rounding and must
        // match the reference pipeline or glyphs gain an extra faint coverage row.
        let mut flags = match self.load_flags {
            FreeTypeLoadFlags::NoHinting => LoadFlag::NO_HINTING,
            FreeTypeLoadFlags::Default => LoadFlag::DEFAULT,
        };
        flags |= match self.load_target {
            FreeTypeLoadTarget::Light => LoadFlag::TARGET_LIGHT,
            FreeTypeLoadTarget::Normal => LoadFlag::TARGET_NORMAL,
            FreeTypeLoadTarget::Mono => LoadFlag::TARGET_MONO,
            FreeTypeLoadTarget::HorizontalLcd => LoadFlag::TARGET_LCD,
        };
        flags
    }

    #[must_use]
    pub fn compute_render_mode(self, is_subpixel_preferred: bool) -> RenderMode {
        match self.render_target {
            FreeTypeRenderTarget::HorizontalLcd if is_subpixel_preferred => RenderMode::Lcd,
            FreeTypeRenderTarget::HorizontalLcd | FreeTypeRenderTarget::Normal => {
                RenderMode::Normal
            }
            FreeTypeRenderTarget::Light => RenderMode::Light,
            FreeTypeRenderTarget::Mono => RenderMode::Mono,
        }
    }
}

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
pub(crate) fn points_to_pixels(points: f32) -> f32 {
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
        ft_config: FreeTypeConfig,
        subpixel_preferred: bool,
        bgr: bool,
    ) -> RasterizedGlyph {
        let guard = self.inner.borrow();
        let Some(face) = &guard.face else {
            return RasterizedGlyph::empty();
        };
        Self::set_font_size(face, font_size);
        let load_flags = ft_config.compute_load_flags();
        let render_mode = ft_config.compute_render_mode(subpixel_preferred);
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
