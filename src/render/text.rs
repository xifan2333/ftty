//! Cell-grid to vertex generation: backgrounds, glyphs, and dynamic overlays.

use crate::color::{Color, Rgb};
use crate::font::atlas::CachedGlyph;
use crate::font::{CellMetrics, FontManager, GlyphAtlas};
use crate::grid::{Cell, CellFlags, CursorShape, Grid, Row};
use crate::input::ime::Preedit;
use crate::width::KITTY_PLACEHOLDER;

use super::colors::{ColorScheme, RenderOptions};
use super::procedural::{SOLID_UV, is_procedural_glyph, render_procedural_glyph};

pub const SELECTION_BG: [f32; 4] = [0.35, 0.45, 0.70, 0.5];

pub(crate) fn visible_glyph(cell: &Cell) -> bool {
    cell.c != ' '
        && cell.c != KITTY_PLACEHOLDER
        && !cell
            .flags
            .intersects(CellFlags::HIDDEN | CellFlags::WIDE_CHAR_SPACER | CellFlags::WRAP_SPACER)
}

/// Formatting code points that should never be rasterized on their own.
fn is_cluster_format(c: char) -> bool {
    matches!(c, '\u{200D}' | '\u{FE0E}' | '\u{FE0F}')
        || ('\u{E0020}'..='\u{E007F}').contains(&c)
        || ('\u{1F3FB}'..='\u{1F3FF}').contains(&c)
}

/// Symbols that participate in programming ligatures (Fira Code–style). Letters,
/// digits, and space act as run boundaries so `a==b` still ligates the `==`.
fn is_ligature_symbol(c: char) -> bool {
    matches!(
        c,
        '=' | '+'
            | '-'
            | '<'
            | '>'
            | '|'
            | '&'
            | '*'
            | '/'
            | ':'
            | '!'
            | '~'
            | '^'
            | '%'
            | '#'
            | '$'
            | '@'
            | '\\'
            | '.'
            | '_'
            | '?'
            | ';'
    )
}

/// A single cell eligible to participate in an ASCII ligature run: visible,
/// width-1 ligature symbol, and no existing grapheme cluster.
fn is_ligature_candidate(cell: &Cell, line: &Row, col: usize) -> bool {
    if cell
        .flags
        .intersects(CellFlags::HIDDEN | CellFlags::WIDE_CHAR | CellFlags::WIDE_CHAR_SPACER)
    {
        return false;
    }
    is_ligature_symbol(cell.c) && line.cluster_text(col).is_none()
}

/// Returns the length of the contiguous run of cells starting at `col` that is
/// eligible for ASCII ligature shaping: same style (fg/flags/hyperlink), width-1
/// ligature symbols with no existing cluster. Runs shorter than two cells return
/// `None`. Single-glyph shaping is still required to accept the run; fonts
/// without a matching GSUB ligature simply fall back to per-cell quads.
pub(crate) fn ascii_ligature_run(line: &Row, col: usize) -> Option<usize> {
    let cells = &line.cells;
    let first = cells.get(col)?;
    if !is_ligature_candidate(first, line, col) {
        return None;
    }
    let mut end = col + 1;
    while end < cells.len() {
        let cell = &cells[end];
        if !is_ligature_candidate(cell, line, end)
            || cell.fg != first.fg
            || cell.flags != first.flags
            || cell.hyperlink_id != first.hyperlink_id
        {
            break;
        }
        end += 1;
    }
    (end - col >= 2).then_some(end - col)
}

/// Concatenates the cell characters of a ligature run into one cluster string.
pub(crate) fn ligature_text(line: &Row, col: usize, len: usize) -> String {
    line.cells[col..col + len].iter().map(|c| c.c).collect()
}

fn prepare_preedit_glyphs(p: &Preedit, fonts: &FontManager, atlas: &mut GlyphAtlas) -> bool {
    let mut full = false;
    for c in p.text.chars() {
        if !is_procedural_glyph(c) {
            full |= atlas
                .get_or_insert(c, CellFlags::UNDERLINE, fonts)
                .is_none();
        }
    }
    full
}

fn prepare_cell_glyphs(
    cell: &Cell,
    cluster: Option<&str>,
    fonts: &FontManager,
    atlas: &mut GlyphAtlas,
) -> bool {
    if !visible_glyph(cell) {
        return false;
    }
    let mut full = false;
    if let Some(cluster_text) = cluster {
        if atlas
            .get_or_insert_cluster(cluster_text, cell.flags, fonts)
            .is_some()
        {
            return false;
        }
        for extra in cluster_text.chars().skip(1) {
            if !is_cluster_format(extra) {
                full |= atlas.get_or_insert(extra, cell.flags, fonts).is_none();
            }
        }
    }
    if !is_procedural_glyph(cell.c) {
        full |= atlas.get_or_insert(cell.c, cell.flags, fonts).is_none();
    }
    full
}

fn prepare_row_glyphs(
    grid: &Grid,
    row: usize,
    attempt: usize,
    repacked: bool,
    fonts: &FontManager,
    atlas: &mut GlyphAtlas,
) -> bool {
    let line = grid.visible_line(row);
    if !line.dirty.get() && attempt == 0 && !repacked {
        return false;
    }
    let mut full = false;
    let mut col = 0;
    while col < line.cells.len() {
        let cell = &line.cells[col];
        if let Some(len) = ascii_ligature_run(line, col) {
            let text = ligature_text(line, col, len);
            if atlas
                .get_or_insert_cluster(&text, cell.flags, fonts)
                .is_some()
            {
                col += len;
                continue;
            }
        }
        full |= prepare_cell_glyphs(cell, line.cluster_text(col), fonts, atlas);
        col += 1;
    }
    full
}

fn prepare_grid_rows(
    grid: &Grid,
    attempt: usize,
    repacked: bool,
    fonts: &FontManager,
    atlas: &mut GlyphAtlas,
) -> bool {
    let mut full = false;
    for row in 0..grid.rows {
        full |= prepare_row_glyphs(grid, row, attempt, repacked, fonts, atlas);
    }
    full
}

pub fn prepare_atlas(
    grid: &Grid,
    fonts: &FontManager,
    atlas: &mut GlyphAtlas,
    preedit: Option<&Preedit>,
) -> bool {
    let mut repacked = false;
    for attempt in 0..2 {
        let mut full = false;
        // Pre-cache fallback glyph '?' so it is guaranteed available if the atlas fills.
        let _ = atlas.get_or_insert('?', CellFlags::empty(), fonts);
        if let Some(p) = preedit {
            full |= prepare_preedit_glyphs(p, fonts, atlas);
        }
        full |= prepare_grid_rows(grid, attempt, repacked, fonts, atlas);
        if !full || attempt == 1 {
            break;
        }
        // Repack only at a frame boundary, before generating any vertices or uploading pixels.
        atlas.clear();
        repacked = true;
    }
    repacked
}

pub fn rgba(color: Rgb) -> [f32; 4] {
    [
        f32::from(color.r) / 255.0,
        f32::from(color.g) / 255.0,
        f32::from(color.b) / 255.0,
        1.0,
    ]
}

pub fn cell_colors(cell: &Cell, colors: ColorScheme<'_>) -> (Rgb, Rgb) {
    let fg = cell
        .fg
        .to_rgb(colors.palette, colors.foreground, colors.background);
    let bg = cell
        .bg
        .to_rgb(colors.palette, colors.foreground, colors.background);
    if cell.flags.contains(CellFlags::REVERSE) {
        (bg, fg)
    } else {
        (fg, bg)
    }
}

pub(crate) fn push_quad(
    vertices: &mut Vec<f32>,
    [x0, y0, x1, y1]: [f32; 4],
    [[u0, v0], [u1, v1]]: [[f32; 2]; 2],
    [r, g, b, a]: [f32; 4],
) {
    vertices.extend_from_slice(&[
        x0, y0, u0, v0, r, g, b, a, // vertex 0
        x1, y0, u1, v0, r, g, b, a, // vertex 1
        x0, y1, u0, v1, r, g, b, a, // vertex 2
        x1, y0, u1, v0, r, g, b, a, // vertex 3
        x1, y1, u1, v1, r, g, b, a, // vertex 4
        x0, y1, u0, v1, r, g, b, a, // vertex 5
    ]);
}

pub fn cursor_cell(grid: &Grid) -> Option<(usize, usize, usize)> {
    if !grid.cursor.visible || grid.cursor.row >= grid.rows {
        return None;
    }
    // If scrolled back into history, only display cursor if its row is still in the visible viewport
    let row = if grid.viewport_offset == 0 {
        grid.cursor.row
    } else if grid.cursor.row + grid.viewport_offset < grid.rows {
        grid.cursor.row + grid.viewport_offset
    } else {
        return None;
    };

    // The grid keeps col == cols while a wrap is pending; display the cursor at the edge.
    let mut col = grid.cursor.col.min(grid.cols - 1);
    let line = grid.visible_line(row);
    if col > 0
        && col < line.cells.len()
        && line.cells[col].flags.contains(CellFlags::WIDE_CHAR_SPACER)
    {
        col -= 1;
    }
    let width = if col < line.cells.len() && line.cells[col].flags.contains(CellFlags::WIDE_CHAR) {
        2.min(grid.cols - col)
    } else {
        1
    };
    Some((row, col, width))
}

pub struct RenderContext<'a> {
    pub grid: &'a Grid,
    pub colors: ColorScheme<'a>,
    pub metrics: CellMetrics,
    pub fonts: &'a FontManager,
    pub atlas: &'a GlyphAtlas,
    pub options: RenderOptions<'a>,
    pub cursor: Option<(usize, usize, usize)>,
}

fn find_bg_span_end(cells: &[Cell], mut col: usize, bg: Rgb, colors: ColorScheme<'_>) -> usize {
    while col < cells.len() {
        let (_, next_bg) = cell_colors(&cells[col], colors);
        if next_bg != bg {
            break;
        }
        col += 1;
    }
    col
}

fn build_merged_background_spans(
    vertices: &mut Vec<f32>,
    line: &Row,
    colors: ColorScheme<'_>,
    pad_x: f32,
    y: f32,
    cw: f32,
    ch: f32,
) {
    let mut col = 0;
    while col < line.cells.len() {
        let (_, bg) = cell_colors(&line.cells[col], colors);
        if bg == colors.background {
            col += 1;
            continue;
        }
        let start_col = col;
        col = find_bg_span_end(&line.cells, col + 1, bg, colors);
        let sx = pad_x + start_col as f32 * cw;
        let ex = pad_x + col as f32 * cw;
        push_quad(vertices, [sx, y, ex, y + ch], SOLID_UV, rgba(bg));
    }
}

pub fn build_row_backgrounds(vertices: &mut Vec<f32>, row: usize, ctx: &RenderContext<'_>) {
    vertices.clear();
    let grid = ctx.grid;
    let colors = ctx.colors;
    let metrics = ctx.metrics;
    let options = ctx.options;
    let cursor = ctx.cursor;

    let cw = metrics.cell_width as f32;
    let ch = metrics.cell_height as f32;
    let pad_x = f32::from(options.padding[0]);
    let pad_y = f32::from(options.padding[1]);

    let abs_line = grid.scrollback.len() + row - grid.viewport_offset();
    let line = grid.visible_line(row);

    let has_selection = options.selection.is_some_and(|s| s.spans_line(abs_line));
    let has_block_cursor =
        cursor.is_some_and(|(r, _, _)| r == row && grid.cursor.shape == CursorShape::Block);
    let has_custom_bg = line
        .cells
        .iter()
        .any(|c| c.bg != Color::DefaultBackground || c.flags.contains(CellFlags::REVERSE));

    if !has_selection && !has_block_cursor && !has_custom_bg {
        return;
    }

    let y = pad_y + row as f32 * ch;

    // Draw background and selection on this row with contiguous span merging
    build_merged_background_spans(vertices, line, colors, pad_x, y, cw, ch);
    if let Some(selection) = options.selection
        && let Some((start_col, end_col)) = selection.line_span(abs_line, grid.cols)
    {
        let sx = pad_x + start_col as f32 * cw;
        let ex = pad_x + (end_col + 1) as f32 * cw;
        push_quad(vertices, [sx, y, ex, y + ch], SOLID_UV, SELECTION_BG);
    }

    // Draw block cursor on this row if present
    if let Some((r, col, width)) = cursor
        && r == row
        && grid.cursor.shape == CursorShape::Block
    {
        let x = pad_x + col as f32 * cw;
        push_quad(
            vertices,
            [x, y, x + width as f32 * cw, y + ch],
            SOLID_UV,
            rgba(colors.foreground),
        );
    }
}

#[derive(Clone, Copy)]
struct CellBox {
    x: f32,
    y: f32,
    width: f32,
    ch: f32,
    cw: f32,
}

#[derive(Clone, Copy)]
struct ViewportLayout {
    pad_x: f32,
    pad_y: f32,
    cw: f32,
    ch: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnderlineStyle {
    Double,
    Curly,
    Dotted,
    Dashed,
    Single,
}

impl UnderlineStyle {
    fn from_flags(flags: CellFlags) -> Self {
        if flags.contains(CellFlags::UNDERLINE_DOUBLE) {
            return Self::Double;
        }
        if flags.contains(CellFlags::UNDERLINE_CURLY) {
            return Self::Curly;
        }
        if flags.contains(CellFlags::UNDERLINE_DOTTED) {
            return Self::Dotted;
        }
        if flags.contains(CellFlags::UNDERLINE_DASHED) {
            return Self::Dashed;
        }
        Self::Single
    }
}

fn cell_foreground_color(cell: &Cell, colors: ColorScheme<'_>, under_block: bool) -> [f32; 4] {
    let (fg, _) = cell_colors(cell, colors);
    let mut color = rgba(if under_block { colors.background } else { fg });
    if cell.flags.contains(CellFlags::DIM) {
        color[3] = 0.6;
    }
    color
}

fn emit_positioned_glyph(
    vertices: &mut Vec<f32>,
    glyph: &CachedGlyph,
    cbox: CellBox,
    color: [f32; 4],
    ascent: f32,
) {
    let (gx, gy) = if glyph.colored {
        let pad_x = (cbox.width - glyph.width as f32).max(0.0) / 2.0;
        let pad_y = (cbox.ch - glyph.height as f32).max(0.0) / 2.0;
        ((cbox.x + pad_x).round(), (cbox.y + pad_y).round())
    } else {
        let gx = (cbox.x + glyph.offset_x as f32).round();
        let gy = (cbox.y + ascent - glyph.offset_y as f32 - glyph.height as f32).round();
        (gx, gy)
    };
    let [u, v] = glyph.position.map(|value| value as f32);
    let w = glyph.width as f32;
    let h = glyph.height as f32;
    let glyph_color = if glyph.colored {
        [color[0], color[1], color[2], -color[3].abs().max(0.001)]
    } else {
        color
    };
    push_quad(
        vertices,
        [gx, gy, gx + w, gy + h],
        [[u, v], [u + w, v + h]],
        glyph_color,
    );
}

fn emit_exhausted_placeholder(vertices: &mut Vec<f32>, cbox: CellBox, color: [f32; 4]) {
    let box_top = cbox.y + 2.0;
    let box_bot = cbox.y + cbox.ch - 2.0;
    if box_bot > box_top {
        push_quad(
            vertices,
            [cbox.x + 1.0, box_top, cbox.x + cbox.cw - 1.0, box_bot],
            SOLID_UV,
            [color[0], color[1], color[2], color[3] * 0.5],
        );
    }
}

fn emit_glyph_quad(
    vertices: &mut Vec<f32>,
    cell: &Cell,
    cbox: CellBox,
    color: [f32; 4],
    ctx: &RenderContext<'_>,
) {
    if is_procedural_glyph(cell.c)
        && render_procedural_glyph(vertices, cell.c, cbox.x, cbox.y, cbox.width, cbox.ch, color)
    {
        return;
    }
    if let Some(glyph) = ctx.atlas.get(cell.c, cell.flags, ctx.fonts) {
        if glyph.width > 0 && glyph.height > 0 {
            emit_positioned_glyph(vertices, &glyph, cbox, color, ctx.metrics.ascent as f32);
        }
    } else if let Some(fallback) = ctx.atlas.get('?', CellFlags::empty(), ctx.fonts) {
        // Fallback to '?' when the primary character does not fit in the atlas.
        if fallback.width > 0 && fallback.height > 0 {
            emit_positioned_glyph(vertices, &fallback, cbox, color, ctx.metrics.ascent as f32);
        }
    } else {
        // Solid placeholder quad if the atlas is entirely exhausted.
        emit_exhausted_placeholder(vertices, cbox, color);
    }
}

fn emit_cluster_glyph(
    vertices: &mut Vec<f32>,
    cluster: &str,
    cbox: CellBox,
    color: [f32; 4],
    ctx: &RenderContext<'_>,
) -> bool {
    let Some(glyph) = ctx.atlas.get_cluster(cluster) else {
        return false;
    };
    if glyph.width == 0 || glyph.height == 0 {
        return false;
    }
    emit_positioned_glyph(vertices, &glyph, cbox, color, ctx.metrics.ascent as f32);
    true
}

fn emit_cluster_extras(
    vertices: &mut Vec<f32>,
    cluster: &str,
    cell: &Cell,
    cbox: CellBox,
    color: [f32; 4],
    ctx: &RenderContext<'_>,
) {
    for extra in cluster.chars().skip(1) {
        if is_cluster_format(extra) {
            continue;
        }
        let extra_width = crate::width::wcwidth(extra, ctx.grid.ambiguous_width);
        if extra_width >= 2 {
            continue;
        }
        let Some(glyph) = ctx.atlas.get(extra, cell.flags, ctx.fonts) else {
            continue;
        };
        if glyph.width == 0 || glyph.height == 0 {
            continue;
        }
        let origin = if extra_width <= 0 {
            cbox.x + cbox.width * 0.5
        } else {
            cbox.x + cbox.cw
        };
        let gx = (origin + glyph.offset_x as f32).round();
        let gy = (cbox.y + ctx.metrics.ascent as f32 - glyph.offset_y as f32 - glyph.height as f32)
            .round();
        let [u, v] = glyph.position.map(|value| value as f32);
        let w = glyph.width as f32;
        let h = glyph.height as f32;
        push_quad(
            vertices,
            [gx, gy, gx + w, gy + h],
            [[u, v], [u + w, v + h]],
            color,
        );
    }
}

fn emit_curly_underline(vertices: &mut Vec<f32>, cbox: CellBox, base_top: f32, ul_color: [f32; 4]) {
    let steps = (cbox.width * 2.0).round().max(4.0) as usize;
    let step_w = cbox.width / steps as f32;
    let amplitude = 1.5_f32;
    let period = cbox.cw.max(4.0);
    for step in 0..steps {
        let seg_x = cbox.x + step as f32 * step_w;
        let wave = ((seg_x - cbox.x) / period * std::f32::consts::TAU).sin() * amplitude;
        let seg_y = (base_top + wave).clamp(cbox.y, cbox.y + cbox.ch - 1.0);
        push_quad(
            vertices,
            [seg_x, seg_y, seg_x + step_w, seg_y + 1.0],
            SOLID_UV,
            ul_color,
        );
    }
}

fn emit_dotted_underline(
    vertices: &mut Vec<f32>,
    cbox: CellBox,
    base_top: f32,
    ul_color: [f32; 4],
) {
    let dot_size = 2.0_f32;
    let mut dot_x = cbox.x;
    while dot_x < cbox.x + cbox.width {
        let cur_w = dot_size.min(cbox.x + cbox.width - dot_x);
        push_quad(
            vertices,
            [dot_x, base_top, dot_x + cur_w, base_top + 1.0],
            SOLID_UV,
            ul_color,
        );
        dot_x += dot_size * 2.0;
    }
}

fn emit_dashed_underline(
    vertices: &mut Vec<f32>,
    cbox: CellBox,
    base_top: f32,
    ul_color: [f32; 4],
) {
    let dash_len = 4.0_f32;
    let gap = 3.0_f32;
    let mut dash_x = cbox.x;
    while dash_x < cbox.x + cbox.width {
        let cur_w = dash_len.min(cbox.x + cbox.width - dash_x);
        push_quad(
            vertices,
            [dash_x, base_top, dash_x + cur_w, base_top + 1.0],
            SOLID_UV,
            ul_color,
        );
        dash_x += dash_len + gap;
    }
}

fn emit_underline_style(
    vertices: &mut Vec<f32>,
    flags: CellFlags,
    cbox: CellBox,
    ascent: f32,
    ul_color: [f32; 4],
) {
    let base_top = cbox.y + (ascent + 1.0).min(cbox.ch - 1.0);

    match UnderlineStyle::from_flags(flags) {
        UnderlineStyle::Double => {
            let top1 = cbox.y + ascent.min(cbox.ch - 3.0);
            let top2 = top1 + 2.0;
            push_quad(
                vertices,
                [cbox.x, top1, cbox.x + cbox.width, top1 + 1.0],
                SOLID_UV,
                ul_color,
            );
            push_quad(
                vertices,
                [cbox.x, top2, cbox.x + cbox.width, top2 + 1.0],
                SOLID_UV,
                ul_color,
            );
        }
        UnderlineStyle::Curly => emit_curly_underline(vertices, cbox, base_top, ul_color),
        UnderlineStyle::Dotted => emit_dotted_underline(vertices, cbox, base_top, ul_color),
        UnderlineStyle::Dashed => emit_dashed_underline(vertices, cbox, base_top, ul_color),
        UnderlineStyle::Single => {
            push_quad(
                vertices,
                [cbox.x, base_top, cbox.x + cbox.width, base_top + 1.0],
                SOLID_UV,
                ul_color,
            );
        }
    }
}

fn emit_decorations(
    vertices: &mut Vec<f32>,
    cell: &Cell,
    col: usize,
    abs_line: usize,
    cbox: CellBox,
    color: [f32; 4],
    ctx: &RenderContext<'_>,
) {
    let has_explicit_underline = cell.flags.contains(CellFlags::UNDERLINE);
    let has_hover_underline = (cell.hyperlink_id.is_some() || cell.c != ' ')
        && ctx.options.hovered_span.is_some_and(|span| {
            span.line == abs_line && col >= span.start_col && col <= span.end_col
        });
    if has_explicit_underline || has_hover_underline {
        let ul_color = if cell.underline_color != Color::DefaultForeground {
            let resolved = cell.underline_color.to_rgb(
                ctx.colors.palette,
                ctx.colors.foreground,
                ctx.colors.background,
            );
            rgba(resolved)
        } else {
            color
        };
        emit_underline_style(
            vertices,
            cell.flags,
            cbox,
            ctx.metrics.ascent as f32,
            ul_color,
        );
    }
    if cell.flags.contains(CellFlags::STRIKETHROUGH) {
        let top = cbox.y + (ctx.metrics.ascent as f32 * 0.65).floor();
        push_quad(
            vertices,
            [cbox.x, top, cbox.x + cbox.width, top + 1.0],
            SOLID_UV,
            color,
        );
    }
}

fn emit_cell_foreground(
    vertices: &mut Vec<f32>,
    cell: &Cell,
    cluster: Option<&str>,
    cbox: CellBox,
    color: [f32; 4],
    ctx: &RenderContext<'_>,
) {
    let cluster_rendered = if let Some(text) = cluster {
        emit_cluster_glyph(vertices, text, cbox, color, ctx)
    } else {
        false
    };

    if !cluster_rendered && visible_glyph(cell) {
        emit_glyph_quad(vertices, cell, cbox, color, ctx);
        if let Some(text) = cluster {
            emit_cluster_extras(vertices, text, cell, cbox, color, ctx);
        }
    }
}

fn emit_run_decorations(
    vertices: &mut Vec<f32>,
    line: &Row,
    row: usize,
    col: usize,
    len: usize,
    color: [f32; 4],
    ctx: &RenderContext<'_>,
) {
    let cw = ctx.metrics.cell_width as f32;
    let ch = ctx.metrics.cell_height as f32;
    let pad_x = f32::from(ctx.options.padding[0]);
    let y = f32::from(ctx.options.padding[1]) + row as f32 * ch;
    let abs_line = ctx.grid.scrollback.len() + row - ctx.grid.viewport_offset();
    for (offset, cell) in line.cells[col..col + len].iter().enumerate() {
        let c = col + offset;
        emit_decorations(
            vertices,
            cell,
            c,
            abs_line,
            CellBox {
                x: pad_x + c as f32 * cw,
                y,
                width: cw,
                ch,
                cw,
            },
            color,
            ctx,
        );
    }
}

pub fn build_row_foregrounds(vertices: &mut Vec<f32>, row: usize, ctx: &RenderContext<'_>) {
    vertices.clear();
    let cw = ctx.metrics.cell_width as f32;
    let ch = ctx.metrics.cell_height as f32;
    let pad_x = f32::from(ctx.options.padding[0]);
    let pad_y = f32::from(ctx.options.padding[1]);

    let abs_line = ctx.grid.scrollback.len() + row - ctx.grid.viewport_offset();
    let line = ctx.grid.visible_line(row);
    let y = pad_y + row as f32 * ch;

    // Draw text glyphs and underlines on this row
    let mut col = 0;
    while col < line.cells.len() {
        let cell = &line.cells[col];
        if cell
            .flags
            .intersects(CellFlags::HIDDEN | CellFlags::WIDE_CHAR_SPACER | CellFlags::WRAP_SPACER)
        {
            col += 1;
            continue;
        }
        let x = pad_x + col as f32 * cw;
        let under_block = ctx.grid.cursor.shape == CursorShape::Block
            && ctx
                .cursor
                .is_some_and(|(r, c, width)| row == r && col >= c && col < c + width);
        let color = cell_foreground_color(cell, ctx.colors, under_block);
        if let Some(len) = ascii_ligature_run(line, col) {
            let text = ligature_text(line, col, len);
            let lig_cbox = CellBox {
                x,
                y,
                width: len as f32 * cw,
                ch,
                cw,
            };
            if emit_cluster_glyph(vertices, &text, lig_cbox, color, ctx) {
                // The run rendered as one ligature glyph covering `len` columns.
                // Decorations still follow the per-cell grid.
                emit_run_decorations(vertices, line, row, col, len, color, ctx);
                col += len;
                continue;
            }
        }
        let width = if cell.flags.contains(CellFlags::WIDE_CHAR) {
            2.0 * cw
        } else {
            cw
        };
        let cbox = CellBox {
            x,
            y,
            width,
            ch,
            cw,
        };

        emit_cell_foreground(vertices, cell, line.cluster_text(col), cbox, color, ctx);
        emit_decorations(vertices, cell, col, abs_line, cbox, color, ctx);
        col += 1;
    }
}

fn emit_cursor_overlay(
    vertices: &mut Vec<f32>,
    layout: ViewportLayout,
    grid: &Grid,
    colors: ColorScheme<'_>,
    cursor: (usize, usize, usize),
) {
    let (row, col, width) = cursor;
    let x = layout.pad_x + col as f32 * layout.cw;
    let y = layout.pad_y + row as f32 * layout.ch;
    let rect = match grid.cursor.shape {
        CursorShape::Block => [x, y, x + width as f32 * layout.cw, y + layout.ch],
        CursorShape::Beam => [x, y, x + 2.0_f32.min(layout.cw), y + layout.ch],
        CursorShape::Underline => [
            x,
            y + (layout.ch - 2.0).max(0.0),
            x + width as f32 * layout.cw,
            y + layout.ch,
        ],
    };
    if grid.cursor.shape != CursorShape::Block {
        push_quad(vertices, rect, SOLID_UV, rgba(colors.foreground));
    }
}

fn emit_preedit_char(
    vertices: &mut Vec<f32>,
    c: char,
    (px, py, span_w): (f32, f32, f32),
    layout: ViewportLayout,
    fg_color: [f32; 4],
    ctx: &RenderContext<'_>,
) {
    // Draw preedit cell background
    push_quad(
        vertices,
        [px, py, px + span_w, py + layout.ch],
        SOLID_UV,
        [0.2, 0.25, 0.35, 0.95],
    );

    // Draw preedit glyph
    let span_right = px + span_w;
    if render_procedural_glyph(vertices, c, px, py, span_w, layout.ch, fg_color) {
        // Procedural box drawing / block elements glyph
    } else if let Some(glyph) = ctx.atlas.get(c, CellFlags::UNDERLINE, ctx.fonts)
        && glyph.width > 0
        && glyph.height > 0
    {
        let gx = px + glyph.offset_x as f32;
        let gy = py + ctx.metrics.ascent as f32 - glyph.offset_y as f32 - glyph.height as f32;
        let [u, v] = glyph.position.map(|value| value as f32);
        let w = glyph.width as f32;
        let h = glyph.height as f32;
        let left = gx.max(px);
        let right = (gx + w).min(span_right);
        if right > left {
            let u_left = u + (left - gx);
            let u_right = u + (right - gx);
            push_quad(
                vertices,
                [left, gy, right, gy + h],
                [[u_left, v], [u_right, v + h]],
                fg_color,
            );
        }
    }

    // Draw preedit underline
    let top = py + (ctx.metrics.ascent as f32 + 1.0).min(layout.ch - 1.0);
    push_quad(
        vertices,
        [px, top, px + span_w, top + 1.0],
        SOLID_UV,
        fg_color,
    );
}

fn emit_preedit_overlay(
    vertices: &mut Vec<f32>,
    preedit: &Preedit,
    layout: ViewportLayout,
    ctx: &RenderContext<'_>,
) {
    let grid = ctx.grid;
    let crow_opt = if grid.viewport_offset == 0 {
        Some(grid.cursor.row)
    } else if grid.cursor.row + grid.viewport_offset < grid.rows {
        Some(grid.cursor.row + grid.viewport_offset)
    } else {
        None
    };
    let Some(crow) = crow_opt else {
        return;
    };

    let mut ccol = grid.cursor.col.min(grid.cols.saturating_sub(1));
    let line = grid.visible_line(crow);
    if ccol > 0
        && ccol < line.cells.len()
        && line.cells[ccol].flags.contains(CellFlags::WIDE_CHAR_SPACER)
    {
        ccol -= 1;
    }
    let mut cur_col = ccol;
    let fg_color = rgba(ctx.colors.foreground);

    for c in preedit.text.chars() {
        let remaining_cols = grid.cols.saturating_sub(cur_col);
        if remaining_cols == 0 {
            break;
        }
        let char_width = crate::width::wcwidth(c, grid.ambiguous_width).clamp(0, 2) as usize;
        let visible_cols = char_width.min(remaining_cols);
        let px = layout.pad_x + cur_col as f32 * layout.cw;
        let py = layout.pad_y + crow as f32 * layout.ch;
        let span_w = visible_cols as f32 * layout.cw;

        emit_preedit_char(vertices, c, (px, py, span_w), layout, fg_color, ctx);

        cur_col += char_width;
    }
}

pub fn build_dynamic_overlays(vertices: &mut Vec<f32>, ctx: &RenderContext<'_>) {
    let layout = ViewportLayout {
        pad_x: f32::from(ctx.options.padding[0]),
        pad_y: f32::from(ctx.options.padding[1]),
        cw: ctx.metrics.cell_width as f32,
        ch: ctx.metrics.cell_height as f32,
    };

    if let Some(cursor) = ctx.cursor {
        emit_cursor_overlay(vertices, layout, ctx.grid, ctx.colors, cursor);
    }

    if let Some(preedit) = ctx.options.preedit
        && !preedit.text.is_empty()
        && ctx.grid.cursor.row < ctx.grid.rows
    {
        emit_preedit_overlay(vertices, preedit, layout, ctx);
    }
}

#[cfg(test)]
pub fn build_vertices(
    vertices: &mut Vec<f32>,
    grid: &Grid,
    colors: ColorScheme<'_>,
    metrics: CellMetrics,
    fonts: &FontManager,
    atlas: &GlyphAtlas,
    options: RenderOptions<'_>,
) {
    vertices.clear();
    let cursor = cursor_cell(grid);
    let ctx = RenderContext {
        grid,
        colors,
        metrics,
        fonts,
        atlas,
        options,
        cursor,
    };
    let mut bg_buf = Vec::new();
    let mut fg_buf = Vec::new();
    let mut bgs = Vec::new();
    let mut fgs = Vec::new();
    for r in 0..grid.rows {
        build_row_backgrounds(&mut bg_buf, r, &ctx);
        build_row_foregrounds(&mut fg_buf, r, &ctx);
        grid.visible_line(r).dirty.set(false);
        bgs.extend_from_slice(&bg_buf);
        fgs.extend_from_slice(&fg_buf);
    }
    vertices.extend_from_slice(&bgs);
    vertices.extend_from_slice(&fgs);
    build_dynamic_overlays(vertices, &ctx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{Color, Rgb, default_256_palette};
    use crate::font::FontManager;
    use crate::grid::{CellFlags, Grid};
    use crate::render::{ColorScheme, RenderOptions};

    fn write_cluster(grid: &mut Grid, cluster: &str) {
        for c in cluster.chars() {
            grid.write_char(
                c,
                Color::DefaultForeground,
                Color::DefaultBackground,
                CellFlags::empty(),
            );
        }
    }

    #[test]
    fn test_emoji_clusters_emit_single_composed_quad() {
        let path = std::path::Path::new("/usr/share/fonts/noto/NotoColorEmoji.ttf");
        if !path.exists() {
            return;
        }
        let fonts = FontManager::load(14.0).expect("load fonts");

        for cluster in ["🇨🇳", "👨‍👩‍👧‍👦", "1️⃣"] {
            let mut grid = Grid::new(4, 1, 0);
            grid.cursor.visible = false;
            write_cluster(&mut grid, cluster);

            let mut atlas = GlyphAtlas::new(256, 256);
            prepare_atlas(&grid, &fonts, &mut atlas, None);

            let mut fg_vertices = Vec::new();
            let palette = default_256_palette();
            let colors = ColorScheme::new(&palette, Rgb::new(220, 220, 220), Rgb::new(24, 24, 24));
            let ctx = RenderContext {
                grid: &grid,
                colors,
                metrics: fonts.metrics,
                fonts: &fonts,
                atlas: &atlas,
                options: RenderOptions::default(),
                cursor: None,
            };
            build_row_foregrounds(&mut fg_vertices, 0, &ctx);

            let color_quads: Vec<_> = fg_vertices.chunks(8).filter(|v| v[7] < 0.0).collect();
            assert!(
                !color_quads.is_empty(),
                "cluster {cluster} must emit colored vertices"
            );
            assert_eq!(
                color_quads.len(),
                6,
                "cluster {cluster} must emit exactly 1 composed quad (6 vertices)"
            );
        }
    }
}
