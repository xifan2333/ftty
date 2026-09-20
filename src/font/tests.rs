use std::fs;
use std::io;

use crate::font::FontManager;
use crate::font::atlas::{GlyphAtlas, Shelf};
use crate::font::fallback::{
    FallbackCache, MAX_RESOLVED_CACHE, fontconfig, load_font_bytes, load_font_file, match_family,
};
use crate::grid::CellFlags;

fn fonts() -> &'static FontManager {
    thread_local! {
        static CACHED: &'static FontManager = Box::leak(Box::new(
            FontManager::load(14.0).expect("system monospace font")
        ));
    }
    CACHED.with(|&f| f)
}

#[test]
fn font_metrics_and_rasterization() {
    let fonts = fonts();
    assert!(fonts.metrics.cell_width > 0);
    assert!(fonts.metrics.cell_height >= fonts.metrics.ascent as u32);
    let glyph_idx = fonts.regular().lookup_glyph_index('M');
    let raster = fonts
        .regular()
        .rasterize_indexed(glyph_idx, fonts.font_size, false, false);
    assert!(raster.width > 0 && raster.height > 0);
    assert!(raster.pixels.iter().any(|&pixel| pixel != 0));
}

#[test]
fn rejects_invalid_font_sizes() {
    for size in [0.0, -1.0, 5.9, 72.1, f32::NAN, f32::INFINITY] {
        assert!(
            matches!(FontManager::load(size), Err(e) if e.kind() == io::ErrorKind::InvalidInput)
        );
    }
}

#[test]
fn honors_collection_index_for_paths_and_bytes() {
    let fc = fontconfig().expect("fontconfig must be initialized");
    let (path, _) = match_family(fc, "monospace", false, false).expect("system monospace");
    let bytes = fs::read(&path).expect("read font bytes");
    assert!(load_font_bytes(&bytes, 0).is_ok());
    assert!(load_font_bytes(&bytes, u32::MAX).is_err());
    assert!(load_font_file(&path, 0).is_ok());
    assert!(load_font_file(&path, u32::MAX).is_err());
}

#[test]
fn fallback_discovery_is_cached_per_character() {
    let mut cache = FallbackCache::default();
    let first = cache.resolve('中', 0, "monospace");
    let loaded = cache.faces.len();
    assert_eq!(cache.resolved.len(), 1);

    // Repeated lookups must reuse both the resolved answer and the parsed face.
    assert_eq!(cache.resolve('中', 0, "monospace"), first);
    assert_eq!(cache.faces.len(), loaded);
    assert_eq!(cache.resolved.len(), 1);

    if let Some((face_idx, glyph)) = first {
        assert_ne!(glyph, 0);
        assert_ne!(
            cache.faces[face_idx as usize].font.lookup_glyph_index('中'),
            0
        );
        let face = &cache.faces[face_idx as usize];
        assert!(load_font_file(&face.path, face.index).is_ok());
    }
}

#[test]
fn unassigned_codepoints_do_not_resolve_to_a_glyph() {
    // U+0378 is unassigned, so no installed face should claim coverage for it.
    let key = fonts().face_key('\u{0378}', CellFlags::empty());
    assert_eq!(key.glyph, 0);
    assert_eq!(key.face, 0);
}

#[test]
fn cjk_glyphs_resolve_through_a_fallback_face() {
    let fonts = fonts();
    assert_eq!(fonts.face_key('A', CellFlags::empty()).face, 0);
    let primary = fonts.font_for_style(CellFlags::empty());
    if primary.lookup_glyph_index('中') != 0 {
        return; // The primary face already covers CJK on this system.
    }
    let key = fonts.face_key('中', CellFlags::empty());
    if key.face == 0 {
        eprintln!("skipping: no CJK fallback font installed");
        return;
    }
    let mut atlas = GlyphAtlas::new(256, 256);
    let glyph = atlas
        .get_or_insert('中', CellFlags::empty(), fonts)
        .expect("atlas must fit a CJK glyph");
    assert!(
        glyph.width > 0 && glyph.height > 0,
        "CJK fallback produced an empty bitmap"
    );
    assert!(atlas.pixels.iter().any(|&pixel| pixel != 0));
}

#[test]
fn font_chain_prioritizes_configured_families() {
    let chain =
        FontManager::load_with_families(&["monospace".to_string(), "sans-serif".to_string()], 14.0)
            .expect("load font chain");
    assert!(!chain.families().is_empty());
    let key = chain.face_key('A', CellFlags::empty());
    assert_eq!(key.face, 0, "A should resolve from primary font");
}

#[test]
fn styled_fallback_caching() {
    let mut cache = FallbackCache::default();
    let regular = cache.resolve('中', 0, "monospace");
    let bold = cache.resolve('中', 1, "monospace");
    if regular.is_some() {
        assert_eq!(cache.resolved.len(), 2);
    }
    if let (Some((r_idx, _)), Some((b_idx, _))) = (regular, bold) {
        assert!(r_idx <= b_idx || r_idx == b_idx);
    }
}

#[test]
fn glyph_cache_ignores_color_attributes_and_caches_spaces() {
    let mut atlas = GlyphAtlas::new(32, 32);
    let glyph = atlas
        .get_or_insert('A', CellFlags::empty(), fonts())
        .unwrap();
    assert!(glyph.width > 0 && glyph.height > 0);
    atlas.dirty = false;
    let cached = atlas.get_or_insert('A', CellFlags::DIM | CellFlags::REVERSE, fonts());
    assert_eq!(cached, Some(glyph));
    assert!(!atlas.dirty);
    let space = atlas
        .get_or_insert(' ', CellFlags::empty(), fonts())
        .unwrap();
    assert_eq!(space.width * space.height, 0);
    assert!(!atlas.dirty);
    assert_eq!(atlas.get(' ', CellFlags::empty(), fonts()), Some(space));
}

#[test]
fn styled_glyphs_have_separate_cache_entries() {
    let mut atlas = GlyphAtlas::new(64, 64);
    for flags in [
        CellFlags::empty(),
        CellFlags::BOLD,
        CellFlags::ITALIC,
        CellFlags::BOLD | CellFlags::ITALIC,
    ] {
        assert!(atlas.get_or_insert('A', flags, fonts()).is_some());
    }
    let styled_ascii_count = atlas.ascii_cache.iter().flatten().count();
    assert_eq!(styled_ascii_count, 4);
    assert_eq!(atlas.cache.len(), 0);
}

#[test]
fn growth_preserves_glyph_coordinates_pixels_and_padding() {
    let mut atlas = GlyphAtlas::new(32, 32);
    let first = atlas
        .get_or_insert('A', CellFlags::empty(), fonts())
        .unwrap();
    assert!(
        atlas
            .get_or_insert('W', CellFlags::empty(), fonts())
            .is_some()
    );
    assert!(
        atlas
            .get_or_insert('M', CellFlags::empty(), fonts())
            .is_some()
    );
    let relooked = atlas.get('A', CellFlags::empty(), fonts()).unwrap();
    assert_eq!(first, relooked);
    let [x, y] = relooked.position;
    assert!(x > 0 && y > 0);
    assert_eq!(atlas.pixels[(y * atlas.width + x - 1) as usize], 0);
}

#[test]
fn shelf_accepts_exact_fits_and_preserves_padding() {
    let mut shelf = Shelf::default();
    let first = shelf.allocate(10, 10, [32, 32]).unwrap();
    assert_eq!(first, [1, 1]);
    assert_eq!(shelf.x, 12);
    assert_eq!(shelf.height, 12);
    let second = shelf.allocate(10, 10, [32, 32]).unwrap();
    assert_eq!(second, [13, 1]);
    assert_eq!(shelf.x, 24);
}

#[test]
fn failed_allocation_does_not_consume_space() {
    let mut shelf = Shelf::default();
    let _ = shelf.allocate(10, 10, [32, 32]).unwrap();
    let state_before = (shelf.x, shelf.y, shelf.height);
    assert!(shelf.allocate(30, 30, [32, 32]).is_none());
    assert_eq!((shelf.x, shelf.y, shelf.height), state_before);
}

#[test]
fn atlas_growth_and_full_eviction() {
    let mut atlas = GlyphAtlas::new(16, 16);
    let mut full = false;
    for c in 'A'..='Z' {
        if atlas
            .get_or_insert(c, CellFlags::empty(), fonts())
            .is_none()
        {
            full = true;
            break;
        }
    }
    if full {
        atlas.clear();
        assert!(
            atlas
                .get_or_insert('A', CellFlags::empty(), fonts())
                .is_some()
        );
    }
}

#[test]
fn full_atlas_can_be_reused_between_frames() {
    let mut atlas = GlyphAtlas::new(16, 16);
    let _ = atlas.get_or_insert('A', CellFlags::empty(), fonts());
    atlas.clear();
    assert!(atlas.cache.is_empty());
    assert_eq!(atlas.pixels.iter().sum::<u8>(), 0);
    assert!(atlas.dirty);
    assert!(
        atlas
            .get_or_insert('A', CellFlags::empty(), fonts())
            .is_some()
    );
}

#[test]
fn test_set_font_size_updates_metrics_in_memory_without_reloading() {
    let mut fonts = FontManager::load(14.0).expect("load monospace");
    let initial_width = fonts.metrics.cell_width;
    let initial_height = fonts.metrics.cell_height;

    // Scale up to 28.0 (double size)
    assert!(fonts.set_font_size(28.0));
    assert_eq!(fonts.font_size(), 28.0);
    assert!(fonts.metrics.cell_width > initial_width);
    assert!(fonts.metrics.cell_height > initial_height);

    // Setting same size returns false
    assert!(!fonts.set_font_size(28.0));

    // Invalid sizes rejected
    assert!(!fonts.set_font_size(0.0));
    assert!(!fonts.set_font_size(-5.0));
    assert!(!fonts.set_font_size(f32::NAN));
    assert!(!fonts.set_font_size(f32::INFINITY));
    assert_eq!(fonts.font_size(), 28.0);

    // Scale back down
    assert!(fonts.set_font_size(14.0));
    assert_eq!(fonts.font_size(), 14.0);
    assert_eq!(fonts.metrics.cell_width, initial_width);
    assert_eq!(fonts.metrics.cell_height, initial_height);
}

#[test]
fn fallback_cache_has_bounded_capacity() {
    let mut cache = FallbackCache::default();
    for i in 0..MAX_RESOLVED_CACHE {
        let c = char::from_u32(0x1000 + i as u32).unwrap_or('A');
        cache.resolved_order.push_back((c, 0));
        cache.resolved.insert((c, 0), None);
    }
    assert_eq!(cache.resolved.len(), MAX_RESOLVED_CACHE);
    let oldest = char::from_u32(0x1000).unwrap();
    assert!(cache.resolved.contains_key(&(oldest, 0)));

    // Resolving a new character evicts the oldest entry (FIFO)
    let _ = cache.resolve('Z', 1, "monospace");
    assert_eq!(cache.resolved.len(), MAX_RESOLVED_CACHE);
    assert!(!cache.resolved.contains_key(&(oldest, 0)));
    assert!(cache.resolved.contains_key(&('Z', 1)));
}

#[test]
fn test_cross_style_fallback_glyph_reuse() {
    let mut cache = FallbackCache::default();
    let regular = cache.resolve('中', 0, "monospace");
    let bold = cache.resolve('中', 1, "monospace");
    if regular.is_some() {
        assert_eq!(cache.resolved.len(), 2);
        assert!(bold.is_some());
    }
}

#[test]
fn test_ascii_direct_cache_consistency() {
    let fonts = fonts();
    let mut atlas = GlyphAtlas::new(256, 256);
    for flags in [
        CellFlags::empty(),
        CellFlags::BOLD,
        CellFlags::ITALIC,
        CellFlags::BOLD | CellFlags::ITALIC,
    ] {
        for c in ['A', 'z', '0', '$', ' '] {
            let inserted = atlas.get_or_insert(c, flags, fonts).expect("fit ascii");
            let retrieved = atlas.get(c, flags, fonts).expect("cached ascii");
            assert_eq!(inserted, retrieved);
        }
    }
    // Atlas clear invalidates ASCII cache
    atlas.clear();
    assert_eq!(atlas.get('A', CellFlags::empty(), fonts), None);
}

#[test]
fn test_styled_chain_and_fallback_prewarm() {
    let fonts = FontManager::load(14.0).expect("load monospace");
    assert!(fonts.metrics.cell_width > 0);
    assert!(fonts.metrics.cell_height > 0);

    for flags in [
        CellFlags::empty(),
        CellFlags::BOLD,
        CellFlags::ITALIC,
        CellFlags::BOLD | CellFlags::ITALIC,
    ] {
        let face = fonts.font_for_style(flags);
        assert!(face.horizontal_line_metrics(14.0).is_some());
        let key = fonts.face_key('A', flags);
        assert_eq!(key.glyph, face.lookup_glyph_index('A'));
    }
}

#[test]
fn test_font_cell_metrics_calculation() {
    let fonts = fonts();
    let metrics = fonts.regular.primary.compute_cell_metrics(14.0);
    assert!(metrics.cell_width >= 1);
    assert!(metrics.cell_height >= 1);
    assert!(metrics.ascent > 0);
    assert_eq!(metrics, fonts.metrics);
}

#[test]
fn test_printable_ascii_regular_glyphs_have_nonzero_dimensions() {
    let fonts = fonts();
    let mut atlas = GlyphAtlas::new(1024, 1024);
    for c in '!'..='~' {
        let glyph = atlas
            .get_or_insert(c, CellFlags::empty(), fonts)
            .unwrap_or_else(|| panic!("printable ascii '{c}' must produce a glyph"));
        assert!(
            glyph.width > 0 && glyph.height > 0,
            "printable ascii '{c}' must have non-zero dimensions"
        );
        let retrieved = atlas
            .get(c, CellFlags::empty(), fonts)
            .expect("glyph must be present in atlas");
        assert_eq!(glyph, retrieved);
    }
}

#[test]
fn test_whitespace_and_empty_glyph_safeguards() {
    let fonts = fonts();
    let mut atlas = GlyphAtlas::new(256, 256);
    // Space is whitespace: width * height is 0 and it is successfully cached
    let space = atlas
        .get_or_insert(' ', CellFlags::empty(), fonts)
        .expect("space glyph must be cached");
    assert_eq!(space.width * space.height, 0);
    assert_eq!(atlas.get(' ', CellFlags::empty(), fonts), Some(space));

    // Null char '\0' is not whitespace and has no glyph; get_or_insert must not poison ascii_cache
    let res = atlas.get_or_insert('\0', CellFlags::empty(), fonts);
    assert!(res.is_none());
    assert_eq!(atlas.ascii_cache[0], None);
}

#[test]
fn test_font_manager_initialization_is_fast() {
    let start = std::time::Instant::now();
    let font_mgr = FontManager::load(14.0).expect("load monospace font");
    let elapsed = start.elapsed();
    assert!(font_mgr.metrics.cell_width > 0);
    assert!(font_mgr.metrics.cell_height > 0);
    assert!(
        elapsed < std::time::Duration::from_millis(200),
        "FreeType initialization took {elapsed:?}"
    );
}

#[test]
fn test_subpixel_and_grayscale_rasterization() {
    let fonts_gray =
        FontManager::load_with_families_and_subpixel(&["monospace".to_string()], 14.0, false)
            .expect("load grayscale font");
    assert!(!fonts_gray.subpixel);
    let key = fonts_gray.face_key('M', CellFlags::empty());
    let gray_glyph = fonts_gray.rasterize(key);
    assert!(gray_glyph.width > 0 && gray_glyph.height > 0);
    // Grayscale pixels have identical R, G, B channels
    assert_eq!(gray_glyph.pixels[0], gray_glyph.pixels[1]);
    assert_eq!(gray_glyph.pixels[1], gray_glyph.pixels[2]);

    let fonts_lcd =
        FontManager::load_with_families_and_subpixel(&["monospace".to_string()], 14.0, true)
            .expect("load lcd subpixel font");
    assert!(fonts_lcd.subpixel);
    let lcd_glyph = fonts_lcd.rasterize(key);
    assert!(lcd_glyph.width > 0 && lcd_glyph.height > 0);
    assert!(!lcd_glyph.pixels.is_empty());
}

#[test]
fn test_srgb_optical_alpha_transfer_table() {
    use crate::font::face::LINEAR_TO_SRGB;

    // Boundaries must map identically to 0 and 255
    assert_eq!(LINEAR_TO_SRGB[0], 0);
    assert_eq!(LINEAR_TO_SRGB[255], 255);

    // Curve must be strictly non-decreasing across the entire u8 domain
    for i in 0..255 {
        assert!(
            LINEAR_TO_SRGB[i] <= LINEAR_TO_SRGB[i + 1],
            "inversion at {i}: {} > {}",
            LINEAR_TO_SRGB[i],
            LINEAR_TO_SRGB[i + 1]
        );
    }

    // Mid-tone (50% geometric coverage) must be perceptually expanded for sRGB displays (~73.5%)
    assert_eq!(LINEAR_TO_SRGB[128], 188);
    // Low-mid tone (25% coverage) must be expanded (~53.7%)
    assert_eq!(LINEAR_TO_SRGB[64], 137);

    // Rasterization must apply the sRGB table and boost anti-aliased edge coverage
    let fonts =
        FontManager::load_with_families_and_subpixel(&["monospace".to_string()], 14.0, false)
            .expect("load font");
    let key = fonts.face_key('M', CellFlags::empty());
    let glyph = fonts.rasterize(key);
    assert!(glyph.width > 0 && glyph.height > 0);
    let max_val = glyph.pixels.iter().copied().max().unwrap_or(0);
    assert!(max_val >= 250);

    // Compare raw FreeType buffer directly with LINEAR_TO_SRGB to ensure partial-coverage
    // edge pixels are transformed rather than identity-mapped.
    let fc = fontconfig().expect("fontconfig must be initialized");
    let (path, index) = match_family(fc, "monospace", false, false).expect("system monospace");
    let lib = freetype::Library::init().expect("ft init");
    let face = lib.new_face(&path, index as isize).expect("new face");
    face.set_char_size(0, (14.0 * 64.0) as isize, 72, 72)
        .expect("set size");
    face.load_glyph(
        face.get_char_index('M' as usize).unwrap_or(0),
        freetype::face::LoadFlag::RENDER | freetype::face::LoadFlag::TARGET_LIGHT,
    )
    .expect("load glyph");
    let slot = face.glyph();
    let bmp = slot.bitmap();
    let raw_buf = bmp.buffer();
    let raw_partial = raw_buf
        .iter()
        .copied()
        .find(|&b| (20..=180).contains(&b))
        .expect("glyph must contain edge pixels with partial coverage");
    let expected = LINEAR_TO_SRGB[raw_partial as usize];
    assert!(
        expected > raw_partial,
        "sRGB transfer curve must strictly expand partial coverage {raw_partial} -> {expected}"
    );
    assert!(
        glyph.pixels.contains(&expected),
        "rasterized glyph pixels must contain the boosted sRGB value {expected} from raw {raw_partial}"
    );
}

#[test]
fn test_atlas_dirty_rect_tracking_and_full_upload_reset() {
    let mut atlas = GlyphAtlas::new(128, 128);
    assert!(atlas.full_upload);
    assert!(atlas.dirty);
    assert!(atlas.dirty_rect.is_none());

    // Simulate first full upload
    atlas.full_upload = false;
    atlas.dirty = false;

    let fonts = fonts();
    // Insert first glyph
    let g1 = atlas.get_or_insert('A', CellFlags::empty(), fonts);
    assert!(g1.is_some());
    assert!(atlas.dirty);
    assert!(!atlas.full_upload);
    let rect1 = atlas
        .dirty_rect
        .expect("dirty rect must be set on glyph insertion");
    assert!(rect1[2] > rect1[0]);
    assert!(rect1[3] > rect1[1]);

    // Insert second glyph expands bounding box
    let g2 = atlas.get_or_insert('B', CellFlags::empty(), fonts);
    assert!(g2.is_some());
    let rect2 = atlas.dirty_rect.expect("dirty rect must be expanded");
    assert!(rect2[0] <= rect1[0]);
    assert!(rect2[1] <= rect1[1]);
    assert!(rect2[2] >= rect1[2]);
    assert!(rect2[3] >= rect1[3]);

    // Clearing atlas resets to full upload and clears dirty rect
    atlas.clear();
    assert!(atlas.full_upload);
    assert!(atlas.dirty);
    assert!(atlas.dirty_rect.is_none());
}
