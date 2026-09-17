//! Fontconfig system fallback font discovery, candidate matching, and LRU cache.

use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use fontconfig::{CharSet, Fontconfig, Pattern};

use crate::font::MAX_FONT_ZOOM_SCALE;

pub(crate) const MAX_FALLBACK_FACES: usize = 64;
pub(crate) const MAX_RESOLVED_CACHE: usize = 4096;

pub(crate) struct FallbackFace {
    pub(crate) path: PathBuf,
    pub(crate) index: u32,
    pub(crate) style: u8,
    pub(crate) font: fontdue::Font,
}

#[derive(Default)]
pub(crate) struct FallbackCache {
    pub(crate) faces: Vec<FallbackFace>,
    pub(crate) resolved: HashMap<(char, u8), Option<(u16, u16)>>,
    pub(crate) resolved_order: VecDeque<(char, u8)>,
}

impl FallbackCache {
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
        for (pos, face) in self.faces.iter().enumerate() {
            if face.style == style {
                let glyph = face.font.lookup_glyph_index(c);
                if glyph != 0 {
                    return Some((pos as u16, glyph));
                }
            }
        }

        let fc = fontconfig()?;
        let bold = (style & 1) != 0;
        let italic = (style & 2) != 0;
        let candidates = query_fontconfig_candidates(fc, preferred_family, bold, italic, c)?;

        for (path, index) in candidates {
            let position =
                match self.faces.iter().position(|face| {
                    face.path == path && face.index == index && face.style == style
                }) {
                    Some(position) => position,
                    None => {
                        let Ok(font) = load_font_file(&path, index) else {
                            continue;
                        };
                        if self.faces.len() >= MAX_FALLBACK_FACES {
                            self.faces.remove(0);
                            self.resolved.clear();
                            self.resolved_order.clear();
                        }
                        self.faces.push(FallbackFace {
                            path,
                            index,
                            style,
                            font,
                        });
                        self.faces.len() - 1
                    }
                };

            let glyph = self.faces[position].font.lookup_glyph_index(c);
            if glyph != 0 {
                return Some((position as u16, glyph));
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

    if let Ok(font_set) = pattern.sort_fonts(fontconfig::UnicodeCoverage::Trim) {
        let mut candidates = Vec::new();
        for p in font_set.iter().take(8) {
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

    let matched = pattern.font_match().ok()?;
    let path = PathBuf::from(matched.filename().ok()?);
    let index = u32::try_from(matched.face_index().ok()?).ok()?;
    Some(vec![(path, index)])
}

pub(crate) fn load_font_bytes(bytes: &[u8], collection_index: u32) -> io::Result<fontdue::Font> {
    fontdue::Font::from_bytes(
        bytes,
        fontdue::FontSettings {
            collection_index,
            scale: MAX_FONT_ZOOM_SCALE,
            load_substitutions: false,
        },
    )
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub(crate) fn load_font_file(path: &Path, collection_index: u32) -> io::Result<fontdue::Font> {
    let bytes = fs::read(path)?;
    load_font_bytes(&bytes, collection_index)
}
