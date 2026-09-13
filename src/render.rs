//! Wayland EGL ownership and batched OpenGL ES 2 terminal rendering.

use std::io;

use glow::HasContext;
use khronos_egl as egl;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Proxy};
use wayland_egl::WlEglSurface;

use crate::color::Rgb;
use crate::font::{CellMetrics, FontManager, GlyphAtlas};
use crate::grid::{Cell, CellFlags, CursorShape, Grid};

const DEFAULT_FG: Rgb = Rgb::new(220, 220, 220);
const DEFAULT_BG: Rgb = Rgb::new(24, 24, 24);
const SOLID_UV: [[f32; 2]; 2] = [[-1.0, -1.0]; 2];

struct EglContext {
    egl: egl::DynamicInstance<egl::EGL1_5>,
    display: egl::Display,
    context: Option<egl::Context>,
    surface: Option<egl::Surface>,
    // Field order keeps the Wayland connection alive through native window destruction.
    window: WlEglSurface,
    _surface: WlSurface,
    _connection: Connection,
}

impl EglContext {
    fn new(surface: &WlSurface, connection: &Connection, size: [u32; 2]) -> io::Result<Self> {
        let [width, height] = native_size(size)?;
        let window = WlEglSurface::new(surface.id(), width, height).map_err(io::Error::other)?;
        // SAFETY: the library stays loaded in this instance for all EGL calls.
        let egl = unsafe { egl::DynamicInstance::<egl::EGL1_5>::load_required() }
            .map_err(io::Error::other)?;
        // SAFETY: connection owns the live libwayland display and is retained below.
        let display = unsafe { egl.get_display(connection.backend().display_ptr().cast()) }
            .ok_or_else(|| io::Error::other("eglGetDisplay failed"))?;
        egl.initialize(display).map_err(io::Error::other)?;
        // Own each handle as soon as it exists, including during failed initialization.
        let mut context = Self {
            egl,
            display,
            context: None,
            surface: None,
            window,
            _surface: surface.clone(),
            _connection: connection.clone(),
        };
        context
            .egl
            .bind_api(egl::OPENGL_ES_API)
            .map_err(io::Error::other)?;
        let config = context
            .egl
            .choose_first_config(
                display,
                &[
                    egl::SURFACE_TYPE,
                    egl::WINDOW_BIT,
                    egl::RENDERABLE_TYPE,
                    egl::OPENGL_ES2_BIT,
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
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other("no EGL configuration supports OpenGL ES 2"))?;
        context.context = Some(
            context
                .egl
                .create_context(
                    display,
                    config,
                    None,
                    &[egl::CONTEXT_CLIENT_VERSION, 2, egl::NONE],
                )
                .map_err(io::Error::other)?,
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
            .map_err(io::Error::other)?,
        );
        context.make_current()?;
        // Frame callbacks pace drawing; swapping must not block PTY and signal dispatch.
        context
            .egl
            .swap_interval(display, 0)
            .map_err(io::Error::other)?;
        Ok(context)
    }

    fn make_current(&self) -> io::Result<()> {
        self.egl
            .make_current(self.display, self.surface, self.surface, self.context)
            .map_err(io::Error::other)
    }
}

impl Drop for EglContext {
    fn drop(&mut self) {
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

fn native_size([width, height]: [u32; 2]) -> io::Result<[i32; 2]> {
    if width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid surface size",
        ));
    }
    Ok([width as i32, height as i32])
}

const VERTEX_SHADER: &str = r#"
attribute vec2 a_position;
attribute vec2 a_tex_coords;
attribute vec4 a_color;
uniform vec2 u_viewport;
uniform vec2 u_atlas_size;
varying mediump vec2 v_tex_coords;
varying lowp vec4 v_color;
void main() {
    v_tex_coords = a_tex_coords / u_atlas_size;
    v_color = a_color;
    gl_Position = vec4(a_position / u_viewport * vec2(2.0, -2.0) + vec2(-1.0, 1.0), 0.0, 1.0);
}
"#;

const FRAGMENT_SHADER: &str = r#"
precision mediump float;
varying mediump vec2 v_tex_coords;
varying lowp vec4 v_color;
uniform sampler2D u_texture;
void main() {
    float alpha = v_tex_coords.x < 0.0 ? 1.0 : texture2D(u_texture, v_tex_coords).a;
    gl_FragColor = vec4(v_color.rgb, v_color.a * alpha);
}
"#;

fn compile_shader(gl: &glow::Context, kind: u32, source: &str) -> io::Result<glow::Shader> {
    // SAFETY: callers hold the current EGL context.
    unsafe {
        let shader = gl.create_shader(kind).map_err(io::Error::other)?;
        gl.shader_source(shader, source);
        gl.compile_shader(shader);
        if !gl.get_shader_compile_status(shader) {
            let log = gl.get_shader_info_log(shader);
            gl.delete_shader(shader);
            return Err(io::Error::other(log));
        }
        Ok(shader)
    }
}

fn create_program(gl: &glow::Context) -> io::Result<glow::Program> {
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
                return Err(io::Error::other(error));
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
            return Err(io::Error::other(log));
        }
        Ok(program)
    }
}

/// Owns GL objects together with their EGL context, including on initialization failure.
pub struct Renderer {
    gl: glow::Context,
    program: Option<glow::Program>,
    vbo: Option<glow::Buffer>,
    texture: Option<glow::Texture>,
    viewport: Option<glow::UniformLocation>,
    atlas_size: Option<glow::UniformLocation>,
    vertices: Vec<f32>,
    egl: EglContext,
}

impl Renderer {
    /// Creates a renderer after the first XDG surface configure has been acknowledged.
    ///
    /// # Errors
    /// Returns an error if EGL, shaders, or GL resources cannot be initialized.
    pub fn new(surface: &WlSurface, connection: &Connection, size: [u32; 2]) -> io::Result<Self> {
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
        let mut renderer = Self {
            gl,
            program: None,
            vbo: None,
            texture: None,
            viewport: None,
            atlas_size: None,
            vertices: Vec::with_capacity(8192),
            egl,
        };
        // SAFETY: the owned EGL context is current for all initialization calls.
        unsafe {
            renderer.program = Some(create_program(&renderer.gl)?);
            renderer.vbo = Some(renderer.gl.create_buffer().map_err(io::Error::other)?);
            renderer.texture = Some(renderer.gl.create_texture().map_err(io::Error::other)?);
            let gl = &renderer.gl;
            let program = renderer.program.expect("initialized program");
            renderer.viewport = gl.get_uniform_location(program, "u_viewport");
            renderer.atlas_size = gl.get_uniform_location(program, "u_atlas_size");
            gl.use_program(Some(program));
            gl.uniform_1_i32(gl.get_uniform_location(program, "u_texture").as_ref(), 0);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, renderer.texture);
            for parameter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, glow::LINEAR as i32);
            }
            for parameter in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, glow::CLAMP_TO_EDGE as i32);
            }
            gl.enable(glow::BLEND);
            gl.blend_func_separate(
                glow::SRC_ALPHA,
                glow::ONE_MINUS_SRC_ALPHA,
                glow::ONE,
                glow::ONE_MINUS_SRC_ALPHA,
            );
        }
        Ok(renderer)
    }

    /// Resizes the native window; the caller updates the grid from the same dimensions.
    ///
    /// # Errors
    /// Returns an error for zero or unrepresentable dimensions.
    pub fn resize(&self, size: [u32; 2]) -> io::Result<()> {
        let [width, height] = native_size(size)?;
        self.egl.window.resize(width, height, 0, 0);
        Ok(())
    }

    /// Uploads newly cached glyphs and draws the current grid into the back buffer.
    ///
    /// # Errors
    /// Returns an error if the context cannot be made current or dimensions are invalid.
    pub fn render_grid(
        &mut self,
        grid: &Grid,
        palette: &[Rgb; 256],
        fonts: &FontManager,
        atlas: &mut GlyphAtlas,
        size: [u32; 2],
    ) -> io::Result<()> {
        let [width, height] = native_size(size)?;
        self.egl.make_current()?;
        prepare_atlas(grid, fonts, atlas);
        build_vertices(
            &mut self.vertices,
            grid,
            palette,
            fonts.metrics,
            fonts,
            atlas,
        );
        // SAFETY: this renderer owns the current context and all referenced GL objects.
        unsafe {
            let gl = &self.gl;
            gl.viewport(0, 0, width, height);
            let [r, g, b, a] = rgba(DEFAULT_BG);
            gl.clear_color(r, g, b, a);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, self.texture);
            if atlas.dirty {
                gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::ALPHA as i32,
                    atlas.width as i32,
                    atlas.height as i32,
                    0,
                    glow::ALPHA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(&atlas.pixels)),
                );
                atlas.dirty = false;
            }
            gl.use_program(self.program);
            gl.uniform_2_f32(self.viewport.as_ref(), width as f32, height as f32);
            gl.uniform_2_f32(
                self.atlas_size.as_ref(),
                atlas.width as f32,
                atlas.height as f32,
            );
            gl.bind_buffer(glow::ARRAY_BUFFER, self.vbo);
            // f32 has no padding, and the slice covers exactly the initialized vertex data.
            let bytes = std::slice::from_raw_parts(
                self.vertices.as_ptr().cast::<u8>(),
                std::mem::size_of_val(self.vertices.as_slice()),
            );
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STREAM_DRAW);
            let stride = 8 * std::mem::size_of::<f32>() as i32;
            for (index, count, offset) in [(0, 2, 0), (1, 2, 8), (2, 4, 16)] {
                gl.enable_vertex_attrib_array(index);
                gl.vertex_attrib_pointer_f32(index, count, glow::FLOAT, false, stride, offset);
            }
            gl.draw_arrays(glow::TRIANGLES, 0, (self.vertices.len() / 8) as i32);
        }
        Ok(())
    }

    /// Presents the frame after the caller requests a Wayland frame callback.
    ///
    /// # Errors
    /// Returns an error if EGL cannot present the buffer.
    pub fn present(&self) -> io::Result<()> {
        self.egl
            .egl
            .swap_buffers(
                self.egl.display,
                self.egl.surface.expect("initialized EGL surface"),
            )
            .map_err(io::Error::other)
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
                if let Some(texture) = self.texture {
                    self.gl.delete_texture(texture);
                }
            }
        }
    }
}

fn visible_glyph(cell: &Cell) -> bool {
    cell.c != ' '
        && !cell
            .flags
            .intersects(CellFlags::HIDDEN | CellFlags::WIDE_CHAR_SPACER)
}

fn prepare_atlas(grid: &Grid, fonts: &FontManager, atlas: &mut GlyphAtlas) {
    for attempt in 0..2 {
        let mut full = false;
        for cell in grid
            .lines
            .iter()
            .flat_map(|row| &row.cells)
            .filter(|cell| visible_glyph(cell))
        {
            full |= atlas.get_or_insert(cell.c, cell.flags, fonts).is_none();
        }
        if !full || attempt == 1 {
            break;
        }
        // Repack only at a frame boundary, before generating any vertices or uploading pixels.
        atlas.clear();
    }
}

fn rgba(color: Rgb) -> [f32; 4] {
    [
        f32::from(color.r) / 255.0,
        f32::from(color.g) / 255.0,
        f32::from(color.b) / 255.0,
        1.0,
    ]
}

fn cell_colors(cell: &Cell, palette: &[Rgb; 256]) -> (Rgb, Rgb) {
    let fg = cell.fg.to_rgb(palette, DEFAULT_FG, DEFAULT_BG);
    let bg = cell.bg.to_rgb(palette, DEFAULT_FG, DEFAULT_BG);
    if cell.flags.contains(CellFlags::REVERSE) {
        (bg, fg)
    } else {
        (fg, bg)
    }
}

fn push_quad(
    vertices: &mut Vec<f32>,
    [x0, y0, x1, y1]: [f32; 4],
    [[u0, v0], [u1, v1]]: [[f32; 2]; 2],
    [r, g, b, a]: [f32; 4],
) {
    for [x, y, u, v] in [
        [x0, y0, u0, v0],
        [x1, y0, u1, v0],
        [x0, y1, u0, v1],
        [x1, y0, u1, v0],
        [x1, y1, u1, v1],
        [x0, y1, u0, v1],
    ] {
        vertices.extend_from_slice(&[x, y, u, v, r, g, b, a]);
    }
}

fn cursor_cell(grid: &Grid) -> Option<(usize, usize, usize)> {
    if !grid.cursor.visible || grid.cursor.row >= grid.rows {
        return None;
    }
    let row = grid.cursor.row;
    // The grid keeps col == cols while a wrap is pending; display the cursor at the edge.
    let mut col = grid.cursor.col.min(grid.cols - 1);
    if col > 0
        && grid.lines[row].cells[col]
            .flags
            .contains(CellFlags::WIDE_CHAR_SPACER)
    {
        col -= 1;
    }
    let width = if grid.lines[row].cells[col]
        .flags
        .contains(CellFlags::WIDE_CHAR)
    {
        2.min(grid.cols - col)
    } else {
        1
    };
    Some((row, col, width))
}

fn build_vertices(
    vertices: &mut Vec<f32>,
    grid: &Grid,
    palette: &[Rgb; 256],
    metrics: CellMetrics,
    fonts: &FontManager,
    atlas: &GlyphAtlas,
) {
    vertices.clear();
    let cw = metrics.cell_width as f32;
    let ch = metrics.cell_height as f32;
    let cursor = cursor_cell(grid);

    // Draw every background first so spacer cells cannot cover wide or overhanging glyphs.
    for (row, line) in grid.lines.iter().enumerate() {
        for (col, cell) in line.cells.iter().enumerate() {
            let (_, bg) = cell_colors(cell, palette);
            if bg != DEFAULT_BG {
                let x = col as f32 * cw;
                let y = row as f32 * ch;
                push_quad(vertices, [x, y, x + cw, y + ch], SOLID_UV, rgba(bg));
            }
        }
    }
    if let Some((row, col, width)) = cursor
        && grid.cursor.shape == CursorShape::Block
    {
        let x = col as f32 * cw;
        let y = row as f32 * ch;
        push_quad(
            vertices,
            [x, y, x + width as f32 * cw, y + ch],
            SOLID_UV,
            rgba(DEFAULT_FG),
        );
    }

    for (row, line) in grid.lines.iter().enumerate() {
        for (col, cell) in line.cells.iter().enumerate() {
            if cell
                .flags
                .intersects(CellFlags::HIDDEN | CellFlags::WIDE_CHAR_SPACER)
            {
                continue;
            }
            let x = col as f32 * cw;
            let y = row as f32 * ch;
            let (fg, _) = cell_colors(cell, palette);
            let under_block = grid.cursor.shape == CursorShape::Block
                && cursor.is_some_and(|(r, c, width)| row == r && col >= c && col < c + width);
            let mut color = rgba(if under_block { DEFAULT_BG } else { fg });
            if cell.flags.contains(CellFlags::DIM) {
                color[3] = 0.6;
            }
            if visible_glyph(cell)
                && let Some(glyph) = atlas.get(cell.c, cell.flags, fonts)
                && glyph.width > 0
                && glyph.height > 0
            {
                let gx = x + glyph.offset_x as f32;
                let gy = y + metrics.ascent as f32 - glyph.offset_y as f32 - glyph.height as f32;
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
            let width = if cell.flags.contains(CellFlags::WIDE_CHAR) {
                2.0 * cw
            } else {
                cw
            };
            if cell.flags.contains(CellFlags::UNDERLINE) {
                let top = y + (metrics.ascent as f32 + 1.0).min(ch - 1.0);
                push_quad(vertices, [x, top, x + width, top + 1.0], SOLID_UV, color);
            }
            if cell.flags.contains(CellFlags::STRIKETHROUGH) {
                let top = y + (metrics.ascent as f32 * 0.65).floor();
                push_quad(vertices, [x, top, x + width, top + 1.0], SOLID_UV, color);
            }
        }
    }

    if let Some((row, col, width)) = cursor {
        let x = col as f32 * cw;
        let y = row as f32 * ch;
        let rect = match grid.cursor.shape {
            CursorShape::Block => return,
            CursorShape::Beam => [x, y, x + 2.0_f32.min(cw), y + ch],
            CursorShape::Underline => [x, y + (ch - 2.0).max(0.0), x + width as f32 * cw, y + ch],
        };
        push_quad(vertices, rect, SOLID_UV, rgba(DEFAULT_FG));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{Color, default_256_palette};

    fn frame(grid: &Grid) -> (Vec<f32>, GlyphAtlas) {
        let fonts = FontManager::load(14.0).expect("system monospace font");
        let mut atlas = GlyphAtlas::new(16, 16);
        prepare_atlas(grid, &fonts, &mut atlas);
        let mut vertices = Vec::new();
        build_vertices(
            &mut vertices,
            grid,
            &default_256_palette(),
            fonts.metrics,
            &fonts,
            &atlas,
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
            cell_colors(&cell, &default_256_palette()),
            (DEFAULT_BG, DEFAULT_FG)
        );
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
    fn invalid_native_dimensions_are_rejected() {
        for size in [[0, 1], [1, 0], [u32::MAX, 1], [1, u32::MAX]] {
            assert!(native_size(size).is_err());
        }
        assert_eq!(native_size([720, 480]).unwrap(), [720, 480]);
    }
}
