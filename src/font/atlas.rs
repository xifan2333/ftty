//! Dynamic shelf-packing glyph texture atlas with bounded exponential growth.

use std::collections::HashMap;

use crate::font::{FontManager, RasterizedGlyph, style_index};
use crate::grid::CellFlags;

pub const MAX_ATLAS_SIZE: u32 = 2048;

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
pub(crate) struct Shelf {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) height: u32,
}

impl Shelf {
    // Include a transparent pixel on every side; failed allocations leave the shelf alone.
    pub(crate) fn allocate(
        &mut self,
        width: u32,
        height: u32,
        bounds: [u32; 2],
    ) -> Option<[u32; 2]> {
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
    // Fast L1 array cache for ASCII characters (0..127) across 4 styles (0..3).
    pub(crate) ascii_cache: [Option<CachedGlyph>; 128 * 4],
    // Fallback O(1) cache for non-ASCII characters directly keyed by (char, style_index).
    pub(crate) cache: HashMap<(char, u8), CachedGlyph>,
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
            ascii_cache: [None; 128 * 4],
            cache: HashMap::new(),
        }
    }

    pub(crate) fn clear(&mut self) {
        self.pixels.fill(0);
        self.cache.clear();
        self.ascii_cache = [None; 128 * 4];
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
        cached.position = loop {
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
        let [x, y] = cached.position;
        let pitch = glyph.pitch.max(width as usize);
        for row in 0..height {
            let src = (row as usize) * pitch;
            let dst = ((y + row) * self.width + x) as usize;
            if src + width as usize <= glyph.pixels.len() {
                self.pixels[dst..dst + width as usize]
                    .copy_from_slice(&glyph.pixels[src..src + width as usize]);
            }
        }
        self.dirty = true;
        Some(cached)
    }

    /// Looks up a glyph without modifying the atlas, for use after frame preparation.
    pub(crate) fn get(
        &self,
        c: char,
        flags: CellFlags,
        _fonts: &FontManager,
    ) -> Option<CachedGlyph> {
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
