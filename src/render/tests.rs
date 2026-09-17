use crate::color::{Color, default_256_palette};
use crate::font::{FontManager, GlyphAtlas};
use crate::grid::{Cell, CellFlags, Grid};
use crate::ime::Preedit;
use crate::render::text::{
    KITTY_PLACEHOLDER, SELECTION_BG, build_row_backgrounds, build_row_foregrounds, build_vertices,
    cell_colors, cursor_cell, prepare_atlas,
};
use crate::render::{
    ColorScheme, DEFAULT_BG, DEFAULT_FG, FRAGMENT_SHADER, HoveredHyperlinkSpan, RenderOptions,
    native_size,
};

#[test]
fn test_selection_background_color_constant() {
    assert_eq!(SELECTION_BG, [0.35, 0.45, 0.70, 0.5]);
}

fn frame(grid: &Grid) -> (Vec<f32>, GlyphAtlas) {
    let fonts = FontManager::load(14.0).expect("system monospace font");
    let mut atlas = GlyphAtlas::new(16, 16);
    prepare_atlas(grid, &fonts, &mut atlas, None);
    let mut vertices = Vec::new();
    build_vertices(
        &mut vertices,
        grid,
        ColorScheme::new(&default_256_palette(), DEFAULT_FG, DEFAULT_BG),
        fonts.metrics,
        &fonts,
        &atlas,
        RenderOptions::default(),
    );
    (vertices, atlas)
}

#[test]
fn backgrounds_precede_glyphs_and_hidden_text_is_omitted() {
    let mut grid = Grid::new(3, 1, 0);
    grid.cursor.visible = false;
    grid.lines[0].cells[0].c = 'W';
    grid.lines[0].cells[0].flags = CellFlags::WIDE_CHAR;
    grid.lines[0].cells[1].flags = CellFlags::WIDE_CHAR_SPACER;
    grid.lines[0].cells[1].bg = Color::Rgb(20, 30, 40);
    grid.lines[0].cells[2].c = 'X';
    grid.lines[0].cells[2].flags = CellFlags::HIDDEN | CellFlags::UNDERLINE;
    let (vertices, atlas) = frame(&grid);
    assert_eq!(vertices.len(), 2 * 48);
    assert!(vertices[2] < 0.0, "first quad is the background");
    assert!(vertices[48 + 2] >= 0.0, "last quad is the wide glyph");
    assert!(
        atlas.pixels.iter().any(|&pixel| pixel != 0),
        "first frame has glyph pixels"
    );
}

#[test]
fn reverse_resolves_defaults_before_swapping() {
    let cell = Cell {
        flags: CellFlags::REVERSE,
        ..Cell::default()
    };
    assert_eq!(
        cell_colors(
            &cell,
            ColorScheme::new(&default_256_palette(), DEFAULT_FG, DEFAULT_BG)
        ),
        (DEFAULT_BG, DEFAULT_FG)
    );
}

#[test]
fn unicode_placeholders_are_omitted_from_glyph_vertices() {
    let mut grid = Grid::new(2, 1, 0);
    grid.cursor.visible = false;
    grid.lines[0].cells[0].c = KITTY_PLACEHOLDER;
    grid.lines[0].cells[0].fg = Color::Rgb(0, 0, 42);
    grid.lines[0].cells[1].c = 'A';

    let (vertices, _) = frame(&grid);
    // Only one text quad for 'A', not for KITTY_PLACEHOLDER
    assert_eq!(vertices.len(), 48);
}

#[test]
fn decorations_render_on_spaces() {
    let mut grid = Grid::new(1, 1, 0);
    grid.cursor.visible = false;
    grid.lines[0].cells[0].flags = CellFlags::UNDERLINE | CellFlags::STRIKETHROUGH;
    let (vertices, _) = frame(&grid);
    assert_eq!(vertices.len(), 2 * 48);
    assert!(
        vertices
            .as_chunks::<8>()
            .0
            .iter()
            .all(|vertex| vertex[2] < 0.0)
    );
}

#[test]
fn cursor_handles_pending_wrap_wide_cells_and_visibility() {
    let mut grid = Grid::new(3, 1, 0);
    grid.cursor.col = 3;
    assert_eq!(cursor_cell(&grid), Some((0, 2, 1)));
    grid.lines[0].cells[1].flags = CellFlags::WIDE_CHAR;
    grid.lines[0].cells[2].flags = CellFlags::WIDE_CHAR_SPACER;
    assert_eq!(cursor_cell(&grid), Some((0, 1, 2)));
    grid.cursor.visible = false;
    assert_eq!(cursor_cell(&grid), None);
}

#[test]
fn cjk_cells_render_through_fallback_faces() {
    let fonts = FontManager::load(14.0).expect("system monospace font");
    if fonts.face_key('中', CellFlags::empty()).face == 0 {
        return; // No CJK-capable face is installed on this machine.
    }
    let mut grid = Grid::new(4, 1, 0);
    grid.cursor.visible = false;
    grid.lines[0].cells[0].c = '中';
    grid.lines[0].cells[0].flags = CellFlags::WIDE_CHAR;
    grid.lines[0].cells[1].flags = CellFlags::WIDE_CHAR_SPACER;

    let mut atlas = GlyphAtlas::new(64, 64);
    prepare_atlas(&grid, &fonts, &mut atlas, None);
    let mut vertices = Vec::new();
    build_vertices(
        &mut vertices,
        &grid,
        ColorScheme::new(&default_256_palette(), DEFAULT_FG, DEFAULT_BG),
        fonts.metrics,
        &fonts,
        &atlas,
        RenderOptions::default(),
    );
    assert_eq!(vertices.len(), 48, "one quad for the wide CJK glyph");
    // A rasterized glyph samples the atlas instead of using the solid placeholder.
    assert!(vertices[2] >= 0.0, "expected a rasterized CJK glyph quad");
    assert!(atlas.pixels.iter().any(|&pixel| pixel != 0));
}

#[test]
fn image_fragment_shader_preserves_rgba_channels() {
    // Regression guard: image placements must sample the uploaded RGBA texture rather
    // than reusing the alpha-only glyph path, which renders every image as a white box.
    assert!(FRAGMENT_SHADER.contains("u_image_mode"));
    assert!(FRAGMENT_SHADER.contains("texel.rgb"));
    assert!(FRAGMENT_SHADER.contains("texel.a"));
}

#[test]
fn invalid_native_dimensions_are_rejected() {
    for size in [[0, 1], [1, 0], [u32::MAX, 1], [1, u32::MAX]] {
        assert!(native_size(size).is_err());
    }
    assert_eq!(native_size([720, 480]).unwrap(), [720, 480]);
}

#[test]
fn atlas_exhaustion_renders_fallback_instead_of_dropping_text() {
    let fonts = FontManager::load(14.0).expect("system monospace font");
    let mut grid = Grid::new(1, 1, 0);
    grid.cursor.visible = false;
    grid.lines[0].cells[0].c = 'Z';

    // Empty atlas with no 'Z' cached - fallback '?' or solid quad must be generated.
    let mut atlas = GlyphAtlas::new(16, 16);
    let _ = atlas.get_or_insert('?', CellFlags::empty(), &fonts);

    let mut vertices = Vec::new();
    build_vertices(
        &mut vertices,
        &grid,
        ColorScheme::new(&default_256_palette(), DEFAULT_FG, DEFAULT_BG),
        fonts.metrics,
        &fonts,
        &atlas,
        RenderOptions::default(),
    );
    // Ensure vertices were generated for the character cell rather than dropped.
    assert_eq!(vertices.len(), 48);

    // Even with a completely empty atlas (no '?' either), a placeholder quad is generated.
    let empty_atlas = GlyphAtlas::new(16, 16);
    let mut placeholder_vertices = Vec::new();
    build_vertices(
        &mut placeholder_vertices,
        &grid,
        ColorScheme::new(&default_256_palette(), DEFAULT_FG, DEFAULT_BG),
        fonts.metrics,
        &fonts,
        &empty_atlas,
        RenderOptions::default(),
    );
    assert_eq!(placeholder_vertices.len(), 48);
    assert_eq!(placeholder_vertices[2], -1.0); // SOLID_UV placeholder
}

#[test]
fn test_padding_offsets_vertices() {
    let fonts = FontManager::load(14.0).expect("system monospace font");
    let mut grid = Grid::new(1, 1, 0);
    grid.cursor.visible = false;
    grid.lines[0].cells[0].bg = Color::Rgb(10, 20, 30);

    let atlas = GlyphAtlas::new(16, 16);
    let mut vertices = Vec::new();
    build_vertices(
        &mut vertices,
        &grid,
        ColorScheme::new(&default_256_palette(), DEFAULT_FG, DEFAULT_BG),
        fonts.metrics,
        &fonts,
        &atlas,
        RenderOptions::new([12, 18], None, None),
    );
    assert_eq!(vertices.len(), 48);
    assert_eq!(vertices[0], 12.0); // x offset by padding_x
    assert_eq!(vertices[1], 18.0); // y offset by padding_y
}

#[test]
fn test_preedit_renders_inline_at_cursor() {
    let fonts = FontManager::load(14.0).expect("system monospace font");
    let mut grid = Grid::new(20, 5, 0);
    grid.cursor.row = 1;
    grid.cursor.col = 2;

    let preedit = Preedit {
        text: "test".to_string(),
        cursor_begin: 0,
        cursor_end: 4,
    };

    let mut atlas = GlyphAtlas::new(64, 64);
    prepare_atlas(&grid, &fonts, &mut atlas, Some(&preedit));

    let mut vertices = Vec::new();
    build_vertices(
        &mut vertices,
        &grid,
        ColorScheme::new(&default_256_palette(), DEFAULT_FG, DEFAULT_BG),
        fonts.metrics,
        &fonts,
        &atlas,
        RenderOptions::new([0, 0], Some(&preedit), None),
    );

    // Vertices must contain the block cursor and the preedit quads
    assert!(vertices.len() >= 4 * 48);
}

#[test]
fn test_wide_preedit_clamped_at_last_column() {
    let fonts = FontManager::load(14.0).expect("system monospace font");
    let mut grid = Grid::new(5, 2, 0);
    grid.cursor.row = 0;
    grid.cursor.col = 4; // last column

    let preedit = Preedit {
        text: "中".to_string(), // wide char (width 2)
        cursor_begin: 0,
        cursor_end: 1,
    };

    let mut atlas = GlyphAtlas::new(64, 64);
    prepare_atlas(&grid, &fonts, &mut atlas, Some(&preedit));

    let mut vertices = Vec::new();
    build_vertices(
        &mut vertices,
        &grid,
        ColorScheme::new(&default_256_palette(), DEFAULT_FG, DEFAULT_BG),
        fonts.metrics,
        &fonts,
        &atlas,
        RenderOptions::new([10, 10], Some(&preedit), None),
    );

    let cw = fonts.metrics.cell_width as f32;
    // The preedit background quad x1 should be clamped to 10 + 5 * cw, NOT 10 + 6 * cw
    let expected_right = 10.0 + 5.0 * cw;
    // Verify no vertex in preedit quad exceeds the right edge
    for chunk in vertices.chunks(8) {
        assert!(chunk[0] <= expected_right + 0.01);
    }
}

#[test]
fn test_hovered_hyperlink_renders_underline() {
    let fonts = FontManager::load(14.0).expect("system monospace font");
    let mut grid = Grid::new(10, 2, 0);
    grid.cursor.visible = false;
    // Write character with hyperlink_id 1 at line 0, col 0
    grid.write_char_styled(
        'a',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
        Color::DefaultForeground,
        Some(1),
    );

    let mut atlas = GlyphAtlas::new(64, 64);
    prepare_atlas(&grid, &fonts, &mut atlas, None);

    let mut vertices_no_hover = Vec::new();
    build_vertices(
        &mut vertices_no_hover,
        &grid,
        ColorScheme::new(&default_256_palette(), DEFAULT_FG, DEFAULT_BG),
        fonts.metrics,
        &fonts,
        &atlas,
        RenderOptions::new([0, 0], None, None),
    );

    let mut vertices_mismatched_hover = Vec::new();
    build_vertices(
        &mut vertices_mismatched_hover,
        &grid,
        ColorScheme::new(&default_256_palette(), DEFAULT_FG, DEFAULT_BG),
        fonts.metrics,
        &fonts,
        &atlas,
        RenderOptions::new([0, 0], None, None).with_hovered_span(Some(HoveredHyperlinkSpan {
            line: 1, // different line
            start_col: 0,
            end_col: 0,
        })),
    );

    let mut vertices_matched_hover = Vec::new();
    build_vertices(
        &mut vertices_matched_hover,
        &grid,
        ColorScheme::new(&default_256_palette(), DEFAULT_FG, DEFAULT_BG),
        fonts.metrics,
        &fonts,
        &atlas,
        RenderOptions::new([0, 0], None, None).with_hovered_span(Some(HoveredHyperlinkSpan {
            line: 0,
            start_col: 0,
            end_col: 0,
        })),
    );

    // Without hover and with mismatched hover, only the character glyph quad is produced (48 f32 floats)
    assert_eq!(vertices_no_hover.len(), 48);
    assert_eq!(vertices_mismatched_hover.len(), 48);
    // With matched hover, an extra solid underline quad is produced (+48 f32 floats = 96)
    assert_eq!(vertices_matched_hover.len(), 96);
}

#[test]
fn test_hovered_hyperlink_scopes_to_span_not_entire_url() {
    let fonts = FontManager::load(14.0).expect("system monospace font");
    let mut grid = Grid::new(10, 2, 0);
    grid.cursor.visible = false;
    // Line 0: write 'x' with hyperlink 1
    grid.cursor.row = 0;
    grid.cursor.col = 0;
    grid.write_char_styled(
        'x',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
        Color::DefaultForeground,
        Some(1),
    );
    // Line 1: write 'y' with SAME hyperlink 1
    grid.cursor.row = 1;
    grid.cursor.col = 0;
    grid.write_char_styled(
        'y',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
        Color::DefaultForeground,
        Some(1),
    );

    let mut atlas = GlyphAtlas::new(64, 64);
    prepare_atlas(&grid, &fonts, &mut atlas, None);

    let mut vertices = Vec::new();
    // Hover ONLY the span on line 1
    build_vertices(
        &mut vertices,
        &grid,
        ColorScheme::new(&default_256_palette(), DEFAULT_FG, DEFAULT_BG),
        fonts.metrics,
        &fonts,
        &atlas,
        RenderOptions::new([0, 0], None, None).with_hovered_span(Some(HoveredHyperlinkSpan {
            line: 1,
            start_col: 0,
            end_col: 0,
        })),
    );

    // 2 characters (48 floats each) + 1 underline quad for line 1 only (48 floats) = 144 floats.
    // If line 0 had also been underlined due to identical URL ID, it would be 192 floats.
    assert_eq!(vertices.len(), 144);
}

#[test]
fn test_procedural_box_and_block_glyphs_bypass_atlas_and_render_solid() {
    let fonts = FontManager::load(14.0).expect("system monospace font");
    let mut grid = Grid::new(4, 1, 0);
    grid.cursor.visible = false;
    // Cell 0: Full block '█' (U+2588)
    grid.write_char(
        '█',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    // Cell 1: Full block '█' (U+2588)
    grid.write_char(
        '█',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    // Cell 2: Box horizontal '─' (U+2500)
    grid.write_char(
        '─',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    // Cell 3: Box horizontal '─' (U+2500)
    grid.write_char(
        '─',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );

    let mut atlas = GlyphAtlas::new(64, 64);
    prepare_atlas(&grid, &fonts, &mut atlas, None);

    // Procedural glyphs must NOT be inserted into atlas
    assert!(atlas.get('█', CellFlags::empty(), &fonts).is_none());
    assert!(atlas.get('─', CellFlags::empty(), &fonts).is_none());

    let mut vertices = Vec::new();
    build_vertices(
        &mut vertices,
        &grid,
        ColorScheme::new(&default_256_palette(), DEFAULT_FG, DEFAULT_BG),
        fonts.metrics,
        &fonts,
        &atlas,
        RenderOptions::default(),
    );

    let cw = fonts.metrics.cell_width as f32;
    let ch = fonts.metrics.cell_height as f32;

    // 4 glyph quads generated (each 48 floats)
    assert_eq!(vertices.len(), 4 * 48);

    // Verify cell 0 (█) covers full height from 0 to ch
    assert_eq!(vertices[1], 0.0);
    assert_eq!(vertices[33], ch);

    // Verify all 4 procedural glyphs use SOLID_UV (-1.0)
    for i in 0..4 {
        let offset = i * 48;
        assert_eq!(vertices[offset + 2], -1.0); // u
        assert_eq!(vertices[offset + 3], -1.0); // v
    }

    // Verify cell 0 (█) and cell 1 (█) touch edge-to-edge with zero gap
    let cell0_right = vertices[8]; // x1 of cell 0
    let cell1_left = vertices[48]; // x0 of cell 1
    assert_eq!(cell0_right, cell1_left);
    assert_eq!(cell0_right, cw);

    // Verify cell 2 (─) and cell 3 (─) touch edge-to-edge with zero gap
    let cell2_right = vertices[2 * 48 + 8]; // x1 of cell 2
    let cell3_left = vertices[3 * 48]; // x0 of cell 3
    assert_eq!(cell2_right, cell3_left);
    assert_eq!(cell2_right, 3.0 * cw);
}

#[test]
fn test_incremental_dirty_tracking_only_regenerates_modified_row() {
    let fonts = FontManager::load(14.0).expect("system monospace font");
    let mut grid = Grid::new(10, 5, 0);
    grid.cursor.visible = false;

    // Write to row 1
    grid.cursor.row = 1;
    grid.write_char(
        'A',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );

    let atlas = GlyphAtlas::new(32, 32);
    let palette = default_256_palette();
    let colors = ColorScheme::new(&palette, DEFAULT_FG, DEFAULT_BG);

    let mut row_bg = vec![Vec::new(); 5];
    let mut row_fg = vec![Vec::new(); 5];
    let ctx = crate::render::text::RenderContext {
        grid: &grid,
        colors,
        metrics: fonts.metrics,
        fonts: &fonts,
        atlas: &atlas,
        options: RenderOptions::default(),
        cursor: None,
    };

    // Initially, all rows generated and cleared
    for (r, (bg, fg)) in row_bg.iter_mut().zip(row_fg.iter_mut()).enumerate() {
        let line = grid.visible_line(r);
        build_row_backgrounds(bg, r, &ctx);
        build_row_foregrounds(fg, r, &ctx);
        line.dirty.set(false);
    }

    // All rows now have dirty == false
    for r in 0..5 {
        assert!(!grid.visible_line(r).dirty.get());
    }

    // Modify ONLY row 3
    grid.cursor.row = 3;
    grid.write_char(
        'B',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );

    // Assert ONLY row 3 became dirty!
    assert!(!grid.visible_line(0).dirty.get());
    assert!(!grid.visible_line(1).dirty.get());
    assert!(!grid.visible_line(2).dirty.get());
    assert!(grid.visible_line(3).dirty.get());
    assert!(!grid.visible_line(4).dirty.get());
}
