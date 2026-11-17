//! Fontconfig family matching and the fallback face cache.

use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use fontconfig::{CharSet, Fontconfig, Pattern};

use super::face::Font;

pub(crate) const MAX_FALLBACK_FACES: usize = 64;
pub const MAX_RESOLVED_CACHE: usize = 4096;

pub struct FallbackFace {
    pub path: PathBuf,
    pub index: u32,
    pub style: u8,
    pub font: Font,
}

#[derive(Default)]
pub struct FallbackCache {
    pub faces: Vec<FallbackFace>,
    pub resolved: HashMap<(char, u8), Option<(u16, u16)>>,
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

    fn find_loaded_face(&self, c: char, exact_style: Option<u8>) -> Option<(u16, u16)> {
        for (pos, face) in self.faces.iter().enumerate() {
            if exact_style.is_none_or(|s| face.style == s) {
                let glyph = face.font.lookup_glyph_index(c);
                if glyph != 0 {
                    return Some((pos as u16, glyph));
                }
            }
        }
        None
    }

    fn ensure_candidate_face(&mut self, path: PathBuf, index: u32, style: u8) -> Option<usize> {
        if let Some(pos) = self
            .faces
            .iter()
            .position(|face| face.path == path && face.index == index && face.style == style)
        {
            return Some(pos);
        }
        let font = load_font_file(&path, index).ok()?;
        Some(self.insert_face(FallbackFace {
            path,
            index,
            style,
            font,
        }))
    }

    fn discover(&mut self, c: char, style: u8, preferred_family: &str) -> Option<(u16, u16)> {
        // Step 1: Check if an already loaded face matching the exact requested style covers `c`
        if let Some(hit) = self.find_loaded_face(c, Some(style)) {
            return Some(hit);
        }

        // Step 2: Attempt Fontconfig discovery for the specific requested style (bold/italic)
        let bold = (style & 1) != 0;
        let italic = (style & 2) != 0;
        if let Some(fc) = fontconfig()
            && let Some(candidates) =
                query_fontconfig_candidates(fc, preferred_family, bold, italic, c)
        {
            for (path, index) in candidates {
                let Some(position) = self.ensure_candidate_face(path, index, style) else {
                    continue;
                };
                let glyph = self.faces[position].font.lookup_glyph_index(c);
                if glyph != 0 {
                    return Some((position as u16, glyph));
                }
            }
        }

        // Step 3: Graceful fallback: reuse an already loaded face of any other style covering `c`
        self.find_loaded_face(c, None)
    }

    pub fn resolve_cluster(
        &mut self,
        cluster: &str,
        style: u8,
        preferred_family: &str,
    ) -> Option<(u16, u16)> {
        // Step 1: Check if an already loaded face covers it
        for (pos, face) in self.faces.iter().enumerate() {
            if let Some(glyph) = face.font.shape_cluster(cluster) {
                return Some((pos as u16, glyph));
            }
        }
        self.discover_cluster(cluster, style, preferred_family)
    }

    fn discover_cluster(
        &mut self,
        cluster: &str,
        style: u8,
        preferred_family: &str,
    ) -> Option<(u16, u16)> {
        let fc = fontconfig()?;

        for c in cluster.chars().rev() {
            if c.is_ascii() && !c.is_control() {
                continue;
            }
            if let Some(res) = self.try_candidate_char(fc, preferred_family, style, c, cluster) {
                return Some(res);
            }
        }

        self.try_emoji_family(fc, style, cluster)
    }

    fn try_candidate_char(
        &mut self,
        fc: &Fontconfig,
        preferred_family: &str,
        style: u8,
        c: char,
        cluster: &str,
    ) -> Option<(u16, u16)> {
        let bold = (style & 1) != 0;
        let italic = (style & 2) != 0;
        let candidates = query_fontconfig_candidates(fc, preferred_family, bold, italic, c)?;
        for (path, index) in candidates {
            let position = self.ensure_candidate_face(path, index, style)?;
            if let Some(glyph) = self.faces[position].font.shape_cluster(cluster) {
                return Some((position as u16, glyph));
            }
        }
        None
    }

    fn try_emoji_family(
        &mut self,
        fc: &Fontconfig,
        style: u8,
        cluster: &str,
    ) -> Option<(u16, u16)> {
        let (path, index) = match_family(fc, "emoji", false, false)?;
        let position = self.ensure_candidate_face(path, index, style)?;
        let glyph = self.faces[position].font.shape_cluster(cluster)?;
        Some((position as u16, glyph))
    }
}

pub fn fontconfig() -> Option<&'static Fontconfig> {
    static FC: OnceLock<Option<Fontconfig>> = OnceLock::new();
    FC.get_or_init(Fontconfig::new).as_ref()
}

fn configure_pattern_style(
    pattern: &mut Pattern,
    family_name: &str,
    bold: bool,
    italic: bool,
) -> Option<()> {
    let trimmed = family_name.trim();
    if !trimmed.is_empty()
        && !trimmed.eq_ignore_ascii_case("monospace")
        && let Ok(c_family) = CString::new(trimmed)
    {
        pattern.add_string(fontconfig::FC_FAMILY, &c_family).ok()?;
    }
    if let Ok(c_mono) = CString::new("monospace") {
        pattern.add_string(fontconfig::FC_FAMILY, &c_mono).ok()?;
    }
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
    Some(())
}

fn extract_pattern_match(pat: &mut Pattern) -> Option<(PathBuf, u32)> {
    let matched = pat.font_match().ok()?;
    let filename = matched.filename().ok()?;
    let face_index = matched.face_index().ok()?;
    let index = u32::try_from(face_index).ok()?;
    Some((PathBuf::from(filename), index))
}

fn extract_candidate_set(pattern: &mut Pattern) -> Option<Vec<(PathBuf, u32)>> {
    let font_set = pattern.sort_fonts(fontconfig::UnicodeCoverage::Trim).ok()?;
    let mut candidates = Vec::new();
    for p in font_set.iter().take(2) {
        if let Ok(filename) = p.filename()
            && let Ok(face_index) = p.face_index()
            && let Ok(index) = u32::try_from(face_index)
        {
            candidates.push((PathBuf::from(filename), index));
        }
    }
    if candidates.is_empty() {
        None
    } else {
        Some(candidates)
    }
}

pub fn match_family(
    fc: &Fontconfig,
    family_name: &str,
    bold: bool,
    italic: bool,
) -> Option<(PathBuf, u32)> {
    let mut pat = Pattern::new(fc).ok()?;
    configure_pattern_style(&mut pat, family_name, bold, italic)?;
    extract_pattern_match(&mut pat)
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

    configure_pattern_style(&mut pattern, preferred_family, bold, italic)?;
    if let Some(direct) = extract_pattern_match(&mut pattern) {
        return Some(vec![direct]);
    }
    extract_candidate_set(&mut pattern)
}

#[cfg(test)]
pub fn load_font_bytes(bytes: &[u8], collection_index: u32) -> io::Result<Font> {
    Font::from_bytes(bytes, collection_index)
}

pub fn load_font_file(path: &Path, collection_index: u32) -> io::Result<Font> {
    Font::from_file(path, collection_index)
}
