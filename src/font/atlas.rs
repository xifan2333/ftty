//! Shelf-packed glyph atlas: bounded-growth packing of rasterized glyph bitmaps.

use std::collections::HashMap;

use crate::grid::CellFlags;

use super::{FontManager, RasterizedGlyph, style_index};

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
    pub colored: bool,
}

#[derive(Default)]
pub struct Shelf {
    pub x: u32,
    pub y: u32,
    pub height: u32,
}

impl Shelf {
    // Include a transparent pixel on every side; failed allocations leave the shelf alone.
    pub fn allocate(&mut self, width: u32, height: u32, bounds: [u32; 2]) -> Option<[u32; 2]> {
        let padded_width = width.checked_add(2)?;
        let padded_height = height.checked_add(2)?;
        if padded_width > bounds[0] || padded_height > bounds[1] {
            return None;
        }
        // Every offset is checked: `Shelf` and its fields are public, so a caller can install
        // coordinates that would otherwise overflow and wrap into bogus glyph positions.
        let next_x = self.x.checked_add(padded_width);
        let (x, y, shelf_height) = match next_x {
            Some(next_x) if next_x <= bounds[0] => (self.x, self.y, self.height),
            _ => (0, self.y.checked_add(self.height)?, 0),
        };
        if y.checked_add(padded_height)? > bounds[1] {
            return None;
        }
        self.x = x.checked_add(padded_width)?;
        self.y = y;
        self.height = shelf_height.max(padded_height);
        Some([x + 1, y + 1])
    }
}

/// A shelf-packed atlas with bounded growth. Clear it between frames to evict old glyphs.
pub struct GlyphAtlas {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub dirty: bool,
    pub full_upload: bool,
    pub dirty_rect: Option<[u32; 4]>,
    shelf: Shelf,
    // Fast L1 array cache for ASCII characters (0..127) across 4 styles (0..3).
    pub ascii_cache: [Option<CachedGlyph>; 128 * 4],
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
            colored: glyph.colored,
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
