//! Lazy FreeType font face handle and on-demand glyph rasterization.

use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use freetype::face::LoadFlag;

fn ft_library() -> io::Result<&'static freetype::Library> {
    static LIB: OnceLock<Result<freetype::Library, String>> = OnceLock::new();
    let res = LIB.get_or_init(|| freetype::Library::init().map_err(|e| format!("{e:?}")));
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

/// Output of a lazily rasterized glyph bitmap.
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

struct FtFaceWrapper {
    face: freetype::Face,
}

// SAFETY: FreeType `FT_Face` handles are uniquely owned and exclusively
// mutated behind `Mutex<FtFaceWrapper>` synchronization across all threads.
unsafe impl Send for FtFaceWrapper {}

// SAFETY: External synchronization is enforced through `Mutex<FtFaceWrapper>`.
unsafe impl Sync for FtFaceWrapper {}

/// Thread-safe handle to a FreeType face with lazy outline rasterization.
#[derive(Clone)]
pub struct Font {
    inner: Arc<Mutex<FtFaceWrapper>>,
}

impl Font {
    /// Opens a font face from a file path in sub-milliseconds without parsing glyph outlines.
    ///
    /// # Errors
    /// Returns [`std::io::Error`] if the file cannot be opened or FreeType fails to parse headers.
    pub fn from_file(path: &Path, collection_index: u32) -> io::Result<Self> {
        let face = ft_library()?
            .new_face(path, collection_index as isize)
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("FreeType new_face failed: {e:?}"),
                )
            })?;
        Ok(Self {
            inner: Arc::new(Mutex::new(FtFaceWrapper { face })),
        })
    }

    /// Opens a font face from in-memory font bytes in sub-milliseconds without parsing glyph outlines.
    ///
    /// # Errors
    /// Returns [`std::io::Error`] if FreeType fails to parse font tables from the bytes.
    pub fn from_bytes(bytes: &[u8], collection_index: u32) -> io::Result<Self> {
        let face = ft_library()?
            .new_memory_face(bytes.to_vec(), collection_index as isize)
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("FreeType new_memory_face failed: {e:?}"),
                )
            })?;
        Ok(Self {
            inner: Arc::new(Mutex::new(FtFaceWrapper { face })),
        })
    }

    /// Looks up the glyph index in the font's character map (`cmap`).
    ///
    /// Returns `0` if the character is not mapped (.notdef).
    #[must_use]
    pub fn lookup_glyph_index(&self, c: char) -> u16 {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.face.get_char_index(c as usize).unwrap_or(0) as u16
    }

    /// Sets the active character pixel height on the face.
    fn set_font_size(face: &freetype::Face, font_size: f32) {
        let px = font_size.round().max(1.0) as u32;
        let _ = face.set_pixel_sizes(0, px);
    }

    /// Retrieves line height and baseline ascent metrics.
    #[must_use]
    pub fn horizontal_line_metrics(&self, font_size: f32) -> Option<LineMetrics> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::set_font_size(&guard.face, font_size);
        let metrics = guard.face.size_metrics()?;
        let ascent = (metrics.ascender >> 6) as f32;
        let descent = (metrics.descender >> 6) as f32;
        let height = (metrics.height >> 6) as f32;
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
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::set_font_size(&guard.face, font_size);
        let glyph_idx = guard.face.get_char_index(c as usize).unwrap_or(0);
        if guard.face.load_glyph(glyph_idx, LoadFlag::DEFAULT).is_ok() {
            let advance = guard.face.glyph().advance().x >> 6;
            if advance > 0 {
                return advance as f32;
            }
        }
        (font_size * 0.6).ceil().max(1.0)
    }

    /// Rasterizes an indexed glyph on-demand into an 8-bit alpha mask.
    #[must_use]
    pub fn rasterize_indexed(&self, glyph_index: u16, font_size: f32) -> RasterizedGlyph {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::set_font_size(&guard.face, font_size);
        let flags = LoadFlag::RENDER | LoadFlag::TARGET_LIGHT;
        if guard.face.load_glyph(glyph_index as u32, flags).is_err() {
            return RasterizedGlyph::empty();
        }
        let slot = guard.face.glyph();
        let bmp = slot.bitmap();
        let width = bmp.width().max(0) as u32;
        let height = bmp.rows().max(0) as u32;
        if width == 0 || height == 0 {
            return RasterizedGlyph::empty();
        }
        let offset_x = slot.bitmap_left();
        let offset_y = slot.bitmap_top() - height as i32;
        let pitch = bmp.pitch().unsigned_abs() as usize;
        RasterizedGlyph {
            width,
            height,
            offset_x,
            offset_y,
            pitch,
            pixels: bmp.buffer().to_vec(),
        }
    }
}
