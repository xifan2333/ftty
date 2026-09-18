//! Text grid vertex generation, font atlas integration, and dynamic overlay rendering.

#![forbid(unsafe_code)]

use crate::color::{Color, Rgb};
use crate::font::{CellMetrics, FontManager, GlyphAtlas};
use crate::grid::{Cell, CellFlags, CursorShape, Grid};
use crate::input::ime::Preedit;
use crate::render::{ColorScheme, RenderOptions};

pub(crate) const SOLID_UV: [[f32; 2]; 2] = [[-1.0, -1.0]; 2];
pub(crate) const SELECTION_BG: [f32; 4] = [0.35, 0.45, 0.70, 0.5];
pub(crate) const KITTY_PLACEHOLDER: char = '\u{10EEEE}';

pub(crate) fn visible_glyph(cell: &Cell) -> bool {
    cell.c != ' '
        && cell.c != KITTY_PLACEHOLDER
        && !cell
            .flags
            .intersects(CellFlags::HIDDEN | CellFlags::WIDE_CHAR_SPACER)
}

pub(crate) fn prepare_atlas(
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
            for c in p.text.chars() {
                if crate::render::box_drawing::is_procedural_glyph(c) {
                    continue;
                }
                full |= atlas
                    .get_or_insert(c, CellFlags::UNDERLINE, fonts)
                    .is_none();
            }
        }
        for row in 0..grid.rows {
            let line = grid.visible_line(row);
            if !line.dirty.get() && attempt == 0 && !repacked {
                continue;
            }
            for cell in line.cells.iter().filter(|cell| visible_glyph(cell)) {
                if crate::render::box_drawing::is_procedural_glyph(cell.c) {
                    continue;
                }
                full |= atlas.get_or_insert(cell.c, cell.flags, fonts).is_none();
            }
        }
        if !full || attempt == 1 {
            break;
        }
        // Repack only at a frame boundary, before generating any vertices or uploading pixels.
        atlas.clear();
        repacked = true;
    }
    repacked
}

pub(crate) fn rgba(color: Rgb) -> [f32; 4] {
    [
        f32::from(color.r) / 255.0,
        f32::from(color.g) / 255.0,
        f32::from(color.b) / 255.0,
        1.0,
    ]
}

pub(crate) fn cell_colors(cell: &Cell, colors: ColorScheme<'_>) -> (Rgb, Rgb) {
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

pub(crate) fn cursor_cell(grid: &Grid) -> Option<(usize, usize, usize)> {
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

pub(crate) struct RenderContext<'a> {
    pub(crate) grid: &'a Grid,
    pub(crate) colors: ColorScheme<'a>,
    pub(crate) metrics: CellMetrics,
    pub(crate) fonts: &'a FontManager,
    pub(crate) atlas: &'a GlyphAtlas,
    pub(crate) options: RenderOptions<'a>,
    pub(crate) cursor: Option<(usize, usize, usize)>,
}

pub(crate) fn build_row_backgrounds(vertices: &mut Vec<f32>, row: usize, ctx: &RenderContext<'_>) {
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
    let y = pad_y + row as f32 * ch;

    // Draw background and selection on this row with contiguous span merging
    let mut col = 0;
    while col < line.cells.len() {
        let (_, bg) = cell_colors(&line.cells[col], colors);
        if bg == colors.background {
            col += 1;
            continue;
        }
        let start_col = col;
        col += 1;
        while col < line.cells.len() {
            let (_, next_bg) = cell_colors(&line.cells[col], colors);
            if next_bg != bg {
                break;
            }
            col += 1;
        }
        let sx = pad_x + start_col as f32 * cw;
        let ex = pad_x + col as f32 * cw;
        push_quad(vertices, [sx, y, ex, y + ch], SOLID_UV, rgba(bg));
    }
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

pub(crate) fn build_row_foregrounds(vertices: &mut Vec<f32>, row: usize, ctx: &RenderContext<'_>) {
    vertices.clear();
    let grid = ctx.grid;
    let colors = ctx.colors;
    let metrics = ctx.metrics;
    let fonts = ctx.fonts;
    let atlas = ctx.atlas;
    let options = ctx.options;
    let cursor = ctx.cursor;

    let cw = metrics.cell_width as f32;
    let ch = metrics.cell_height as f32;
    let pad_x = f32::from(options.padding[0]);
    let pad_y = f32::from(options.padding[1]);

    let abs_line = grid.scrollback.len() + row - grid.viewport_offset();
    let line = grid.visible_line(row);
    let y = pad_y + row as f32 * ch;

    // Draw text glyphs and underlines on this row
    for (col, cell) in line.cells.iter().enumerate() {
        if cell
            .flags
            .intersects(CellFlags::HIDDEN | CellFlags::WIDE_CHAR_SPACER)
        {
            continue;
        }
        let x = pad_x + col as f32 * cw;
        let (fg, _) = cell_colors(cell, colors);
        let under_block = grid.cursor.shape == CursorShape::Block
            && cursor.is_some_and(|(r, c, width)| row == r && col >= c && col < c + width);
        let mut color = rgba(if under_block { colors.background } else { fg });
        if cell.flags.contains(CellFlags::DIM) {
            color[3] = 0.6;
        }
        let width = if cell.flags.contains(CellFlags::WIDE_CHAR) {
            2.0 * cw
        } else {
            cw
        };
        if visible_glyph(cell) {
            if crate::render::box_drawing::render_procedural_glyph(
                vertices, cell.c, x, y, width, ch, color,
            ) {
                // Procedural box drawing and block elements glyph
            } else if let Some(glyph) = atlas.get(cell.c, cell.flags, fonts) {
                if glyph.width > 0 && glyph.height > 0 {
                    let gx = x + glyph.offset_x as f32;
                    let gy =
                        y + metrics.ascent as f32 - glyph.offset_y as f32 - glyph.height as f32;
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
            } else if let Some(fallback) = atlas.get('?', CellFlags::empty(), fonts) {
                // Fallback to '?' when the primary character does not fit in the atlas.
                if fallback.width > 0 && fallback.height > 0 {
                    let gx = x + fallback.offset_x as f32;
                    let gy = y + metrics.ascent as f32
                        - fallback.offset_y as f32
                        - fallback.height as f32;
                    let [u, v] = fallback.position.map(|value| value as f32);
                    let w = fallback.width as f32;
                    let h = fallback.height as f32;
                    push_quad(
                        vertices,
                        [gx, gy, gx + w, gy + h],
                        [[u, v], [u + w, v + h]],
                        color,
                    );
                }
            } else {
                // Solid placeholder quad if the atlas is entirely exhausted.
                let box_top = y + 2.0;
                let box_bot = y + ch - 2.0;
                if box_bot > box_top {
                    push_quad(
                        vertices,
                        [x + 1.0, box_top, x + cw - 1.0, box_bot],
                        SOLID_UV,
                        [color[0], color[1], color[2], color[3] * 0.5],
                    );
                }
            }
        }
        let has_explicit_underline = cell.flags.contains(CellFlags::UNDERLINE);
        let has_hover_underline = (cell.hyperlink_id.is_some() || cell.c != ' ')
            && options.hovered_span.is_some_and(|span| {
                span.line == abs_line && col >= span.start_col && col <= span.end_col
            });
        if has_explicit_underline || has_hover_underline {
            let ul_color = if cell.underline_color != Color::DefaultForeground {
                let resolved = cell.underline_color.to_rgb(
                    colors.palette,
                    colors.foreground,
                    colors.background,
                );
                rgba(resolved)
            } else {
                color
            };
            let base_top = y + (metrics.ascent as f32 + 1.0).min(ch - 1.0);

            if cell.flags.contains(CellFlags::UNDERLINE_DOUBLE) {
                let top1 = y + (metrics.ascent as f32).min(ch - 3.0);
                let top2 = top1 + 2.0;
                push_quad(
                    vertices,
                    [x, top1, x + width, top1 + 1.0],
                    SOLID_UV,
                    ul_color,
                );
                push_quad(
                    vertices,
                    [x, top2, x + width, top2 + 1.0],
                    SOLID_UV,
                    ul_color,
                );
            } else if cell.flags.contains(CellFlags::UNDERLINE_CURLY) {
                let steps = (width * 2.0).round().max(4.0) as usize;
                let step_w = width / steps as f32;
                let amplitude = 1.5_f32;
                let period = cw.max(4.0);
                for step in 0..steps {
                    let seg_x = x + step as f32 * step_w;
                    let wave = ((seg_x - x) / period * std::f32::consts::TAU).sin() * amplitude;
                    let seg_y = (base_top + wave).clamp(y, y + ch - 1.0);
                    push_quad(
                        vertices,
                        [seg_x, seg_y, seg_x + step_w, seg_y + 1.0],
                        SOLID_UV,
                        ul_color,
                    );
                }
            } else if cell.flags.contains(CellFlags::UNDERLINE_DOTTED) {
                let dot_size = 2.0_f32;
                let mut dot_x = x;
                while dot_x < x + width {
                    let cur_w = dot_size.min(x + width - dot_x);
                    push_quad(
                        vertices,
                        [dot_x, base_top, dot_x + cur_w, base_top + 1.0],
                        SOLID_UV,
                        ul_color,
                    );
                    dot_x += dot_size * 2.0;
                }
            } else if cell.flags.contains(CellFlags::UNDERLINE_DASHED) {
                let dash_len = 4.0_f32;
                let gap = 3.0_f32;
                let mut dash_x = x;
                while dash_x < x + width {
                    let cur_w = dash_len.min(x + width - dash_x);
                    push_quad(
                        vertices,
                        [dash_x, base_top, dash_x + cur_w, base_top + 1.0],
                        SOLID_UV,
                        ul_color,
                    );
                    dash_x += dash_len + gap;
                }
            } else {
                push_quad(
                    vertices,
                    [x, base_top, x + width, base_top + 1.0],
                    SOLID_UV,
                    ul_color,
                );
            }
        }
        if cell.flags.contains(CellFlags::STRIKETHROUGH) {
            let top = y + (metrics.ascent as f32 * 0.65).floor();
            push_quad(vertices, [x, top, x + width, top + 1.0], SOLID_UV, color);
        }
    }
}

pub(crate) fn build_dynamic_overlays(vertices: &mut Vec<f32>, ctx: &RenderContext<'_>) {
    let grid = ctx.grid;
    let colors = ctx.colors;
    let metrics = ctx.metrics;
    let fonts = ctx.fonts;
    let atlas = ctx.atlas;
    let options = ctx.options;
    let cursor = ctx.cursor;

    let cw = metrics.cell_width as f32;
    let ch = metrics.cell_height as f32;
    let pad_x = f32::from(options.padding[0]);
    let pad_y = f32::from(options.padding[1]);

    if let Some((row, col, width)) = cursor {
        let x = pad_x + col as f32 * cw;
        let y = pad_y + row as f32 * ch;
        let rect = match grid.cursor.shape {
            CursorShape::Block => [x, y, x + width as f32 * cw, y + ch],
            CursorShape::Beam => [x, y, x + 2.0_f32.min(cw), y + ch],
            CursorShape::Underline => [x, y + (ch - 2.0).max(0.0), x + width as f32 * cw, y + ch],
        };
        if grid.cursor.shape != CursorShape::Block {
            push_quad(vertices, rect, SOLID_UV, rgba(colors.foreground));
        }
    }

    // If an IME pre-edit string is active, render it inline starting at cursor position
    if let Some(preedit) = options.preedit
        && !preedit.text.is_empty()
        && let Some((crow, ccol, _)) = cursor
    {
        let mut cur_col = ccol;
        for c in preedit.text.chars() {
            let remaining_cols = grid.cols.saturating_sub(cur_col);
            if remaining_cols == 0 {
                break;
            }
            let char_width = unicode_width::UnicodeWidthChar::width(c).unwrap_or(1);
            let visible_cols = char_width.min(remaining_cols);
            let px = pad_x + cur_col as f32 * cw;
            let py = pad_y + crow as f32 * ch;
            let span_w = visible_cols as f32 * cw;

            // Draw preedit cell background
            push_quad(
                vertices,
                [px, py, px + span_w, py + ch],
                SOLID_UV,
                [0.2, 0.25, 0.35, 0.95],
            );

            // Draw preedit glyph
            let span_right = px + span_w;
            if crate::render::box_drawing::render_procedural_glyph(
                vertices,
                c,
                px,
                py,
                span_w,
                ch,
                rgba(colors.foreground),
            ) {
                // Procedural box drawing / block elements glyph
            } else if let Some(glyph) = atlas.get(c, CellFlags::UNDERLINE, fonts)
                && glyph.width > 0
                && glyph.height > 0
            {
                let gx = px + glyph.offset_x as f32;
                let gy = py + metrics.ascent as f32 - glyph.offset_y as f32 - glyph.height as f32;
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
                        rgba(colors.foreground),
                    );
                }
            }

            // Draw preedit underline
            let top = py + (metrics.ascent as f32 + 1.0).min(ch - 1.0);
            push_quad(
                vertices,
                [px, top, px + span_w, top + 1.0],
                SOLID_UV,
                rgba(colors.foreground),
            );

            cur_col += char_width;
        }
    }
}

#[cfg(test)]
pub(crate) fn build_vertices(
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
