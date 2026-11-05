//! Wayland EGL ownership and batched OpenGL ES 3 terminal rendering.

use std::collections::HashMap;

use glow::HasContext;
use khronos_egl as egl;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Proxy};
use wayland_egl::WlEglSurface;

use crate::color::{Color, Rgb};
use crate::error::RenderError;
use crate::font::{CellMetrics, FontManager, GlyphAtlas};
use crate::grid::{Cell, CellFlags, CursorShape, Grid};
use crate::input::ime::Preedit;
use crate::input::selection::Selection;

pub mod procedural;
pub use procedural::{
    SOLID_UV, Stroke, is_procedural_glyph, push_solid_poly_quad, push_solid_quad,
    render_block_element, render_box_drawing, render_procedural_glyph,
};

pub mod box_drawing {
    pub use super::procedural::*;
}

// --- Shader ---

pub const VERTEX_SHADER: &str = r#"
attribute vec2 a_position;
attribute vec2 a_tex_coords;
attribute vec4 a_color;
uniform vec2 u_viewport;
uniform vec2 u_atlas_size;
varying highp vec2 v_tex_coords;
varying lowp vec4 v_color;
void main() {
    v_tex_coords = a_tex_coords / u_atlas_size;
    v_color = a_color;
    gl_Position = vec4(a_position / u_viewport * vec2(2.0, -2.0) + vec2(-1.0, 1.0), 0.0, 1.0);
}
"#;

#[doc(hidden)]
pub const FRAGMENT_SHADER: &str = r#"
#extension GL_EXT_blend_func_extended : enable

#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
varying highp vec2 v_tex_coords;
#else
precision mediump float;
varying mediump vec2 v_tex_coords;
#endif

varying lowp vec4 v_color;
uniform sampler2D u_texture;
// 0 = glyph/background placement, 1 = RGBA kitty image placement.
uniform int u_image_mode;
// 0 = standard alpha blending, 1 = dual-source subpixel blending.
uniform int u_subpixel_mode;

void main() {
    if (u_image_mode == 1) {
        vec4 texel = texture2D(u_texture, v_tex_coords);
#if defined(GL_EXT_blend_func_extended)
        gl_FragColor = vec4(texel.rgb, 1.0);
        gl_SecondaryFragColorEXT = vec4(texel.a * v_color.a);
#else
        gl_FragColor = vec4(texel.rgb, texel.a * v_color.a);
#endif
    } else {
        vec4 mask = v_tex_coords.x < 0.0 ? vec4(1.0) : texture2D(u_texture, v_tex_coords);
        if (v_color.a < 0.0) {
            float alpha = -v_color.a;
#if defined(GL_EXT_blend_func_extended)
            gl_FragColor = vec4(mask.rgb, 1.0);
            gl_SecondaryFragColorEXT = vec4(mask.a * alpha);
#else
            gl_FragColor = vec4(mask.rgb, mask.a * alpha);
#endif
        } else {
#if defined(GL_EXT_blend_func_extended)
            gl_FragColor = vec4(v_color.rgb, 1.0);
            if (u_subpixel_mode == 1) {
                gl_SecondaryFragColorEXT = mask * v_color.a;
            } else {
                gl_SecondaryFragColorEXT = vec4(mask.a * v_color.a);
            }
#else
            gl_FragColor = vec4(v_color.rgb, v_color.a * mask.a);
#endif
        }
    }
}
"#;

pub(crate) fn compile_shader(
    gl: &glow::Context,
    kind: u32,
    source: &str,
) -> Result<glow::Shader, RenderError> {
    // SAFETY: callers hold the current EGL context.
    unsafe {
        let shader = gl
            .create_shader(kind)
            .map_err(RenderError::ShaderCreation)?;
        gl.shader_source(shader, source);
        gl.compile_shader(shader);
        if !gl.get_shader_compile_status(shader) {
            let log = gl.get_shader_info_log(shader);
            gl.delete_shader(shader);
            let kind_name = if kind == glow::VERTEX_SHADER {
                "vertex"
            } else {
                "fragment"
            };
            return Err(RenderError::ShaderCompile {
                kind: kind_name,
                log,
            });
        }
        Ok(shader)
    }
}

pub(crate) fn create_program(gl: &glow::Context) -> Result<glow::Program, RenderError> {
    // SAFETY: initialization holds the current EGL context.
    unsafe {
        let vertex = compile_shader(gl, glow::VERTEX_SHADER, VERTEX_SHADER)?;
        let fragment = match compile_shader(gl, glow::FRAGMENT_SHADER, FRAGMENT_SHADER) {
            Ok(shader) => shader,
            Err(error) => {
                gl.delete_shader(vertex);
                return Err(error);
            }
        };
        let program = match gl.create_program() {
            Ok(program) => program,
            Err(error) => {
                gl.delete_shader(vertex);
                gl.delete_shader(fragment);
                return Err(RenderError::ProgramCreation(error));
            }
        };
        gl.attach_shader(program, vertex);
        gl.attach_shader(program, fragment);
        gl.bind_attrib_location(program, 0, "a_position");
        gl.bind_attrib_location(program, 1, "a_tex_coords");
        gl.bind_attrib_location(program, 2, "a_color");
        gl.link_program(program);
        gl.detach_shader(program, vertex);
        gl.detach_shader(program, fragment);
        gl.delete_shader(vertex);
        gl.delete_shader(fragment);
        if !gl.get_program_link_status(program) {
            let log = gl.get_program_info_log(program);
            gl.delete_program(program);
            return Err(RenderError::ProgramLink { log });
        }
        Ok(program)
    }
}

// --- EGL ---

pub(crate) struct EglContext {
    pub(crate) egl: egl::DynamicInstance<egl::EGL1_5>,
    pub(crate) display: egl::Display,
    pub(crate) context: Option<egl::Context>,
    pub(crate) surface: Option<egl::Surface>,
    pub(crate) initialized: bool,
    // Field order keeps the Wayland connection alive through native window destruction.
    pub(crate) window: WlEglSurface,
    _surface: WlSurface,
    _connection: Connection,
}

impl EglContext {
    pub(crate) fn new(
        surface: &WlSurface,
        connection: &Connection,
        size: [u32; 2],
    ) -> Result<Self, RenderError> {
        let [width, height] = native_size(size)?;
        let window = WlEglSurface::new(surface.id(), width, height)
            .map_err(|e| RenderError::EglSurface(format!("{e:?}")))?;
        // SAFETY: the library stays loaded in this instance for all EGL calls.
        let egl = unsafe { egl::DynamicInstance::<egl::EGL1_5>::load_required() }
            .map_err(|e| RenderError::Gl(format!("{e:?}")))?;
        // SAFETY: connection owns the live libwayland display and is retained below.
        let display = unsafe { egl.get_display(connection.backend().display_ptr().cast()) }
            .ok_or_else(|| RenderError::EglDisplay("eglGetDisplay failed".to_string()))?;
        egl.initialize(display)
            .map_err(|e| RenderError::EglInit(format!("{e:?}")))?;
        // Own each handle as soon as it exists, including during failed initialization.
        let mut context = Self {
            egl,
            display,
            context: None,
            surface: None,
            initialized: false,
            window,
            _surface: surface.clone(),
            _connection: connection.clone(),
        };

        let init_result = (|| -> Result<(), RenderError> {
            context
                .egl
                .bind_api(egl::OPENGL_ES_API)
                .map_err(|e| RenderError::Gl(format!("{e:?}")))?;
            let config = context
                .egl
                .choose_first_config(
                    display,
                    &[
                        egl::SURFACE_TYPE,
                        egl::WINDOW_BIT,
                        egl::RENDERABLE_TYPE,
                        egl::OPENGL_ES3_BIT,
                        egl::RED_SIZE,
                        8,
                        egl::GREEN_SIZE,
                        8,
                        egl::BLUE_SIZE,
                        8,
                        egl::ALPHA_SIZE,
                        8,
                        egl::NONE,
                    ],
                )
                .map_err(|e| RenderError::Gl(format!("{e:?}")))?
                .ok_or(RenderError::NoSupportedConfig)?;
            context.context = Some(
                context
                    .egl
                    .create_context(
                        display,
                        config,
                        None,
                        &[egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE],
                    )
                    .map_err(|e| RenderError::ContextCreation(format!("{e:?}")))?,
            );
            // SAFETY: window wraps a live wl_surface on this EGL display.
            context.surface = Some(
                unsafe {
                    context.egl.create_window_surface(
                        display,
                        config,
                        context.window.ptr().cast_mut(),
                        None,
                    )
                }
                .map_err(|e| RenderError::SurfaceCreation(format!("{e:?}")))?,
            );
            context.make_current()?;
            // Frame callbacks pace drawing; swapping must not block PTY and signal dispatch.
            context
                .egl
                .swap_interval(display, 0)
                .map_err(|e| RenderError::Gl(format!("{e:?}")))?;
            Ok(())
        })();

        if let Err(err) = init_result {
            let _ = context.egl.make_current(display, None, None, None);
            if let Some(surface) = context.surface {
                let _ = context.egl.destroy_surface(display, surface);
            }
            if let Some(ctx) = context.context {
                let _ = context.egl.destroy_context(display, ctx);
            }
            let _ = context.egl.terminate(display);
            return Err(err);
        }

        context.initialized = true;
        Ok(context)
    }

    pub(crate) fn make_current(&self) -> Result<(), RenderError> {
        self.egl
            .make_current(self.display, self.surface, self.surface, self.context)
            .map_err(|e| RenderError::MakeCurrent(format!("{e:?}")))
    }
}

impl Drop for EglContext {
    fn drop(&mut self) {
        if !self.initialized {
            return;
        }
        let _ = self.egl.make_current(self.display, None, None, None);
        if let Some(surface) = self.surface {
            let _ = self.egl.destroy_surface(self.display, surface);
        }
        if let Some(context) = self.context {
            let _ = self.egl.destroy_context(self.display, context);
        }
        let _ = self.egl.terminate(self.display);
    }
}

pub fn native_size([width, height]: [u32; 2]) -> Result<[i32; 2], RenderError> {
    if width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32 {
        return Err(RenderError::InvalidDimensions(width, height));
    }
    Ok([width as i32, height as i32])
}

// --- Text & Vertices ---

#[doc(hidden)]
pub const SELECTION_BG: [f32; 4] = [0.35, 0.45, 0.70, 0.5];
#[doc(hidden)]
pub const KITTY_PLACEHOLDER: char = '\u{10EEEE}';

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

#[doc(hidden)]
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
            for (col, cell) in line.cells.iter().enumerate() {
                if !visible_glyph(cell) {
                    continue;
                }
                if !crate::render::box_drawing::is_procedural_glyph(cell.c) {
                    full |= atlas.get_or_insert(cell.c, cell.flags, fonts).is_none();
                }
                if let Some(cluster) = line.cluster_text(col) {
                    for extra in cluster.chars().skip(1) {
                        if is_cluster_format(extra) {
                            continue;
                        }
                        full |= atlas.get_or_insert(extra, cell.flags, fonts).is_none();
                    }
                }
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

#[doc(hidden)]
pub fn rgba(color: Rgb) -> [f32; 4] {
    [
        f32::from(color.r) / 255.0,
        f32::from(color.g) / 255.0,
        f32::from(color.b) / 255.0,
        1.0,
    ]
}

#[doc(hidden)]
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

#[doc(hidden)]
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

#[doc(hidden)]
pub struct RenderContext<'a> {
    pub grid: &'a Grid,
    pub colors: ColorScheme<'a>,
    pub metrics: CellMetrics,
    pub fonts: &'a FontManager,
    pub atlas: &'a GlyphAtlas,
    pub options: RenderOptions<'a>,
    pub cursor: Option<(usize, usize, usize)>,
}

#[doc(hidden)]
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

#[doc(hidden)]
pub fn build_row_foregrounds(vertices: &mut Vec<f32>, row: usize, ctx: &RenderContext<'_>) {
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
            .intersects(CellFlags::HIDDEN | CellFlags::WIDE_CHAR_SPACER | CellFlags::WRAP_SPACER)
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
            if crate::render::box_drawing::is_procedural_glyph(cell.c)
                && crate::render::box_drawing::render_procedural_glyph(
                    vertices, cell.c, x, y, width, ch, color,
                )
            {
                // Procedural box drawing and block elements glyph
            } else if let Some(glyph) = atlas.get(cell.c, cell.flags, fonts) {
                if glyph.width > 0 && glyph.height > 0 {
                    let (gx, gy) = if glyph.colored {
                        let pad_x = (width - glyph.width as f32).max(0.0) / 2.0;
                        let pad_y = (ch - glyph.height as f32).max(0.0) / 2.0;
                        ((x + pad_x).round(), (y + pad_y).round())
                    } else {
                        let gx = (x + glyph.offset_x as f32).round();
                        let gy = (y + metrics.ascent as f32
                            - glyph.offset_y as f32
                            - glyph.height as f32)
                            .round();
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
            } else if let Some(fallback) = atlas.get('?', CellFlags::empty(), fonts) {
                // Fallback to '?' when the primary character does not fit in the atlas.
                if fallback.width > 0 && fallback.height > 0 {
                    let gx = (x + fallback.offset_x as f32).round();
                    let gy = (y + metrics.ascent as f32
                        - fallback.offset_y as f32
                        - fallback.height as f32)
                        .round();
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
        if let Some(cluster) = line.cluster_text(col) {
            for extra in cluster.chars().skip(1) {
                if is_cluster_format(extra) {
                    continue;
                }
                let extra_width = crate::width::wcwidth(extra, grid.ambiguous_width);
                if extra_width >= 2 {
                    continue;
                }
                let Some(glyph) = atlas.get(extra, cell.flags, fonts) else {
                    continue;
                };
                if glyph.width == 0 || glyph.height == 0 {
                    continue;
                }
                let origin = if extra_width <= 0 {
                    x + width * 0.5
                } else {
                    x + cw
                };
                let gx = (origin + glyph.offset_x as f32).round();
                let gy = (y + metrics.ascent as f32 - glyph.offset_y as f32 - glyph.height as f32)
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

#[doc(hidden)]
pub fn build_dynamic_overlays(vertices: &mut Vec<f32>, ctx: &RenderContext<'_>) {
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

    // If an IME pre-edit string is active, render it inline starting at cursor position.
    // Aligns with Kitty (`kitty/screen.c`): preedit is rendered as an overlay at the active
    // grid cursor row and column even if the hardware cursor is hidden by the terminal application.
    if let Some(preedit) = options.preedit
        && !preedit.text.is_empty()
        && grid.cursor.row < grid.rows
    {
        let crow_opt = if grid.viewport_offset == 0 {
            Some(grid.cursor.row)
        } else if grid.cursor.row + grid.viewport_offset < grid.rows {
            Some(grid.cursor.row + grid.viewport_offset)
        } else {
            None
        };
        if let Some(crow) = crow_opt {
            let mut ccol = grid.cursor.col.min(grid.cols.saturating_sub(1));
            let line = grid.visible_line(crow);
            if ccol > 0
                && ccol < line.cells.len()
                && line.cells[ccol].flags.contains(CellFlags::WIDE_CHAR_SPACER)
            {
                ccol -= 1;
            }
            let mut cur_col = ccol;
            for c in preedit.text.chars() {
                let remaining_cols = grid.cols.saturating_sub(cur_col);
                if remaining_cols == 0 {
                    break;
                }
                let char_width =
                    crate::width::wcwidth(c, grid.ambiguous_width).clamp(0, 2) as usize;
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
                    let gy =
                        py + metrics.ascent as f32 - glyph.offset_y as f32 - glyph.height as f32;
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
}

#[doc(hidden)]
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

// --- Image ---

#[must_use]
pub fn placeholder_image_id(color: Color) -> u32 {
    match color {
        Color::Rgb(r, g, b) => ((r as u32) << 16) | ((g as u32) << 8) | (b as u32),
        Color::Indexed(idx) => idx as u32,
        _ => 0,
    }
}

/// Resolves each 24-bit placeholder id to a full texture id.
///
/// An id that already equals its own low 24 bits takes precedence over any aliasing id that
/// only shares the same low 24 bits, matching the Kitty unicode-placeholder lookup rules.
#[must_use]
#[doc(hidden)]
pub fn build_low24_index(texture_ids: impl Iterator<Item = u32>) -> HashMap<u32, u32> {
    let mut index: HashMap<u32, u32> = HashMap::new();
    for texture_id in texture_ids {
        let low24 = texture_id & 0x00FF_FFFF;
        if texture_id == low24 {
            // An exact id always takes precedence over a 24-bit alias.
            index.insert(low24, texture_id);
        } else {
            index.entry(low24).or_insert(texture_id);
        }
    }
    index
}

/// Returns `buffer` unchanged when its capacity is within the retention cap, otherwise a fresh
/// small vector so oversized transient allocations are deterministically released.
#[must_use]
#[doc(hidden)]
pub fn bounded_image_vertex_buffer(buffer: Vec<f32>) -> Vec<f32> {
    if buffer.capacity() <= MAX_RETAINED_IMAGE_VERTEX_FLOATS {
        buffer
    } else {
        Vec::with_capacity(64)
    }
}

impl Renderer {
    /// Returns the image staging buffer to the renderer for reuse, dropping it when its capacity
    /// exceeds [`MAX_RETAINED_IMAGE_VERTEX_FLOATS`] so oversized transient frames cannot pin memory.
    fn recycle_image_vertices(&mut self, image_vertices: Vec<f32>) {
        self.image_vertices = bounded_image_vertex_buffer(image_vertices);
    }

    pub(crate) fn sync_image_textures(&mut self, graphics: &mut crate::kitty::GraphicStore) {
        let gl = &self.gl;
        let mut to_delete = Vec::new();
        self.image_textures.retain(|id, (tex, _, _, ver)| {
            if let Some(current_ver) = graphics.image_version(*id) {
                if *ver == current_ver {
                    true
                } else {
                    to_delete.push(*tex);
                    false
                }
            } else {
                to_delete.push(*tex);
                false
            }
        });

        // SAFETY: called only by draw with this renderer's EGL context current;
        // every texture in to_delete was removed from this renderer's texture map.
        unsafe {
            for tex in to_delete {
                gl.delete_texture(tex);
            }
        }

        // Upload newly decoded images through GraphicStore's sync seam.
        graphics.sync_gpu_textures(|id, width, height, ver, rgba| {
            if self.image_textures.contains_key(&id) {
                return false;
            }
            // SAFETY: draw holds this renderer's current EGL context. New textures
            // belong to it, and decoded RGBA pixels remain borrowed for the upload.
            unsafe {
                if let Ok(tex) = gl.create_texture() {
                    gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                    gl.tex_parameter_i32(
                        glow::TEXTURE_2D,
                        glow::TEXTURE_MIN_FILTER,
                        glow::LINEAR as i32,
                    );
                    gl.tex_parameter_i32(
                        glow::TEXTURE_2D,
                        glow::TEXTURE_MAG_FILTER,
                        glow::LINEAR as i32,
                    );
                    gl.tex_parameter_i32(
                        glow::TEXTURE_2D,
                        glow::TEXTURE_WRAP_S,
                        glow::CLAMP_TO_EDGE as i32,
                    );
                    gl.tex_parameter_i32(
                        glow::TEXTURE_2D,
                        glow::TEXTURE_WRAP_T,
                        glow::CLAMP_TO_EDGE as i32,
                    );
                    gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
                    gl.tex_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        glow::RGBA as i32,
                        width as i32,
                        height as i32,
                        0,
                        glow::RGBA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(Some(rgba)),
                    );
                    self.image_textures.insert(id, (tex, width, height, ver));
                    true
                } else {
                    false
                }
            }
        });
    }

    pub(crate) fn render_image_placements(
        &mut self,
        grid: &Grid,
        z_negative: bool,
        metrics: CellMetrics,
        options: RenderOptions<'_>,
    ) {
        let cw = metrics.cell_width as f32;
        let ch = metrics.cell_height as f32;
        let pad_x = f32::from(options.padding[0]);
        let pad_y = f32::from(options.padding[1]);
        let h = grid.scrollback.len();
        let viewport_start = h.saturating_sub(grid.viewport_offset);
        let viewport_end = viewport_start + grid.rows;

        for placement in grid.graphics.placements() {
            let is_match = if z_negative {
                placement.z_index < 0
            } else {
                placement.z_index >= 0
            };
            if !is_match {
                continue;
            }

            let end_line = placement.line + placement.rows;
            if end_line <= viewport_start || placement.line >= viewport_end {
                continue;
            }

            let Some(&(tex, img_w, img_h, _)) = self.image_textures.get(&placement.image_id) else {
                continue;
            };

            let screen_row_f = placement.line as f32 - viewport_start as f32;

            // Pixel-based clipping: compute the placement's destination rectangle on screen.
            let orig_x0 = pad_x + placement.col as f32 * cw + placement.offset_x as f32;
            let orig_y0 = pad_y + screen_row_f * ch + placement.offset_y as f32;
            let orig_w = (placement.cols as f32 * cw).max(1.0);
            let orig_h = (placement.rows as f32 * ch).max(1.0);
            let orig_x1 = orig_x0 + orig_w;
            let orig_y1 = orig_y0 + orig_h;

            // Determine vertical clipping range:
            // When at the bottom of scrollback (live screen), clip against active scroll margins (DECSTBM)
            // so images never overflow into fixed status footers. When viewing historical scrollback,
            // clip against the visible viewport.
            let (clip_min_y, clip_max_y) = if grid.viewport_offset == 0
                && (grid.scroll_region_top > 0
                    || grid.scroll_region_bottom < grid.rows.saturating_sub(1))
            {
                (
                    pad_y + grid.scroll_region_top as f32 * ch,
                    pad_y + (grid.scroll_region_bottom + 1) as f32 * ch,
                )
            } else {
                (pad_y, pad_y + grid.rows as f32 * ch)
            };
            let clip_min_x = pad_x;
            let clip_max_x = pad_x + grid.cols as f32 * cw;

            // Intersect destination rectangle with clipping bounds
            let x0 = orig_x0.clamp(clip_min_x, clip_max_x);
            let x1 = orig_x1.clamp(clip_min_x, clip_max_x);
            let y0 = orig_y0.clamp(clip_min_y, clip_max_y);
            let y1 = orig_y1.clamp(clip_min_y, clip_max_y);

            if x1 <= x0 || y1 <= y0 {
                continue;
            }

            let sx0 = placement.src_x as f32;
            let sy0 = placement.src_y as f32;
            let full_src_w = placement
                .src_w
                .map_or(img_w as f32 - sx0, |w| (w as f32).min(img_w as f32 - sx0));
            let full_src_h = placement
                .src_h
                .map_or(img_h as f32 - sy0, |h| (h as f32).min(img_h as f32 - sy0));

            // Derive UV crop coordinates precisely from the clipped pixel fractions
            let left_ratio = (x0 - orig_x0) / orig_w;
            let right_ratio = (x1 - orig_x0) / orig_w;
            let top_ratio = (y0 - orig_y0) / orig_h;
            let bottom_ratio = (y1 - orig_y0) / orig_h;

            let crop_sx0 = sx0 + full_src_w * left_ratio;
            let crop_sx1 = sx0 + full_src_w * right_ratio;
            let crop_sy0 = sy0 + full_src_h * top_ratio;
            let crop_sy1 = sy0 + full_src_h * bottom_ratio;

            self.render_single_cropped_image(
                tex,
                img_w as f32,
                img_h as f32,
                [x0, y0, x1, y1],
                [[crop_sx0, crop_sy0], [crop_sx1, crop_sy1]],
            );
        }
    }

    pub(crate) fn render_single_cropped_image(
        &mut self,
        tex: glow::Texture,
        img_w: f32,
        img_h: f32,
        [x0, y0, x1, y1]: [f32; 4],
        src_coords: [[f32; 2]; 2],
    ) {
        let mut img_vertices = std::mem::take(&mut self.image_vertices);
        img_vertices.clear();
        push_quad(
            &mut img_vertices,
            [x0, y0, x1, y1],
            src_coords,
            [1.0, 1.0, 1.0, 1.0],
        );
        self.render_image_quads(tex, img_w, img_h, &img_vertices);
        self.recycle_image_vertices(img_vertices);
    }

    pub(crate) fn render_single_image(
        &mut self,
        tex: glow::Texture,
        img_w: f32,
        img_h: f32,
        [x0, y0, x1, y1]: [f32; 4],
    ) {
        // Move the shared staging buffer out so it can be borrowed while `self` is mutated,
        // then hand it back to retain its capacity for the next image.
        let mut img_vertices = std::mem::take(&mut self.image_vertices);
        img_vertices.clear();
        push_quad(
            &mut img_vertices,
            [x0, y0, x1, y1],
            [[0.0, 0.0], [img_w, img_h]],
            [1.0, 1.0, 1.0, 1.0],
        );
        self.render_image_quads(tex, img_w, img_h, &img_vertices);
        self.recycle_image_vertices(img_vertices);
    }

    pub(crate) fn render_image_quads(
        &mut self,
        tex: glow::Texture,
        img_w: f32,
        img_h: f32,
        vertices: &[f32],
    ) {
        let gl = &self.gl;
        // SAFETY: draw holds this renderer's current EGL context; tex and the VBO
        // belong to it. vertices contains initialized f32 values with no padding,
        // and remains live throughout the byte upload.
        unsafe {
            gl.use_program(self.program);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.uniform_1_i32(self.image_mode.as_ref(), 1);
            gl.uniform_1_i32(self.subpixel_mode.as_ref(), 0);
            gl.uniform_2_f32(self.atlas_size.as_ref(), img_w, img_h);

            let bytes = std::slice::from_raw_parts(
                vertices.as_ptr().cast::<u8>(),
                std::mem::size_of_val(vertices),
            );
            crate::render::Renderer::upload_vbo(
                gl,
                self.image_vbo,
                &mut self.image_vbo_capacity,
                bytes,
            );

            let stride = 8 * std::mem::size_of::<f32>() as i32;
            for (index, count, offset) in [(0, 2, 0), (1, 2, 8), (2, 4, 16)] {
                gl.enable_vertex_attrib_array(index);
                gl.vertex_attrib_pointer_f32(index, count, glow::FLOAT, false, stride, offset);
            }
            gl.draw_arrays(glow::TRIANGLES, 0, (vertices.len() / 8) as i32);
        }
    }

    pub(crate) fn render_unicode_placeholders(
        &mut self,
        grid: &Grid,
        metrics: CellMetrics,
        options: RenderOptions<'_>,
    ) {
        if self.image_textures.is_empty() {
            return;
        }

        let cw = metrics.cell_width as f32;
        let ch = metrics.cell_height as f32;
        let pad_x = f32::from(options.padding[0]);
        let pad_y = f32::from(options.padding[1]);

        struct RectBox {
            id: u32,
            col_start: usize,
            col_end: usize,
            row_start: usize,
            row_end: usize,
        }

        let mut completed_boxes: Vec<RectBox> = Vec::new();
        let mut active_boxes: Vec<RectBox> = Vec::new();

        // Built lazily on the first placeholder-bearing row so placement-only frames pay nothing.
        // Once built it maps each 24-bit placeholder id to its full texture id in O(1) per cell.
        let mut low24_index: Option<HashMap<u32, u32>> = None;

        for row in 0..grid.rows {
            let line = grid.visible_line(row);
            let mut row_segments: Vec<(u32, usize, usize)> = Vec::new();

            if line.placeholders.is_some() {
                let low24_index = low24_index
                    .get_or_insert_with(|| build_low24_index(self.image_textures.keys().copied()));
                let mut current_run: Option<(u32, usize, usize)> = None;

                for (col, cell) in line.cells.iter().enumerate() {
                    if cell.c == KITTY_PLACEHOLDER {
                        let id_low24 = placeholder_image_id(cell.fg) & 0x00FF_FFFF;
                        if id_low24 != 0 {
                            let real_id = low24_index.get(&id_low24).copied();

                            if let Some(matched_id) = real_id {
                                match current_run {
                                    Some((cur_id, start, end))
                                        if cur_id == matched_id && end + 1 == col =>
                                    {
                                        current_run = Some((cur_id, start, col));
                                    }
                                    Some(prev) => {
                                        row_segments.push(prev);
                                        current_run = Some((matched_id, col, col));
                                    }
                                    None => {
                                        current_run = Some((matched_id, col, col));
                                    }
                                }
                                continue;
                            }
                        }
                    }
                    if let Some(prev) = current_run.take() {
                        row_segments.push(prev);
                    }
                }
                if let Some(prev) = current_run {
                    row_segments.push(prev);
                }
            }

            // Merge matching row segments with active boxes from the previous row
            let mut next_active: Vec<RectBox> = Vec::new();
            for (id, col_start, col_end) in row_segments {
                if let Some(pos) = active_boxes.iter().position(|b| {
                    b.id == id
                        && b.col_start == col_start
                        && b.col_end == col_end
                        && b.row_end + 1 == row
                }) {
                    let mut b = active_boxes.swap_remove(pos);
                    b.row_end = row;
                    next_active.push(b);
                } else {
                    next_active.push(RectBox {
                        id,
                        col_start,
                        col_end,
                        row_start: row,
                        row_end: row,
                    });
                }
            }
            completed_boxes.append(&mut active_boxes);
            active_boxes = next_active;
        }
        completed_boxes.extend(active_boxes);

        for b in completed_boxes {
            let Some(&(tex, img_w, img_h, _)) = self.image_textures.get(&b.id) else {
                continue;
            };

            let box_w = b.col_end - b.col_start + 1;
            let box_h = b.row_end - b.row_start + 1;
            let (virt_cols, virt_rows) = grid
                .graphics
                .virtual_placements()
                .get(&b.id)
                .copied()
                .unwrap_or((box_w, box_h));

            if virt_cols == box_w && virt_rows == box_h {
                let x0 = pad_x + b.col_start as f32 * cw;
                let y0 = pad_y + b.row_start as f32 * ch;
                let x1 = pad_x + (b.col_end + 1) as f32 * cw;
                let y1 = pad_y + (b.row_end + 1) as f32 * ch;

                self.render_single_image(tex, img_w as f32, img_h as f32, [x0, y0, x1, y1]);
            } else {
                let total_c = virt_cols.max(1) as f32;
                let total_r = virt_rows.max(1) as f32;
                let mut img_vertices = std::mem::take(&mut self.image_vertices);
                img_vertices.clear();

                for row in b.row_start..=b.row_end {
                    let line = grid.visible_line(row);
                    for col in b.col_start..=b.col_end {
                        let (img_row, img_col) = if let Some(coords) = &line.placeholders
                            && let Some(&(ir, ic, _)) = coords.get(&col)
                        {
                            (ir as usize, ic as usize)
                        } else {
                            (row - b.row_start, col - b.col_start)
                        };

                        let x0 = pad_x + col as f32 * cw;
                        let y0 = pad_y + row as f32 * ch;
                        let x1 = x0 + cw;
                        let y1 = y0 + ch;

                        let u0 = (img_col as f32 / total_c) * img_w as f32;
                        let u1 = ((img_col + 1) as f32 / total_c) * img_w as f32;
                        let v0 = (img_row as f32 / total_r) * img_h as f32;
                        let v1 = ((img_row + 1) as f32 / total_r) * img_h as f32;

                        push_quad(
                            &mut img_vertices,
                            [x0, y0, x1, y1],
                            [[u0, v0], [u1, v1]],
                            [1.0, 1.0, 1.0, 1.0],
                        );
                    }
                }

                if !img_vertices.is_empty() {
                    self.render_image_quads(tex, img_w as f32, img_h as f32, &img_vertices);
                }
                self.recycle_image_vertices(img_vertices);
            }
        }
    }
}

// --- Renderer Core ---

/// Active color scheme holding the 256-color palette and default foreground/background.
#[derive(Debug, Clone, Copy)]
pub struct ColorScheme<'a> {
    pub palette: &'a [Rgb; 256],
    pub foreground: Rgb,
    pub background: Rgb,
}

impl<'a> ColorScheme<'a> {
    #[must_use]
    pub fn new(palette: &'a [Rgb; 256], foreground: Rgb, background: Rgb) -> Self {
        Self {
            palette,
            foreground,
            background,
        }
    }
}

impl<'a> From<&'a crate::theme::Theme> for ColorScheme<'a> {
    fn from(theme: &'a crate::theme::Theme) -> Self {
        Self::new(theme.palette(), theme.default_fg(), theme.default_bg())
    }
}

/// Represents the contiguous grid span of a hovered hyperlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HoveredHyperlinkSpan {
    pub line: usize,
    pub start_col: usize,
    pub end_col: usize,
}

/// Options controlling frame layout, window padding, active IME composition, and text selection.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderOptions<'a> {
    pub padding: [u16; 2],
    pub preedit: Option<&'a Preedit>,
    pub selection: Option<&'a Selection>,
    pub hovered_span: Option<HoveredHyperlinkSpan>,
}

impl<'a> RenderOptions<'a> {
    #[must_use]
    pub fn new(
        padding: [u16; 2],
        preedit: Option<&'a Preedit>,
        selection: Option<&'a Selection>,
    ) -> Self {
        Self {
            padding,
            preedit,
            selection,
            hovered_span: None,
        }
    }

    #[must_use]
    pub fn with_hovered_span(mut self, hovered_span: Option<HoveredHyperlinkSpan>) -> Self {
        self.hovered_span = hovered_span;
        self
    }
}

const MAX_RENDER_CACHE_ROWS: usize = 512;

#[doc(hidden)]
pub const DEFAULT_FG: Rgb = Rgb::new(220, 220, 220);
#[doc(hidden)]
pub const DEFAULT_BG: Rgb = Rgb::new(24, 24, 24);

// Retained capacity cap for the image vertex staging buffer. A single dense placeholder frame can
// grow the buffer to tens of thousands of floats; without a cap that peak allocation would stay
// resident forever. Anything above the cap is dropped at the end of the draw instead of recycled.
pub mod pipeline;
pub use pipeline::{RenderBatch, RenderPipeline, row_cache_needs_reset};

#[doc(hidden)]
pub const MAX_RETAINED_IMAGE_VERTEX_FLOATS: usize = 16 * 1024;

/// Owns GL objects together with their EGL context, including on initialization failure.
pub struct Renderer {
    pub(crate) pipeline: RenderPipeline,
    pub(crate) gl: glow::Context,
    pub(crate) program: Option<glow::Program>,
    pub(crate) vbo: Option<glow::Buffer>,
    pub(crate) vbo_capacity: usize,
    pub(crate) image_vbo: Option<glow::Buffer>,
    pub(crate) image_vbo_capacity: usize,
    pub(crate) texture: Option<glow::Texture>,
    pub(crate) viewport: Option<glow::UniformLocation>,
    pub(crate) atlas_size: Option<glow::UniformLocation>,
    pub(crate) image_mode: Option<glow::UniformLocation>,
    pub(crate) subpixel_mode: Option<glow::UniformLocation>,
    pub(crate) has_dual_source: bool,
    pub(crate) image_textures: HashMap<u32, (glow::Texture, u32, u32, u64)>,
    /// Scratch vertex staging buffer reused across image draws within a frame.
    pub(crate) image_vertices: Vec<f32>,
    pub(crate) egl: EglContext,
}

impl Renderer {
    /// Clears cached per-row vertex geometry across all screens and styles.
    pub fn clear_cache(&mut self) {
        self.pipeline.clear_cache();
    }

    /// Creates a renderer after the first XDG surface configure has been acknowledged.
    ///
    /// # Errors
    /// Returns an error if EGL, shaders, or GL resources cannot be initialized.
    pub fn new(
        surface: &WlSurface,
        connection: &Connection,
        size: [u32; 2],
    ) -> Result<Self, RenderError> {
        let egl = EglContext::new(surface, connection, size)?;
        // SAFETY: EGL is current, and its library and connection outlive the GL objects.
        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                egl.egl
                    .get_proc_address(name)
                    .map_or(std::ptr::null(), |function| {
                        function as *const () as *const _
                    })
            })
        };
        let exts = gl.supported_extensions();
        let has_dual_source = exts.contains("GL_EXT_blend_func_extended")
            || exts.contains("GL_ARB_blend_func_extended");

        let mut renderer = Self {
            pipeline: RenderPipeline::new(),
            gl,
            program: None,
            vbo: None,
            vbo_capacity: 0,
            image_vbo: None,
            image_vbo_capacity: 0,
            texture: None,
            viewport: None,
            atlas_size: None,
            image_mode: None,
            subpixel_mode: None,
            has_dual_source,
            image_textures: HashMap::new(),
            image_vertices: Vec::with_capacity(64),
            egl,
        };
        // SAFETY: the owned EGL context is current for all initialization calls.
        unsafe {
            let program = create_program(&renderer.gl)?;
            renderer.program = Some(program);
            renderer.vbo = Some(
                renderer
                    .gl
                    .create_buffer()
                    .map_err(RenderError::BufferCreation)?,
            );
            renderer.image_vbo = Some(
                renderer
                    .gl
                    .create_buffer()
                    .map_err(RenderError::BufferCreation)?,
            );
            renderer.texture = Some(
                renderer
                    .gl
                    .create_texture()
                    .map_err(RenderError::TextureCreation)?,
            );
            let gl = &renderer.gl;
            renderer.viewport = gl.get_uniform_location(program, "u_viewport");
            renderer.atlas_size = gl.get_uniform_location(program, "u_atlas_size");
            renderer.image_mode = gl.get_uniform_location(program, "u_image_mode");
            renderer.subpixel_mode = gl.get_uniform_location(program, "u_subpixel_mode");
            gl.use_program(Some(program));
            gl.uniform_1_i32(gl.get_uniform_location(program, "u_texture").as_ref(), 0);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, renderer.texture);
            for parameter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, glow::NEAREST as i32);
            }
            for parameter in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, glow::CLAMP_TO_EDGE as i32);
            }
            gl.enable(glow::BLEND);
            if renderer.has_dual_source {
                gl.blend_func(glow::SRC1_COLOR, glow::ONE_MINUS_SRC1_COLOR);
            } else {
                gl.blend_func_separate(
                    glow::SRC_ALPHA,
                    glow::ONE_MINUS_SRC_ALPHA,
                    glow::ONE,
                    glow::ONE_MINUS_SRC_ALPHA,
                );
            }
        }
        Ok(renderer)
    }

    /// Resizes the native window; the caller updates the grid from the same dimensions.
    ///
    /// # Errors
    /// Returns an error for zero or unrepresentable dimensions.
    pub fn resize(&mut self, size: [u32; 2]) -> Result<(), RenderError> {
        let [width, height] = native_size(size)?;
        self.egl.window.resize(width, height, 0, 0);
        self.clear_cache();
        Ok(())
    }

    /// Uploads newly cached glyphs and draws the current grid into the back buffer.
    ///
    /// # Errors
    /// Returns an error if the context cannot be made current or dimensions are invalid.
    pub fn render_grid(
        &mut self,
        grid: &mut Grid,
        colors: ColorScheme<'_>,
        fonts: &FontManager,
        atlas: &mut GlyphAtlas,
        size: [u32; 2],
        options: RenderOptions<'_>,
    ) -> Result<(), RenderError> {
        let [width, height] = native_size(size)?;
        self.egl.make_current()?;
        self.sync_image_textures(&mut grid.graphics);
        let repacked = self
            .pipeline
            .prepare_frame(grid, colors, fonts, atlas, size, options)?
            .atlas_repacked;
        if repacked {
            grid.mark_all_dirty();
        }

        let clear_color = self.pipeline.clear_color();

        // SAFETY: this renderer owns the current context and all referenced GL objects.
        unsafe {
            let gl = &self.gl;
            gl.viewport(0, 0, width, height);
            let [r, g, b, a] = clear_color;
            gl.clear_color(r, g, b, a);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }

        // Pass 1: z < 0 images (behind text)
        self.render_image_placements(grid, true, fonts.metrics, options);

        // Pass 2: text backgrounds, selection, text glyphs, cursor, preedit
        // SAFETY: draw made this renderer's EGL context current. The texture and VBO
        // belong to it, atlas.pixels covers the upload, and the vertex byte slice
        // covers initialized f32 values without padding and is used only for this upload.
        unsafe {
            let gl = &self.gl;
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, self.texture);
            if atlas.dirty {
                gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
                if atlas.full_upload || atlas.dirty_rect.is_none() {
                    gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
                    gl.pixel_store_i32(glow::UNPACK_SKIP_PIXELS, 0);
                    gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, 0);
                    gl.tex_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        glow::RGBA as i32,
                        atlas.width as i32,
                        atlas.height as i32,
                        0,
                        glow::RGBA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(Some(&atlas.pixels)),
                    );
                    atlas.full_upload = false;
                } else if let Some([min_x, min_y, max_x, max_y]) = atlas.dirty_rect {
                    let sub_w = (max_x - min_x).min(atlas.width - min_x);
                    let sub_h = (max_y - min_y).min(atlas.height - min_y);
                    if sub_w > 0 && sub_h > 0 {
                        gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, atlas.width as i32);
                        gl.pixel_store_i32(glow::UNPACK_SKIP_PIXELS, min_x as i32);
                        gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, min_y as i32);
                        gl.tex_sub_image_2d(
                            glow::TEXTURE_2D,
                            0,
                            min_x as i32,
                            min_y as i32,
                            sub_w as i32,
                            sub_h as i32,
                            glow::RGBA,
                            glow::UNSIGNED_BYTE,
                            glow::PixelUnpackData::Slice(Some(&atlas.pixels)),
                        );
                        gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
                        gl.pixel_store_i32(glow::UNPACK_SKIP_PIXELS, 0);
                        gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, 0);
                    }
                }
                atlas.dirty_rect = None;
                atlas.dirty = false;
            }
            gl.use_program(self.program);
            gl.uniform_1_i32(self.image_mode.as_ref(), 0);
            gl.uniform_1_i32(
                self.subpixel_mode.as_ref(),
                i32::from(fonts.subpixel && self.has_dual_source),
            );
            gl.uniform_2_f32(self.viewport.as_ref(), width as f32, height as f32);
            gl.uniform_2_f32(
                self.atlas_size.as_ref(),
                atlas.width as f32,
                atlas.height as f32,
            );
            // f32 has no padding, and the slice covers exactly the initialized vertex data.
            let verts = self.pipeline.vertices();
            let bytes = std::slice::from_raw_parts(
                verts.as_ptr().cast::<u8>(),
                std::mem::size_of_val(verts),
            );
            Self::upload_vbo(gl, self.vbo, &mut self.vbo_capacity, bytes);
            let stride = 8 * std::mem::size_of::<f32>() as i32;
            for (index, count, offset) in [(0, 2, 0), (1, 2, 8), (2, 4, 16)] {
                gl.enable_vertex_attrib_array(index);
                gl.vertex_attrib_pointer_f32(index, count, glow::FLOAT, false, stride, offset);
            }
            gl.draw_arrays(glow::TRIANGLES, 0, (verts.len() / 8) as i32);
        }

        // Pass 3: z >= 0 images (above text)
        self.render_image_placements(grid, false, fonts.metrics, options);
        self.render_unicode_placeholders(grid, fonts.metrics, options);
        Ok(())
    }

    pub(crate) unsafe fn upload_vbo(
        gl: &glow::Context,
        vbo: Option<glow::Buffer>,
        vbo_capacity: &mut usize,
        bytes: &[u8],
    ) {
        // SAFETY: caller ensures an EGL context is current, owns vbo, and bytes contains valid vertex data.
        unsafe {
            gl.bind_buffer(glow::ARRAY_BUFFER, vbo);
            if bytes.len() > *vbo_capacity {
                let new_cap = bytes.len().max(vbo_capacity.saturating_mul(2)).max(16384);
                gl.buffer_data_size(glow::ARRAY_BUFFER, new_cap as i32, glow::DYNAMIC_DRAW);
                *vbo_capacity = new_cap;
            }
            gl.buffer_sub_data_u8_slice(glow::ARRAY_BUFFER, 0, bytes);
        }
    }

    /// Presents the frame after the caller requests a Wayland frame callback.
    ///
    /// # Errors
    /// Returns an error if EGL cannot present the buffer.
    pub fn present(&self) -> Result<(), RenderError> {
        let surface = self.egl.surface.ok_or(RenderError::SurfaceNotInitialized)?;
        self.egl
            .egl
            .swap_buffers(self.egl.display, surface)
            .map_err(|e| RenderError::Gl(format!("{e:?}")))
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // If the context is lost, its destruction below reclaims the GL resources.
        if self.egl.make_current().is_ok() {
            // SAFETY: GL objects are deleted before their EGL context or display.
            unsafe {
                if let Some(program) = self.program {
                    self.gl.delete_program(program);
                }
                if let Some(vbo) = self.vbo {
                    self.gl.delete_buffer(vbo);
                }
                if let Some(image_vbo) = self.image_vbo {
                    self.gl.delete_buffer(image_vbo);
                }
                if let Some(texture) = self.texture {
                    self.gl.delete_texture(texture);
                }
                for (_, (tex, _, _, _)) in self.image_textures.drain() {
                    self.gl.delete_texture(tex);
                }
            }
        }
    }
}

pub mod shader {
    pub use super::{FRAGMENT_SHADER, VERTEX_SHADER};
}

pub mod text {
    pub use super::{
        KITTY_PLACEHOLDER, RenderContext, SELECTION_BG, build_dynamic_overlays,
        build_row_backgrounds, build_row_foregrounds, build_vertices, cell_colors, cursor_cell,
        prepare_atlas, rgba,
    };
}

pub mod image {
    pub use super::{bounded_image_vertex_buffer, build_low24_index, placeholder_image_id};
}
