//! Wayland EGL ownership and batched OpenGL ES 2 terminal rendering.

use std::collections::HashMap;
use std::io;

use glow::HasContext;
use khronos_egl as egl;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Proxy};
use wayland_egl::WlEglSurface;

use crate::color::{Color, Rgb};
use crate::font::{CellMetrics, FontManager, GlyphAtlas};
use crate::grid::{Cell, CellFlags, CursorShape, Grid};
use crate::ime::Preedit;
use crate::selection::Selection;

#[cfg(test)]
const DEFAULT_FG: Rgb = Rgb::new(220, 220, 220);
#[cfg(test)]
const DEFAULT_BG: Rgb = Rgb::new(24, 24, 24);
const SOLID_UV: [[f32; 2]; 2] = [[-1.0, -1.0]; 2];
const SELECTION_BG: [f32; 4] = [0.35, 0.45, 0.70, 0.5];

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

/// Options controlling frame layout, window padding, active IME composition, and text selection.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderOptions<'a> {
    pub padding: [u16; 2],
    pub preedit: Option<&'a Preedit>,
    pub selection: Option<&'a Selection>,
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
        }
    }
}

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
            image_mode: None,
            image_textures: HashMap::new(),
            vertices: Vec::with_capacity(8192),
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
        colors: ColorScheme<'_>,
        fonts: &FontManager,
        atlas: &mut GlyphAtlas,
        size: [u32; 2],
        options: RenderOptions<'_>,
    ) -> io::Result<()> {
        let [width, height] = native_size(size)?;
        self.egl.make_current()?;
        prepare_atlas(grid, fonts, atlas, options.preedit);
        build_vertices(
            &mut self.vertices,
            grid,
            colors,
            fonts.metrics,
            fonts,
            atlas,
            options,
        );
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
        let gl = &self.gl;
        let mut img_vertices = Vec::with_capacity(48);
        push_quad(
            &mut img_vertices,
            [x0, y0, x1, y1],
            [[0.0, 0.0], [img_w, img_h]],
            [1.0, 1.0, 1.0, 1.0],
        );

        // SAFETY: draw holds this renderer's current EGL context; tex and the VBO
        // belong to it. img_vertices contains six initialized vertices of eight
        // f32 values, with no padding, and remains live throughout the byte upload.
        unsafe {
            gl.use_program(self.program);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.uniform_1_i32(self.image_mode.as_ref(), 1);
            gl.uniform_2_f32(self.atlas_size.as_ref(), img_w, img_h);

            gl.bind_buffer(glow::ARRAY_BUFFER, self.vbo);
            let bytes = std::slice::from_raw_parts(
                img_vertices.as_ptr().cast::<u8>(),
                std::mem::size_of_val(img_vertices.as_slice()),
            );
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STREAM_DRAW);

            let stride = 8 * std::mem::size_of::<f32>() as i32;
            for (index, count, offset) in [(0, 2, 0), (1, 2, 8), (2, 4, 16)] {
                gl.enable_vertex_attrib_array(index);
                gl.vertex_attrib_pointer_f32(index, count, glow::FLOAT, false, stride, offset);
            }
            gl.draw_arrays(glow::TRIANGLES, 0, 6);
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

        let mut boxes: HashMap<u32, (usize, usize, usize, usize)> = HashMap::new();

        for row in 0..grid.rows {
            let line = grid.visible_line(row);
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
                            boxes
                                .entry(matched_id)
                                .and_modify(|b| {
                                    b.0 = b.0.min(col);
                                    b.1 = b.1.min(row);
                                    b.2 = b.2.max(col);
                                    b.3 = b.3.max(row);
                                })
                                .or_insert((col, row, col, row));
                        }
                    }
                }
            }
        }

        for (id, (min_col, min_row, max_col, max_row)) in boxes {
            let Some(&(tex, img_w, img_h, _)) = self.image_textures.get(&id) else {
                continue;
            };
            let x0 = pad_x + min_col as f32 * cw;
            let y0 = pad_y + min_row as f32 * ch;
            let x1 = pad_x + (max_col + 1) as f32 * cw;
            let y1 = pad_y + (max_row + 1) as f32 * ch;

            self.render_single_image(tex, img_w as f32, img_h as f32, [x0, y0, x1, y1]);
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

const KITTY_PLACEHOLDER: char = '\u{10EEEE}';

fn placeholder_image_id(color: Color) -> u32 {
    match color {
        Color::Rgb(r, g, b) => ((r as u32) << 16) | ((g as u32) << 8) | (b as u32),
        Color::Indexed(idx) => idx as u32,
        _ => 0,
    }
}

fn visible_glyph(cell: &Cell) -> bool {
    cell.c != ' '
        && cell.c != KITTY_PLACEHOLDER
        && !cell
            .flags
            .intersects(CellFlags::HIDDEN | CellFlags::WIDE_CHAR_SPACER)
}

fn prepare_atlas(
    grid: &Grid,
    fonts: &FontManager,
    atlas: &mut GlyphAtlas,
    preedit: Option<&Preedit>,
) {
    for attempt in 0..2 {
        let mut full = false;
        // Pre-cache fallback glyph '?' so it is guaranteed available if the atlas fills.
        let _ = atlas.get_or_insert('?', CellFlags::empty(), fonts);
        if let Some(p) = preedit {
            for c in p.text.chars() {
                full |= atlas
                    .get_or_insert(c, CellFlags::UNDERLINE, fonts)
                    .is_none();
            }
        }
        for row in 0..grid.rows {
            let line = grid.visible_line(row);
            for cell in line.cells.iter().filter(|cell| visible_glyph(cell)) {
                full |= atlas.get_or_insert(cell.c, cell.flags, fonts).is_none();
            }
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

fn cell_colors(cell: &Cell, colors: ColorScheme<'_>) -> (Rgb, Rgb) {
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

fn build_vertices(
    vertices: &mut Vec<f32>,
    grid: &Grid,
    colors: ColorScheme<'_>,
    metrics: CellMetrics,
    fonts: &FontManager,
    atlas: &GlyphAtlas,
    options: RenderOptions<'_>,
) {
    vertices.clear();
    let cw = metrics.cell_width as f32;
    let ch = metrics.cell_height as f32;
    let pad_x = f32::from(options.padding[0]);
    let pad_y = f32::from(options.padding[1]);
    let cursor = cursor_cell(grid);

    // Draw every background first so spacer cells cannot cover wide or overhanging glyphs.
    for row in 0..grid.rows {
        let abs_line = grid.scrollback.len() + row - grid.viewport_offset;
        let line = grid.visible_line(row);
        for (col, cell) in line.cells.iter().enumerate() {
            let (_, bg) = cell_colors(cell, colors);
            let x = pad_x + col as f32 * cw;
            let y = pad_y + row as f32 * ch;
            if bg != colors.background {
                push_quad(vertices, [x, y, x + cw, y + ch], SOLID_UV, rgba(bg));
            }
            if options.selection.is_some_and(|s| s.contains(abs_line, col)) {
                push_quad(vertices, [x, y, x + cw, y + ch], SOLID_UV, SELECTION_BG);
            }
        }
    }
    if let Some((row, col, width)) = cursor
        && grid.cursor.shape == CursorShape::Block
    {
        let x = pad_x + col as f32 * cw;
        let y = pad_y + row as f32 * ch;
        push_quad(
            vertices,
            [x, y, x + width as f32 * cw, y + ch],
            SOLID_UV,
            rgba(colors.foreground),
        );
    }

    for row in 0..grid.rows {
        let line = grid.visible_line(row);
        for (col, cell) in line.cells.iter().enumerate() {
            if cell
                .flags
                .intersects(CellFlags::HIDDEN | CellFlags::WIDE_CHAR_SPACER)
            {
                continue;
            }
            let x = pad_x + col as f32 * cw;
            let y = pad_y + row as f32 * ch;
            let (fg, _) = cell_colors(cell, colors);
            let under_block = grid.cursor.shape == CursorShape::Block
                && cursor.is_some_and(|(r, c, width)| row == r && col >= c && col < c + width);
            let mut color = rgba(if under_block { colors.background } else { fg });
            if cell.flags.contains(CellFlags::DIM) {
                color[3] = 0.6;
            }
            if visible_glyph(cell) {
                if let Some(glyph) = atlas.get(cell.c, cell.flags, fonts) {
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
        let x = pad_x + col as f32 * cw;
        let y = pad_y + row as f32 * ch;
        let rect = match grid.cursor.shape {
            CursorShape::Block => {
                // If preedit is active, don't early return so preedit can be drawn on top of the block
                [x, y, x + width as f32 * cw, y + ch]
            }
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

            // Draw preedit glyph, clipped to the cells that fit on this row so a wide
            // fallback glyph cannot bleed past the right edge of the terminal.
            let span_right = px + span_w;
            if let Some(glyph) = atlas.get(c, CellFlags::UNDERLINE, fonts)
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
mod tests {
    use super::*;
    use crate::color::{Color, default_256_palette};

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
}
