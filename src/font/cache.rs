//! Persistent font resolution and cell metrics cache to bypass Fontconfig initialization on startup.

use std::hash::Hasher;
use std::io::{self, Cursor, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::font::{CellMetrics, MAX_FONT_SIZE, MIN_FONT_SIZE};

const CACHE_MAGIC: &[u8; 8] = b"FTTYFONT";
const CACHE_VERSION: u32 = 4;

/// Maximum number of font family names supported in a single cache entry.
pub const MAX_CACHED_FAMILIES: usize = 64;
/// Maximum number of fallback font entries supported in a single cache entry.
pub const MAX_CACHED_FALLBACKS: usize = 64;

/// File metadata tracking a resolved font on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedFontFile {
    pub path: PathBuf,
    pub index: u32,
    pub mtime_secs: i64,
    pub mtime_nanos: u32,
    pub file_size: u64,
}

impl CachedFontFile {
    /// Inspects the file at `path` and builds a cache record if readable.
    #[must_use]
    pub fn from_path_and_index(path: PathBuf, index: u32) -> Option<Self> {
        let meta = std::fs::metadata(&path).ok()?;
        let (mtime_secs, mtime_nanos) = match meta.modified() {
            Ok(time) => match time.duration_since(SystemTime::UNIX_EPOCH) {
                Ok(dur) => (dur.as_secs() as i64, dur.subsec_nanos()),
                Err(err) => {
                    let dur = err.duration();
                    (-(dur.as_secs() as i64), dur.subsec_nanos())
                }
            },
            Err(_) => (0, 0),
        };
        Some(Self {
            path,
            index,
            mtime_secs,
            mtime_nanos,
            file_size: meta.len(),
        })
    }

    /// Verifies that the font file still exists on disk with matching size and modification time.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return false;
        };
        if meta.len() != self.file_size {
            return false;
        }
        let (mtime_secs, mtime_nanos) = match meta.modified() {
            Ok(time) => match time.duration_since(SystemTime::UNIX_EPOCH) {
                Ok(dur) => (dur.as_secs() as i64, dur.subsec_nanos()),
                Err(err) => {
                    let dur = err.duration();
                    (-(dur.as_secs() as i64), dur.subsec_nanos())
                }
            },
            Err(_) => (0, 0),
        };
        mtime_secs == self.mtime_secs && mtime_nanos == self.mtime_nanos
    }
}

/// Serialized font cache holding resolved primary font, metrics, and fallback paths.
#[derive(Debug, Clone, PartialEq)]
pub struct FontCacheData {
    pub families: Vec<String>,
    pub font_size: f32,
    pub subpixel: bool,
    pub dirs_fingerprint: u64,
    pub primary: CachedFontFile,
    pub metrics: CellMetrics,
    pub fallbacks: Vec<Option<CachedFontFile>>,
}

/// Generates a fingerprint of font system directories to detect font installations or config changes.
#[must_use]
pub fn font_system_fingerprint() -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let dirs = [
        Path::new("/etc/fonts"),
        Path::new("/usr/share/fonts"),
        Path::new("/usr/local/share/fonts"),
    ];
    for d in &dirs {
        if let Ok(meta) = std::fs::metadata(d)
            && let Ok(mtime) = meta.modified()
            && let Ok(dur) = mtime.duration_since(SystemTime::UNIX_EPOCH)
        {
            hasher.write_u64(dur.as_secs());
            hasher.write_u32(dur.subsec_nanos());
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home_path = PathBuf::from(home);
        for sub in [".config/fontconfig", ".local/share/fonts", ".fonts"] {
            let path = home_path.join(sub);
            if let Ok(meta) = std::fs::metadata(&path)
                && let Ok(mtime) = meta.modified()
                && let Ok(dur) = mtime.duration_since(SystemTime::UNIX_EPOCH)
            {
                hasher.write_u64(dur.as_secs());
                hasher.write_u32(dur.subsec_nanos());
            }
        }
    }
    hasher.finish()
}

/// Locates the persistent font cache file path in `$XDG_CACHE_HOME/ftty/font_cache.bin`.
#[must_use]
pub fn cache_file_path() -> Option<PathBuf> {
    let cache_dir = if let Some(val) = std::env::var_os("XDG_CACHE_HOME") {
        if !val.is_empty() {
            PathBuf::from(val)
        } else {
            dirs_cache_dir()?
        }
    } else {
        dirs_cache_dir()?
    };
    Some(cache_dir.join("ftty").join("font_cache.bin"))
}

fn dirs_cache_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join(".cache"))
}

/// Attempts to load and validate cached font metadata matching requested configuration.
#[must_use]
pub fn try_load_cache(
    families: &[String],
    font_size: f32,
    subpixel: bool,
) -> Option<FontCacheData> {
    let cache_path = cache_file_path()?;
    let bytes = std::fs::read(cache_path).ok()?;
    let data = deserialize_cache(&bytes)?;

    if data.families != families
        || (data.font_size - font_size).abs() > f32::EPSILON
        || data.subpixel != subpixel
    {
        return None;
    }

    if data.dirs_fingerprint != font_system_fingerprint() {
        return None;
    }

    if !data.primary.is_valid() {
        return None;
    }
    for fb in data.fallbacks.iter().flatten() {
        if !fb.is_valid() {
            return None;
        }
    }

    Some(data)
}

/// Atomically persists resolved font metadata and cell metrics to cache file.
pub fn save_cache(
    families: &[String],
    font_size: f32,
    subpixel: bool,
    primary_path: &Path,
    primary_index: u32,
    metrics: CellMetrics,
    fallback_entries: &[Option<(PathBuf, u32)>],
) {
    if families.len() > MAX_CACHED_FAMILIES || fallback_entries.len() > MAX_CACHED_FALLBACKS {
        return;
    }

    let Some(cache_path) = cache_file_path() else {
        return;
    };
    let Some(parent) = cache_path.parent() else {
        return;
    };
    let _ = std::fs::create_dir_all(parent);

    let Some(primary) =
        CachedFontFile::from_path_and_index(primary_path.to_path_buf(), primary_index)
    else {
        return;
    };
    let mut fallbacks = Vec::with_capacity(fallback_entries.len());
    for entry in fallback_entries {
        match entry {
            Some((path, index)) => {
                fallbacks.push(CachedFontFile::from_path_and_index(path.clone(), *index));
            }
            None => {
                fallbacks.push(None);
            }
        }
    }

    let payload = serialize_cache(&FontCacheData {
        families: families.to_vec(),
        font_size,
        subpixel,
        dirs_fingerprint: font_system_fingerprint(),
        primary,
        metrics,
        fallbacks,
    });

    let tmp_path = parent.join(format!("font_cache.bin.tmp.{}", std::process::id()));
    if std::fs::write(&tmp_path, payload).is_ok() {
        let _ = std::fs::rename(&tmp_path, &cache_path);
    }
}

/// Serializes font cache data to compact binary buffer.
#[must_use]
pub fn serialize_cache(data: &FontCacheData) -> Vec<u8> {
    let mut buf = Vec::with_capacity(512);
    buf.extend_from_slice(CACHE_MAGIC);
    buf.extend_from_slice(&CACHE_VERSION.to_le_bytes());

    buf.extend_from_slice(&(data.families.len() as u32).to_le_bytes());
    for family in &data.families {
        let bytes = family.as_bytes();
        buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        buf.extend_from_slice(bytes);
    }

    buf.extend_from_slice(&data.font_size.to_bits().to_le_bytes());
    buf.push(u8::from(data.subpixel));
    buf.extend_from_slice(&data.dirs_fingerprint.to_le_bytes());

    write_cached_file(&mut buf, &data.primary);

    buf.extend_from_slice(&data.metrics.cell_width.to_le_bytes());
    buf.extend_from_slice(&data.metrics.cell_height.to_le_bytes());
    buf.extend_from_slice(&data.metrics.ascent.to_le_bytes());

    buf.extend_from_slice(&(data.fallbacks.len() as u32).to_le_bytes());
    for fb in &data.fallbacks {
        match fb {
            Some(file) => {
                buf.push(1);
                write_cached_file(&mut buf, file);
            }
            None => {
                buf.push(0);
            }
        }
    }

    buf
}

fn write_cached_file(buf: &mut Vec<u8>, file: &CachedFontFile) {
    let raw_bytes = file.path.as_os_str().as_bytes();
    buf.extend_from_slice(&(raw_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(raw_bytes);
    buf.extend_from_slice(&file.index.to_le_bytes());
    buf.extend_from_slice(&file.mtime_secs.to_le_bytes());
    buf.extend_from_slice(&file.mtime_nanos.to_le_bytes());
    buf.extend_from_slice(&file.file_size.to_le_bytes());
}

/// Deserializes font cache data from compact binary buffer.
#[must_use]
pub fn deserialize_cache(bytes: &[u8]) -> Option<FontCacheData> {
    if bytes.len() < 8 + 4 {
        return None;
    }
    if &bytes[..8] != CACHE_MAGIC {
        return None;
    }
    let mut cursor = Cursor::new(&bytes[8..]);
    let version = read_u32(&mut cursor).ok()?;
    if version != CACHE_VERSION {
        return None;
    }

    let num_families = read_u32(&mut cursor).ok()? as usize;
    if num_families > MAX_CACHED_FAMILIES {
        return None;
    }
    let mut families = Vec::with_capacity(num_families);
    for _ in 0..num_families {
        let str_len = read_u32(&mut cursor).ok()? as usize;
        if str_len > 1024 {
            return None;
        }
        let mut s_bytes = vec![0u8; str_len];
        cursor.read_exact(&mut s_bytes).ok()?;
        let family = String::from_utf8(s_bytes).ok()?;
        families.push(family);
    }

    let font_size = f32::from_bits(read_u32(&mut cursor).ok()?);
    if !font_size.is_finite() || font_size < MIN_FONT_SIZE || font_size > MAX_FONT_SIZE {
        return None;
    }

    let mut subpixel_byte = [0u8; 1];
    cursor.read_exact(&mut subpixel_byte).ok()?;
    let subpixel = subpixel_byte[0] != 0;

    let dirs_fingerprint = read_u64(&mut cursor).ok()?;

    let primary = read_cached_file(&mut cursor).ok()?;

    let cell_width = read_u32(&mut cursor).ok()?;
    let cell_height = read_u32(&mut cursor).ok()?;
    let ascent = read_i32(&mut cursor).ok()?;

    if !(1..=256).contains(&cell_width) || !(1..=256).contains(&cell_height) {
        return None;
    }
    if ascent < -(cell_height as i32) || ascent > (cell_height as i32 * 2) {
        return None;
    }

    let metrics = CellMetrics {
        cell_width,
        cell_height,
        ascent,
    };

    let num_fallbacks = read_u32(&mut cursor).ok()? as usize;
    if num_fallbacks > MAX_CACHED_FALLBACKS {
        return None;
    }
    let mut fallbacks = Vec::with_capacity(num_fallbacks);
    for _ in 0..num_fallbacks {
        let mut tag = [0u8; 1];
        cursor.read_exact(&mut tag).ok()?;
        if tag[0] == 1 {
            let fb = read_cached_file(&mut cursor).ok()?;
            fallbacks.push(Some(fb));
        } else {
            fallbacks.push(None);
        }
    }

    Some(FontCacheData {
        families,
        font_size,
        subpixel,
        dirs_fingerprint,
        primary,
        metrics,
        fallbacks,
    })
}

fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_i32<R: Read>(r: &mut R) -> io::Result<i32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(i32::from_le_bytes(b))
}

fn read_i64<R: Read>(r: &mut R) -> io::Result<i64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(i64::from_le_bytes(b))
}

fn read_u64<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn read_cached_file<R: Read>(r: &mut R) -> io::Result<CachedFontFile> {
    let len = read_u32(r)? as usize;
    if len > 4096 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "path too long"));
    }
    let mut path_bytes = vec![0u8; len];
    r.read_exact(&mut path_bytes)?;
    let path = PathBuf::from(std::ffi::OsString::from_vec(path_bytes));
    let index = read_u32(r)?;
    let mtime_secs = read_i64(r)?;
    let mtime_nanos = read_u32(r)?;
    let file_size = read_u64(r)?;

    Ok(CachedFontFile {
        path,
        index,
        mtime_secs,
        mtime_nanos,
        file_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_font_cache_roundtrip() {
        let original = FontCacheData {
            families: vec!["monospace".to_string(), "JoyPixels".to_string()],
            font_size: 14.5,
            subpixel: true,
            dirs_fingerprint: 0x1234_5678_9ABC_DEF0,
            primary: CachedFontFile {
                path: PathBuf::from("/usr/share/fonts/TTF/DejaVuSansMono.ttf"),
                index: 0,
                mtime_secs: 1_700_000_000,
                mtime_nanos: 123_456,
                file_size: 334_128,
            },
            metrics: CellMetrics {
                cell_width: 9,
                cell_height: 18,
                ascent: 14,
            },
            fallbacks: vec![
                None,
                Some(CachedFontFile {
                    path: PathBuf::from("/usr/share/fonts/JoyPixels.ttf"),
                    index: 1,
                    mtime_secs: 1_700_000_100,
                    mtime_nanos: 0,
                    file_size: 20_000_000,
                }),
            ],
        };

        let serialized = serialize_cache(&original);
        let deserialized = deserialize_cache(&serialized);
        assert_eq!(deserialized, Some(original));
    }

    #[test]
    fn test_corrupted_cache_handled_safely() {
        assert!(deserialize_cache(b"").is_none());
        assert!(deserialize_cache(b"INVALID_HEADER_DATA").is_none());
        assert!(deserialize_cache(b"FTTYFONT\x01\x00\x00\x00").is_none()); // Version 1 mismatch
        assert!(deserialize_cache(b"FTTYFONT\x02\x00\x00\x00").is_none()); // Version 2 mismatch
        assert!(deserialize_cache(b"FTTYFONT\x03\x00\x00\x00").is_none()); // Version 3 mismatch
    }

    #[test]
    fn test_invalid_cell_metrics_rejected() {
        let mut data = FontCacheData {
            families: vec!["monospace".to_string()],
            font_size: 14.0,
            subpixel: false,
            dirs_fingerprint: 1,
            primary: CachedFontFile {
                path: PathBuf::from("/nonexistent.ttf"),
                index: 0,
                mtime_secs: 0,
                mtime_nanos: 0,
                file_size: 0,
            },
            metrics: CellMetrics {
                cell_width: 0, // Invalid zero width
                cell_height: 18,
                ascent: 14,
            },
            fallbacks: Vec::new(),
        };
        let bytes = serialize_cache(&data);
        assert!(deserialize_cache(&bytes).is_none());

        data.metrics.cell_width = 10;
        data.metrics.cell_height = 0; // Invalid zero height
        let bytes2 = serialize_cache(&data);
        assert!(deserialize_cache(&bytes2).is_none());
    }
}
