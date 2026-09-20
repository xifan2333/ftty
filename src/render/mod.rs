//! Wayland EGL ownership and batched OpenGL ES 3 terminal rendering.

pub mod box_drawing;
pub(crate) mod egl;
pub(crate) mod image;
pub(crate) mod shader;
pub mod text;

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use glow::HasContext;
use wayland_client::Connection;
use wayland_client::protocol::wl_surface::WlSurface;

use crate::color::Rgb;
use crate::error::RenderError;
use crate::font::{CellMetrics, FontManager, GlyphAtlas};
use crate::grid::{CursorShape, Grid};
use crate::input::ime::Preedit;
use crate::input::selection::Selection;

pub use box_drawing::{is_procedural_glyph, render_procedural_glyph};
pub(crate) use egl::EglContext;
pub use egl::native_size;
use shader::create_program;
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
use text::{
    RenderContext, build_dynamic_overlays, build_row_backgrounds, build_row_foregrounds,
    cursor_cell, prepare_atlas, rgba,
};

#[cfg(test)]
const DEFAULT_FG: Rgb = Rgb::new(220, 220, 220);
#[cfg(test)]
const DEFAULT_BG: Rgb = Rgb::new(24, 24, 24);

const MAX_RENDER_CACHE_ROWS: usize = 512;

/// Owns GL objects together with their EGL context, including on initialization failure.
pub struct Renderer {
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
    pub(crate) vertices: Vec<f32>,
    pub(crate) static_vertices_len: usize,
    pub(crate) vbo_full_upload: bool,
    pub(crate) row_bg: Vec<Vec<f32>>,
    pub(crate) row_fg: Vec<Vec<f32>>,
    pub(crate) row_valid: Vec<bool>,
    pub(crate) last_cursor: Option<(usize, usize, usize)>,
    pub(crate) last_cursor_shape: Option<CursorShape>,
    pub(crate) last_selection: Option<Selection>,
    pub(crate) last_viewport_offset: usize,
    pub(crate) last_hovered_span: Option<HoveredHyperlinkSpan>,
    pub(crate) last_padding: [u16; 2],
    pub(crate) last_cols: usize,
    pub(crate) last_preedit: Option<crate::input::ime::Preedit>,
    pub(crate) egl: EglContext,
}

#[must_use]
pub(crate) fn row_cache_needs_reset(
    cached_rows: usize,
    current_rows: usize,
    last_cols: usize,
    current_cols: usize,
    padding_changed: bool,
) -> bool {
    cached_rows != current_rows || last_cols != current_cols || padding_changed
}

impl Renderer {
    /// Clears cached per-row vertex geometry across all screens and styles.
    pub fn clear_cache(&mut self) {
        self.row_bg.clear();
        self.row_fg.clear();
        self.row_valid.clear();
        self.last_cols = 0;
        self.vertices = Vec::new();
        self.static_vertices_len = 0;
        self.vbo_full_upload = true;
        self.last_preedit = None;
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
            vertices: Vec::with_capacity(8192),
            static_vertices_len: 0,
            vbo_full_upload: true,
            row_bg: Vec::new(),
            row_fg: Vec::new(),
            row_valid: Vec::new(),
            last_cursor: None,
            last_cursor_shape: None,
            last_selection: None,
            last_viewport_offset: 0,
            last_hovered_span: None,
            last_padding: [0, 0],
            last_cols: 0,
            last_preedit: None,
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
            let bytes = std::slice::from_raw_parts(
                self.vertices.as_ptr().cast::<u8>(),
                std::mem::size_of_val(self.vertices.as_slice()),
            );
            if self.vbo_full_upload || bytes.len() > self.vbo_capacity {
                Self::upload_vbo(gl, self.vbo, &mut self.vbo_capacity, bytes);
            } else {
                let static_offset = self.static_vertices_len * std::mem::size_of::<f32>();
                if static_offset < bytes.len() {
                    let overlay_bytes = &bytes[static_offset..];
                    gl.bind_buffer(glow::ARRAY_BUFFER, self.vbo);
                    gl.buffer_sub_data_u8_slice(
                        glow::ARRAY_BUFFER,
                        static_offset as i32,
                        overlay_bytes,
                    );
                }
            }
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

    pub(crate) fn build_incremental_vertices(
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

        let cols_changed = self.last_cols != grid.cols;
        let rows = grid.rows.min(MAX_RENDER_CACHE_ROWS);
        if row_cache_needs_reset(
            self.row_valid.len(),
            rows,
            self.last_cols,
            grid.cols,
            padding_changed,
        ) {
            self.row_bg = vec![Vec::new(); rows];
            self.row_fg = vec![Vec::new(); rows];
            self.row_valid = vec![false; rows];
            self.static_vertices_len = 0;
            self.vbo_full_upload = true;
        }
        self.last_cols = grid.cols;

        let ctx = RenderContext {
            grid,
            colors,
            metrics,
            fonts,
            atlas,
            options,
            cursor,
        };

        let mut any_row_regenerated = false;
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

            let cursor_affects_row =
                grid.cursor.shape == CursorShape::Block && (row_has_cursor || row_had_cursor);

            let needs_regen = !self.row_valid[r]
                || line.dirty.get()
                || viewport_changed
                || shape_changed
                || cols_changed
                || (cursor_changed && cursor_affects_row)
                || (selection_changed && (row_has_sel || row_had_sel))
                || (hover_changed && (row_has_hover || row_had_hover));

            if needs_regen {
                build_row_backgrounds(&mut self.row_bg[r], r, &ctx);
                build_row_foregrounds(&mut self.row_fg[r], r, &ctx);
                self.row_valid[r] = true;
                line.dirty.set(false);
                any_row_regenerated = true;
            }
        }

        let preedit_changed = self.last_preedit.as_ref() != options.preedit;

        let overlays_changed = cursor_changed
            || selection_changed
            || hover_changed
            || preedit_changed
            || viewport_changed
            || shape_changed
            || cols_changed
            || padding_changed;

        if any_row_regenerated || self.static_vertices_len == 0 {
            let total_floats: usize = self.row_bg[..rows].iter().map(Vec::len).sum::<usize>()
                + self.row_fg[..rows].iter().map(Vec::len).sum::<usize>()
                + 192;
            self.vertices.clear();
            self.vertices.reserve(total_floats);
            // 1. All row backgrounds first (prevents lower row background from covering upper row descenders)
            for r in 0..rows {
                self.vertices.extend_from_slice(&self.row_bg[r]);
            }
            // 2. All row foregrounds (glyphs, underlines, borders)
            for r in 0..rows {
                self.vertices.extend_from_slice(&self.row_fg[r]);
            }
            self.static_vertices_len = self.vertices.len();
            // 3. Dynamic overlays (cursor, preedit)
            build_dynamic_overlays(&mut self.vertices, &ctx);
            self.vbo_full_upload = true;
        } else if overlays_changed {
            self.vertices.truncate(self.static_vertices_len);
            build_dynamic_overlays(&mut self.vertices, &ctx);
            self.vbo_full_upload = false;
        }

        self.last_cursor = cursor;
        self.last_selection = options.selection.cloned();
        self.last_hovered_span = options.hovered_span;
        self.last_preedit = options.preedit.cloned();
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
