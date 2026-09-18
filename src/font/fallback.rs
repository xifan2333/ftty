//! Fontconfig system fallback font discovery, candidate matching, and LRU cache.

use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use fontconfig::{CharSet, Fontconfig, Pattern};

use crate::font::Font;

pub(crate) const MAX_FALLBACK_FACES: usize = 64;
pub(crate) const MAX_RESOLVED_CACHE: usize = 4096;

pub(crate) struct FallbackFace {
    pub(crate) path: PathBuf,
    pub(crate) index: u32,
    pub(crate) style: u8,
    pub(crate) font: Font,
}

#[derive(Default)]
pub(crate) struct FallbackCache {
    pub(crate) faces: Vec<FallbackFace>,
    pub(crate) resolved: HashMap<(char, u8), Option<(u16, u16)>>,
    pub(crate) resolved_order: VecDeque<(char, u8)>,
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

    pub(crate) fn resolve(
        &mut self,
        c: char,
        style: u8,
        preferred_family: &str,
    ) -> Option<(u16, u16)> {
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

pub(crate) fn fontconfig() -> Option<&'static Fontconfig> {
    static FC: OnceLock<Option<Fontconfig>> = OnceLock::new();
    FC.get_or_init(Fontconfig::new).as_ref()
}

pub(crate) fn match_family(
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

#[cfg(test)]
pub(crate) fn load_font_bytes(bytes: &[u8], collection_index: u32) -> io::Result<Font> {
    Font::from_bytes(bytes, collection_index)
}

pub(crate) fn load_font_file(path: &Path, collection_index: u32) -> io::Result<Font> {
    Font::from_file(path, collection_index)
}
