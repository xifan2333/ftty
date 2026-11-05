//! Wayland EGL ownership and batched OpenGL ES 3 terminal rendering.

use std::collections::HashMap;

use glow::HasContext;
use wayland_client::Connection;
use wayland_client::protocol::wl_surface::WlSurface;

use crate::error::RenderError;
use crate::font::{FontManager, GlyphAtlas};
use crate::grid::Grid;

use egl::native_size;
use shader::create_program;

mod colors;
pub mod egl;
pub mod image;
mod pipeline;
pub mod procedural;
pub mod shader;
pub mod text;

pub use colors::{ColorScheme, DEFAULT_BG, DEFAULT_FG, HoveredHyperlinkSpan, RenderOptions};
pub(crate) use egl::EglContext;
pub use pipeline::{RenderBatch, RenderPipeline, row_cache_needs_reset};

const MAX_RENDER_CACHE_ROWS: usize = 512;

// Retained capacity cap for the image vertex staging buffer. A single dense placeholder frame can
// grow the buffer to tens of thousands of floats; without a cap that peak allocation would stay
// resident forever. Anything above the cap is dropped at the end of the draw instead of recycled.
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
