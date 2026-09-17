//! Wayland EGL ownership and batched OpenGL ES 2 terminal rendering.

pub mod box_drawing;
pub mod text;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::io;

use glow::HasContext;
use khronos_egl as egl;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Proxy};
use wayland_egl::WlEglSurface;

use crate::color::{Color, Rgb};
use crate::font::{CellMetrics, FontManager, GlyphAtlas};
use crate::grid::{CursorShape, Grid};
use crate::ime::Preedit;
use crate::render::text::{
    KITTY_PLACEHOLDER, RenderContext, build_dynamic_overlays, build_row_backgrounds,
    build_row_foregrounds, cursor_cell, prepare_atlas, push_quad, rgba,
};
use crate::selection::Selection;

pub use box_drawing::{is_procedural_glyph, render_procedural_glyph};

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

#[cfg(test)]
const DEFAULT_FG: Rgb = Rgb::new(220, 220, 220);
#[cfg(test)]
const DEFAULT_BG: Rgb = Rgb::new(24, 24, 24);

struct EglContext {
    egl: egl::DynamicInstance<egl::EGL1_5>,
    display: egl::Display,
    context: Option<egl::Context>,
    surface: Option<egl::Surface>,
    initialized: bool,
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
            initialized: false,
            window,
            _surface: surface.clone(),
            _connection: connection.clone(),
        };

        let init_result = (|| -> io::Result<()> {
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

    fn make_current(&self) -> io::Result<()> {
        self.egl
            .make_current(self.display, self.surface, self.surface, self.context)
            .map_err(io::Error::other)
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

pub fn native_size([width, height]: [u32; 2]) -> io::Result<[i32; 2]> {
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
// 0 = single-channel glyph coverage, 1 = RGBA kitty image placement.
uniform int u_image_mode;
void main() {
    if (u_image_mode == 1) {
        vec4 texel = texture2D(u_texture, v_tex_coords);
        gl_FragColor = vec4(texel.rgb, texel.a * v_color.a);
    } else {
        float alpha = v_tex_coords.x < 0.0 ? 1.0 : texture2D(u_texture, v_tex_coords).a;
        gl_FragColor = vec4(v_color.rgb, v_color.a * alpha);
    }
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

const MAX_RENDER_CACHE_ROWS: usize = 512;

/// Owns GL objects together with their EGL context, including on initialization failure.
pub struct Renderer {
    gl: glow::Context,
    program: Option<glow::Program>,
    vbo: Option<glow::Buffer>,
    texture: Option<glow::Texture>,
    viewport: Option<glow::UniformLocation>,
    atlas_size: Option<glow::UniformLocation>,
    image_mode: Option<glow::UniformLocation>,
    image_textures: HashMap<u32, (glow::Texture, u32, u32, u64)>,
    vertices: Vec<f32>,
    row_bg: Vec<Vec<f32>>,
    row_fg: Vec<Vec<f32>>,
    row_valid: Vec<bool>,
    last_cursor: Option<(usize, usize, usize)>,
    last_cursor_shape: Option<CursorShape>,
    last_selection: Option<Selection>,
    last_viewport_offset: usize,
    last_hovered_span: Option<HoveredHyperlinkSpan>,
    last_padding: [u16; 2],
    egl: EglContext,
}

impl Renderer {
    /// Clears cached per-row vertex geometry across all screens and styles.
    pub fn clear_cache(&mut self) {
        self.row_bg.clear();
        self.row_fg.clear();
        self.row_valid.clear();
    }

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
            image_mode: None,
            image_textures: HashMap::new(),
            vertices: Vec::with_capacity(8192),
            row_bg: Vec::new(),
            row_fg: Vec::new(),
            row_valid: Vec::new(),
            last_cursor: None,
            last_cursor_shape: None,
            last_selection: None,
            last_viewport_offset: 0,
            last_hovered_span: None,
            last_padding: [0, 0],
            egl,
        };
        // SAFETY: the owned EGL context is current for all initialization calls.
        unsafe {
            let program = create_program(&renderer.gl)?;
            renderer.program = Some(program);
            renderer.vbo = Some(renderer.gl.create_buffer().map_err(io::Error::other)?);
            renderer.texture = Some(renderer.gl.create_texture().map_err(io::Error::other)?);
            let gl = &renderer.gl;
            renderer.viewport = gl.get_uniform_location(program, "u_viewport");
            renderer.atlas_size = gl.get_uniform_location(program, "u_atlas_size");
            renderer.image_mode = gl.get_uniform_location(program, "u_image_mode");
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
    pub fn resize(&mut self, size: [u32; 2]) -> io::Result<()> {
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
        grid: &Grid,
        colors: ColorScheme<'_>,
        fonts: &FontManager,
        atlas: &mut GlyphAtlas,
        size: [u32; 2],
        options: RenderOptions<'_>,
    ) -> io::Result<()> {
        let [width, height] = native_size(size)?;
        self.egl.make_current()?;
        let repacked = prepare_atlas(grid, fonts, atlas, options.preedit);
        if repacked {
            self.clear_cache();
            grid.mark_all_dirty();
        }
        self.build_incremental_vertices(grid, colors, fonts.metrics, fonts, atlas, options);
        self.sync_image_textures(grid);

        // SAFETY: this renderer owns the current context and all referenced GL objects.
        unsafe {
            let gl = &self.gl;
            gl.viewport(0, 0, width, height);
            let [r, g, b, a] = rgba(colors.background);
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
            gl.uniform_1_i32(self.image_mode.as_ref(), 0);
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

        // Pass 3: z >= 0 images (above text)
        self.render_image_placements(grid, false, fonts.metrics, options);
        self.render_unicode_placeholders(grid, fonts.metrics, options);
        Ok(())
    }

    fn sync_image_textures(&mut self, grid: &Grid) {
        let gl = &self.gl;
        let mut to_delete = Vec::new();
        self.image_textures.retain(|id, (tex, _, _, ver)| {
            if let Some(_img) = grid.images.get(id) {
                let current_ver = grid.image_versions.get(id).copied().unwrap_or(0);
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

        for (id, img) in &grid.images {
            if !self.image_textures.contains_key(id) {
                let ver = grid.image_versions.get(id).copied().unwrap_or(0);
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
                            img.width as i32,
                            img.height as i32,
                            0,
                            glow::RGBA,
                            glow::UNSIGNED_BYTE,
                            glow::PixelUnpackData::Slice(Some(&img.rgba)),
                        );
                        self.image_textures
                            .insert(*id, (tex, img.width, img.height, ver));
                    }
                }
            }
        }
    }

    fn render_image_placements(
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

        for placement in &grid.placements {
            let is_match = if z_negative {
                placement.z_index < 0
            } else {
                placement.z_index >= 0
            };
            if !is_match {
                continue;
            }

            if placement.line < viewport_start || placement.line >= viewport_end {
                continue;
            }

            let Some(&(tex, img_w, img_h, _)) = self.image_textures.get(&placement.image_id) else {
                continue;
            };

            let screen_row = placement.line - viewport_start;
            let x0 = pad_x + placement.col as f32 * cw + placement.offset_x as f32;
            let y0 = pad_y + screen_row as f32 * ch + placement.offset_y as f32;
            let x1 = x0 + placement.cols as f32 * cw;
            let y1 = y0 + placement.rows as f32 * ch;

            self.render_single_image(tex, img_w as f32, img_h as f32, [x0, y0, x1, y1]);
        }
    }

    fn render_single_image(
        &mut self,
        tex: glow::Texture,
        img_w: f32,
        img_h: f32,
        [x0, y0, x1, y1]: [f32; 4],
    ) {
        let mut img_vertices = Vec::with_capacity(48);
        push_quad(
            &mut img_vertices,
            [x0, y0, x1, y1],
            [[0.0, 0.0], [img_w, img_h]],
            [1.0, 1.0, 1.0, 1.0],
        );
        self.render_image_quads(tex, img_w, img_h, &img_vertices);
    }

    fn render_image_quads(&mut self, tex: glow::Texture, img_w: f32, img_h: f32, vertices: &[f32]) {
        let gl = &self.gl;
        // SAFETY: draw holds this renderer's current EGL context; tex and the VBO
        // belong to it. vertices contains initialized f32 values with no padding,
        // and remains live throughout the byte upload.
        unsafe {
            gl.use_program(self.program);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.uniform_1_i32(self.image_mode.as_ref(), 1);
            gl.uniform_2_f32(self.atlas_size.as_ref(), img_w, img_h);

            gl.bind_buffer(glow::ARRAY_BUFFER, self.vbo);
            let bytes = std::slice::from_raw_parts(
                vertices.as_ptr().cast::<u8>(),
                std::mem::size_of_val(vertices),
            );
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STREAM_DRAW);

            let stride = 8 * std::mem::size_of::<f32>() as i32;
            for (index, count, offset) in [(0, 2, 0), (1, 2, 8), (2, 4, 16)] {
                gl.enable_vertex_attrib_array(index);
                gl.vertex_attrib_pointer_f32(index, count, glow::FLOAT, false, stride, offset);
            }
            gl.draw_arrays(glow::TRIANGLES, 0, (vertices.len() / 8) as i32);
        }
    }

    fn render_unicode_placeholders(
        &mut self,
        grid: &Grid,
        metrics: CellMetrics,
        options: RenderOptions<'_>,
    ) {
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

        for row in 0..grid.rows {
            let line = grid.visible_line(row);
            let mut row_segments: Vec<(u32, usize, usize)> = Vec::new();
            let mut current_run: Option<(u32, usize, usize)> = None;

            for (col, cell) in line.cells.iter().enumerate() {
                if cell.c == KITTY_PLACEHOLDER {
                    let id_low24 = placeholder_image_id(cell.fg) & 0x00FF_FFFF;
                    if id_low24 != 0 {
                        let real_id = if self.image_textures.contains_key(&id_low24) {
                            Some(id_low24)
                        } else {
                            self.image_textures
                                .keys()
                                .find(|&&k| (k & 0x00FF_FFFF) == id_low24)
                                .copied()
                        };

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
                .virtual_placements
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
                let num_cells = box_w * box_h;
                let mut img_vertices = Vec::with_capacity(num_cells * 48);

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
            }
        }
    }

    /// Presents the frame after the caller requests a Wayland frame callback.
    ///
    /// # Errors
    /// Returns an error if EGL cannot present the buffer.
    pub fn present(&self) -> io::Result<()> {
        let surface = self
            .egl
            .surface
            .ok_or_else(|| io::Error::other("EGL window surface is not initialized"))?;
        self.egl
            .egl
            .swap_buffers(self.egl.display, surface)
            .map_err(io::Error::other)
    }

    fn build_incremental_vertices(
        &mut self,
        grid: &Grid,
        colors: ColorScheme<'_>,
        metrics: CellMetrics,
        fonts: &FontManager,
        atlas: &GlyphAtlas,
        options: RenderOptions<'_>,
    ) {
        let cursor = cursor_cell(grid);
        let viewport_offset = grid.viewport_offset();
        let viewport_changed = self.last_viewport_offset != viewport_offset;
        self.last_viewport_offset = viewport_offset;

        let padding_changed = self.last_padding != options.padding;
        self.last_padding = options.padding;

        let cursor_shape = Some(grid.cursor.shape);
        let shape_changed = self.last_cursor_shape != cursor_shape;
        self.last_cursor_shape = cursor_shape;

        let cursor_changed = self.last_cursor != cursor;
        let selection_changed = self.last_selection.as_ref() != options.selection;
        let hover_changed = self.last_hovered_span != options.hovered_span;

        let rows = grid.rows.min(MAX_RENDER_CACHE_ROWS);
        if self.row_valid.len() != rows || padding_changed {
            self.row_bg = vec![Vec::new(); rows];
            self.row_fg = vec![Vec::new(); rows];
            self.row_valid = vec![false; rows];
        }

        let ctx = RenderContext {
            grid,
            colors,
            metrics,
            fonts,
            atlas,
            options,
            cursor,
        };

        for r in 0..rows {
            let line = grid.visible_line(r);
            let abs_line = grid.scrollback.len() + r - viewport_offset;

            let row_has_cursor = cursor.is_some_and(|(cr, _, _)| cr == r);
            let row_had_cursor = self.last_cursor.is_some_and(|(cr, _, _)| cr == r);
            let row_has_sel = options.selection.is_some_and(|s| s.spans_line(abs_line));
            let row_had_sel = self
                .last_selection
                .as_ref()
                .is_some_and(|s| s.spans_line(abs_line));
            let row_has_hover = options
                .hovered_span
                .is_some_and(|span| span.line == abs_line);
            let row_had_hover = self
                .last_hovered_span
                .is_some_and(|span| span.line == abs_line);

            let needs_regen = !self.row_valid[r]
                || line.dirty.get()
                || viewport_changed
                || shape_changed
                || (cursor_changed && (row_has_cursor || row_had_cursor))
                || (selection_changed && (row_has_sel || row_had_sel))
                || (hover_changed && (row_has_hover || row_had_hover));

            if needs_regen {
                build_row_backgrounds(&mut self.row_bg[r], r, &ctx);
                build_row_foregrounds(&mut self.row_fg[r], r, &ctx);
                self.row_valid[r] = true;
                line.dirty.set(false);
            }
        }

        self.vertices.clear();
        // 1. All row backgrounds first (prevents lower row background from covering upper row descenders)
        for r in 0..rows {
            self.vertices.extend_from_slice(&self.row_bg[r]);
        }
        // 2. All row foregrounds (glyphs, underlines, borders)
        for r in 0..rows {
            self.vertices.extend_from_slice(&self.row_fg[r]);
        }
        // 3. Dynamic overlays (cursor, preedit)
        build_dynamic_overlays(&mut self.vertices, &ctx);

        self.last_cursor = cursor;
        self.last_selection = options.selection.cloned();
        self.last_hovered_span = options.hovered_span;
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
                for (_, (tex, _, _, _)) in self.image_textures.drain() {
                    self.gl.delete_texture(tex);
                }
            }
        }
    }
}

fn placeholder_image_id(color: Color) -> u32 {
    match color {
        Color::Rgb(r, g, b) => ((r as u32) << 16) | ((g as u32) << 8) | (b as u32),
        Color::Indexed(idx) => idx as u32,
        _ => 0,
    }
}
