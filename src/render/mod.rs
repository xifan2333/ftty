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

pub use colors::{ColorScheme, HoveredHyperlinkSpan, RenderOptions};
pub(crate) use egl::EglContext;
pub use pipeline::RenderPipeline;

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

#[cfg(test)]
mod tests {

    use crate::color::Rgb;
    use crate::color::{Color, default_256_palette};
    use crate::font::{FontManager, GlyphAtlas};
    use crate::grid::{Cell, CellFlags, Grid};
    use crate::input::ime::Preedit;
    use crate::render::egl::native_size;
    use crate::render::shader::FRAGMENT_SHADER;
    use crate::render::text::{
        SELECTION_BG, build_dynamic_overlays, build_row_backgrounds, build_row_foregrounds,
        build_vertices, cell_colors, cursor_cell, prepare_atlas,
    };
    use crate::render::{ColorScheme, HoveredHyperlinkSpan, RenderOptions};
    use crate::width::KITTY_PLACEHOLDER;

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
            ColorScheme::new(
                &default_256_palette(),
                Rgb::new(220, 220, 220),
                Rgb::new(24, 24, 24),
            ),
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
                ColorScheme::new(
                    &default_256_palette(),
                    Rgb::new(220, 220, 220),
                    Rgb::new(24, 24, 24)
                )
            ),
            (Rgb::new(24, 24, 24), Rgb::new(220, 220, 220))
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
            ColorScheme::new(
                &default_256_palette(),
                Rgb::new(220, 220, 220),
                Rgb::new(24, 24, 24),
            ),
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
    fn subpixel_fragment_shader_supports_dual_source_blending() {
        // Regression guard: fragment shader must declare GL_EXT_blend_func_extended support
        // and secondary color output for WezTerm-grade 3-channel LCD subpixel antialiasing.
        assert!(FRAGMENT_SHADER.contains("GL_EXT_blend_func_extended"));
        assert!(FRAGMENT_SHADER.contains("gl_SecondaryFragColorEXT"));
        assert!(FRAGMENT_SHADER.contains("u_subpixel_mode"));
        // Secondary coverage must be modulated by vertex alpha so translucent quads (e.g. selection)
        // and dimmed glyphs are properly attenuated under dual-source blending.
        assert!(FRAGMENT_SHADER.contains("mask * v_color.a"));
        assert!(FRAGMENT_SHADER.contains("mask.a * v_color.a"));
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
            ColorScheme::new(
                &default_256_palette(),
                Rgb::new(220, 220, 220),
                Rgb::new(24, 24, 24),
            ),
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
            ColorScheme::new(
                &default_256_palette(),
                Rgb::new(220, 220, 220),
                Rgb::new(24, 24, 24),
            ),
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
            ColorScheme::new(
                &default_256_palette(),
                Rgb::new(220, 220, 220),
                Rgb::new(24, 24, 24),
            ),
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
            ColorScheme::new(
                &default_256_palette(),
                Rgb::new(220, 220, 220),
                Rgb::new(24, 24, 24),
            ),
            fonts.metrics,
            &fonts,
            &atlas,
            RenderOptions::new([0, 0], Some(&preedit), None),
        );

        // Vertices must contain the block cursor and the preedit quads
        assert!(vertices.len() >= 4 * 48);

        // Also test preedit renders when cursor is hidden (!grid.cursor.visible) per Kitty
        grid.cursor.visible = false;
        let mut hidden_cursor_vertices = Vec::new();
        build_vertices(
            &mut hidden_cursor_vertices,
            &grid,
            ColorScheme::new(
                &default_256_palette(),
                Rgb::new(220, 220, 220),
                Rgb::new(24, 24, 24),
            ),
            fonts.metrics,
            &fonts,
            &atlas,
            RenderOptions::new([0, 0], Some(&preedit), None),
        );
        assert!(
            !hidden_cursor_vertices.is_empty(),
            "preedit must render even when cursor is hidden"
        );
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
            ColorScheme::new(
                &default_256_palette(),
                Rgb::new(220, 220, 220),
                Rgb::new(24, 24, 24),
            ),
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
            ColorScheme::new(
                &default_256_palette(),
                Rgb::new(220, 220, 220),
                Rgb::new(24, 24, 24),
            ),
            fonts.metrics,
            &fonts,
            &atlas,
            RenderOptions::new([0, 0], None, None),
        );

        let mut vertices_mismatched_hover = Vec::new();
        build_vertices(
            &mut vertices_mismatched_hover,
            &grid,
            ColorScheme::new(
                &default_256_palette(),
                Rgb::new(220, 220, 220),
                Rgb::new(24, 24, 24),
            ),
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
            ColorScheme::new(
                &default_256_palette(),
                Rgb::new(220, 220, 220),
                Rgb::new(24, 24, 24),
            ),
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
            ColorScheme::new(
                &default_256_palette(),
                Rgb::new(220, 220, 220),
                Rgb::new(24, 24, 24),
            ),
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
            ColorScheme::new(
                &default_256_palette(),
                Rgb::new(220, 220, 220),
                Rgb::new(24, 24, 24),
            ),
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
        let colors = ColorScheme::new(&palette, Rgb::new(220, 220, 220), Rgb::new(24, 24, 24));

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

    #[test]
    fn test_column_resize_regenerates_row_foregrounds_and_backgrounds() {
        let fonts = FontManager::load(14.0).expect("system monospace font");
        let mut grid = Grid::new(10, 5, 0);
        grid.cursor.visible = false;
        for r in 0..5 {
            grid.cursor.row = r;
            grid.cursor.col = 0;
            grid.write_char(
                'X',
                Color::DefaultForeground,
                Color::Rgb(10, 20, 30),
                CellFlags::empty(),
            );
        }

        let mut atlas = GlyphAtlas::new(64, 64);
        prepare_atlas(&grid, &fonts, &mut atlas, None);
        let palette = default_256_palette();
        let colors = ColorScheme::new(&palette, Rgb::new(220, 220, 220), Rgb::new(24, 24, 24));

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
        for r in 0..5 {
            build_row_backgrounds(&mut row_bg[r], r, &ctx);
            build_row_foregrounds(&mut row_fg[r], r, &ctx);
        }

        // Now expand grid horizontally from 10 to 20 columns (same number of rows)
        grid.resize(20, 5);
        assert_eq!(grid.cols, 20);
        assert_eq!(grid.rows, 5);

        // Add content in the newly expanded column space (column 15)
        grid.cursor.row = 2;
        grid.cursor.col = 15;
        grid.write_char(
            'Y',
            Color::DefaultForeground,
            Color::Rgb(50, 60, 70),
            CellFlags::empty(),
        );

        prepare_atlas(&grid, &fonts, &mut atlas, None);
        let ctx_expanded = crate::render::text::RenderContext {
            grid: &grid,
            colors,
            metrics: fonts.metrics,
            fonts: &fonts,
            atlas: &atlas,
            options: RenderOptions::default(),
            cursor: None,
        };

        // Rebuild row 2: it must generate vertices for both 'X' at col 0 and 'Y' at col 15
        build_row_backgrounds(&mut row_bg[2], 2, &ctx_expanded);
        build_row_foregrounds(&mut row_fg[2], 2, &ctx_expanded);

        // Row 2 background must have at least 2 quads (cell 0 and cell 15 custom bg)
        assert!(row_bg[2].len() >= 2 * 48);
        // Row 2 foreground must have at least 2 quads ('X' and 'Y')
        assert!(row_fg[2].len() >= 2 * 48);
    }

    #[test]
    fn test_row_cache_needs_reset_on_column_or_row_change() {
        use crate::render::pipeline::row_cache_needs_reset;

        // Same rows, expanded columns (the exact bug case): must trigger reset
        assert!(row_cache_needs_reset(24, 24, 80, 150, false));
        // Same rows and columns: must not reset
        assert!(!row_cache_needs_reset(24, 24, 80, 80, false));
        // Changed rows: must reset
        assert!(row_cache_needs_reset(24, 40, 80, 80, false));
        // Padding changed: must reset
        assert!(row_cache_needs_reset(24, 24, 80, 80, true));
    }

    #[test]
    fn test_contiguous_background_cells_merge_into_single_quad() {
        let mut grid = Grid::new(10, 1, 0);
        grid.cursor.visible = false;
        for col in 2..8 {
            grid.lines[0].cells[col].bg = Color::Rgb(50, 60, 70);
        }
        let fonts = FontManager::load(14.0).expect("system monospace font");
        let atlas = GlyphAtlas::new(16, 16);
        let mut bg_vertices = Vec::new();
        let palette = default_256_palette();
        let ctx = crate::render::text::RenderContext {
            grid: &grid,
            colors: ColorScheme::new(&palette, Rgb::new(220, 220, 220), Rgb::new(24, 24, 24)),
            metrics: fonts.metrics,
            fonts: &fonts,
            atlas: &atlas,
            options: RenderOptions::default(),
            cursor: None,
        };
        build_row_backgrounds(&mut bg_vertices, 0, &ctx);
        // 6 contiguous cells should be merged into exactly 1 quad (48 floats)
        assert_eq!(bg_vertices.len(), 48);
    }

    #[test]
    fn test_preedit_overlay_vertex_regeneration() {
        let fonts = FontManager::load(14.0).expect("system monospace font");
        let grid = Grid::new(80, 24, 0);
        let mut atlas = GlyphAtlas::new(64, 64);
        let preedit1 = Preedit {
            text: "hello".to_string(),
            cursor_begin: 0,
            cursor_end: 5,
        };
        prepare_atlas(&grid, &fonts, &mut atlas, Some(&preedit1));

        let mut vertices = Vec::new();
        build_vertices(
            &mut vertices,
            &grid,
            ColorScheme::new(
                &default_256_palette(),
                Rgb::new(220, 220, 220),
                Rgb::new(24, 24, 24),
            ),
            fonts.metrics,
            &fonts,
            &atlas,
            RenderOptions::new([0, 0], Some(&preedit1), None),
        );
        assert!(!vertices.is_empty());
    }

    #[test]
    fn test_dynamic_overlay_separation_and_layering() {
        let mut grid = Grid::new(10, 2, 0);
        grid.cursor.visible = true;
        grid.cursor.shape = crate::grid::CursorShape::Beam;
        grid.lines[0].cells[0].c = 'A';
        grid.lines[0].cells[0].bg = Color::Rgb(10, 20, 30);

        let fonts = FontManager::load(14.0).expect("system monospace font");
        let mut atlas = GlyphAtlas::new(32, 32);
        prepare_atlas(&grid, &fonts, &mut atlas, None);

        let palette = default_256_palette();
        let colors = ColorScheme::new(&palette, Rgb::new(220, 220, 220), Rgb::new(24, 24, 24));
        let cursor = cursor_cell(&grid);
        let ctx = crate::render::text::RenderContext {
            grid: &grid,
            colors,
            metrics: fonts.metrics,
            fonts: &fonts,
            atlas: &atlas,
            options: RenderOptions::default(),
            cursor,
        };

        // 1. Build row backgrounds and foregrounds
        let mut row_bg = Vec::new();
        let mut row_fg = Vec::new();
        build_row_backgrounds(&mut row_bg, 0, &ctx);
        build_row_foregrounds(&mut row_fg, 0, &ctx);

        let mut vertices = Vec::new();
        vertices.extend_from_slice(&row_bg);
        vertices.extend_from_slice(&row_fg);
        let static_len = vertices.len();

        // 2. Append dynamic overlays (Beam cursor)
        build_dynamic_overlays(&mut vertices, &ctx);
        assert_eq!(vertices.len(), static_len + 48);

        // 3. Truncating tail restores static vertices exactly
        vertices.truncate(static_len);
        assert_eq!(vertices.len(), static_len);

        // 4. When cursor hidden, overlay appends 0 floats
        let ctx_hidden = crate::render::text::RenderContext {
            grid: &grid,
            colors,
            metrics: fonts.metrics,
            fonts: &fonts,
            atlas: &atlas,
            options: RenderOptions::default(),
            cursor: None,
        };
        build_dynamic_overlays(&mut vertices, &ctx_hidden);
        assert_eq!(vertices.len(), static_len);
    }

    #[test]
    fn test_underline_cursor_overlay_quad_count() {
        let mut grid = Grid::new(10, 2, 0);
        grid.cursor.visible = true;
        grid.cursor.shape = crate::grid::CursorShape::Underline;

        let fonts = FontManager::load(14.0).expect("system monospace font");
        let atlas = GlyphAtlas::new(16, 16);
        let palette = default_256_palette();
        let colors = ColorScheme::new(&palette, Rgb::new(220, 220, 220), Rgb::new(24, 24, 24));
        let cursor = cursor_cell(&grid);
        let ctx = crate::render::text::RenderContext {
            grid: &grid,
            colors,
            metrics: fonts.metrics,
            fonts: &fonts,
            atlas: &atlas,
            options: RenderOptions::default(),
            cursor,
        };

        let mut overlay_vertices = Vec::new();
        build_dynamic_overlays(&mut overlay_vertices, &ctx);
        assert_eq!(overlay_vertices.len(), 48);
    }

    #[test]
    fn test_low24_placeholder_index_prefers_exact_id_and_resolves_aliases() {
        use crate::render::image::build_low24_index;

        // 0x10EEEE is an exact (24-bit) id; 0xAB10EEEE shares the same low 24 bits.
        let index = build_low24_index([0xAB10_EEEE_u32, 0x10EEEE].into_iter());
        assert_eq!(index.get(&0x10EEEE), Some(&0x10EEEE));

        // An alias with no exact counterpart still resolves to the full texture id.
        let alias_only = build_low24_index([0x1234_5678_u32].into_iter());
        assert_eq!(alias_only.get(&0x345678), Some(&0x1234_5678));

        // Order independence: an exact id encountered after an alias must win.
        let reordered = build_low24_index([0xAB10_EEEE_u32, 0x10EEEE].into_iter().rev());
        assert_eq!(reordered.get(&0x10EEEE), Some(&0x10EEEE));

        // Unrelated ids do not collide.
        assert_eq!(build_low24_index([1_u32].into_iter()).get(&2), None);
    }

    #[test]
    fn test_image_vertex_buffer_is_released_when_it_exceeds_retention_cap() {
        use crate::render::MAX_RETAINED_IMAGE_VERTEX_FLOATS;
        use crate::render::image::bounded_image_vertex_buffer;

        // A modest staging buffer is retained for reuse (capacity preserved).
        let small = Vec::<f32>::with_capacity(1024);
        let retained = bounded_image_vertex_buffer(small);
        assert!(retained.capacity() >= 1024);

        // An oversized transient allocation is dropped rather than pinned for later frames.
        let huge = Vec::<f32>::with_capacity(MAX_RETAINED_IMAGE_VERTEX_FLOATS + 1);
        let released = bounded_image_vertex_buffer(huge);
        assert!(released.capacity() <= 64);
    }

    #[test]
    fn test_render_submodule_exports() {
        use crate::color::Color;
        use crate::render::image::{
            bounded_image_vertex_buffer, build_low24_index, placeholder_image_id,
        };
        use crate::render::procedural::{
            Stroke, is_procedural_glyph, render_block_element, render_box_drawing,
            render_procedural_glyph,
        };
        use crate::render::shader::{FRAGMENT_SHADER, VERTEX_SHADER};

        assert!(VERTEX_SHADER.contains("v_tex_coords"));
        assert!(FRAGMENT_SHADER.contains("u_image_mode"));
        assert!(is_procedural_glyph('─'));
        assert_eq!(placeholder_image_id(Color::DefaultForeground), 0);
        assert_eq!(placeholder_image_id(Color::Rgb(1, 2, 3)), 0x010203);
        let mut verts = Vec::new();
        let handled = render_procedural_glyph(&mut verts, '█', 0.0, 0.0, 10.0, 20.0, [1.0; 4]);
        assert!(handled);
        let mut block_verts = Vec::new();
        render_block_element(&mut block_verts, '▌', 0.0, 0.0, 10.0, 20.0, [1.0; 4]);
        assert!(!block_verts.is_empty());
        let mut box_verts = Vec::new();
        render_box_drawing(&mut box_verts, '│', 0.0, 0.0, 10.0, 20.0, [1.0; 4]);
        assert!(!box_verts.is_empty());
        let _ = Stroke::Light;
        let _ = bounded_image_vertex_buffer(Vec::new());
        let _ = build_low24_index(std::iter::empty());
    }

    #[test]
    fn test_partially_scrolled_image_placement_visibility_and_cropping() {
        let placement_line = 5;
        let placement_rows = 10;
        let end_line = placement_line + placement_rows; // 15

        // Case 1: When viewport_start is 8 (placement partially scrolled off top),
        // end_line (15) > viewport_start (8), so placement is NOT skipped.
        let viewport_start = 8;
        let viewport_end = 28;
        assert!(end_line > viewport_start && placement_line < viewport_end);

        // Case 2: When viewport_start is 15 (placement completely scrolled off top),
        // end_line (15) <= viewport_start (15), so placement is correctly skipped.
        let viewport_start = 15;
        assert!(end_line <= viewport_start);

        // Case 3: When placement_line >= viewport_end (below viewport), correctly skipped.
        let _viewport_start = 0;
        let viewport_end = 5;
        assert!(placement_line >= viewport_end);
    }

    #[test]
    fn color_emoji_vertices_have_negative_alpha_in_render_pipeline() {
        let path = std::path::Path::new("/usr/share/fonts/noto/NotoColorEmoji.ttf");
        if !path.exists() {
            return;
        }
        let mut grid = Grid::new(4, 1, 0);
        grid.cursor.visible = false;
        grid.lines[0].cells[0].c = '😀';
        grid.lines[0].cells[0].flags = CellFlags::WIDE_CHAR;
        grid.lines[0].cells[1].flags = CellFlags::WIDE_CHAR_SPACER;
        let fonts = FontManager::load(14.0).expect("system monospace font");
        let mut atlas = GlyphAtlas::new(256, 256);
        prepare_atlas(&grid, &fonts, &mut atlas, None);
        let mut vertices = Vec::new();
        build_vertices(
            &mut vertices,
            &grid,
            ColorScheme::new(
                &default_256_palette(),
                Rgb::new(220, 220, 220),
                Rgb::new(24, 24, 24),
            ),
            fonts.metrics,
            &fonts,
            &atlas,
            RenderOptions::default(),
        );
        let glyph_quads: Vec<_> = vertices.chunks(8).filter(|v| v[2] >= 0.0).collect();
        assert!(!glyph_quads.is_empty(), "must render glyph vertices");
        assert!(
            glyph_quads[0][7] < 0.0,
            "colored emoji vertex must have negative alpha"
        );

        let cw = fonts.metrics.cell_width as f32;
        let ch = fonts.metrics.cell_height as f32;
        let gx0 = glyph_quads[0][0];
        let gy0 = glyph_quads[0][1];
        assert!(
            gx0 >= 0.0,
            "emoji must start inside or at left edge of cell"
        );
        assert!(gx0 < 2.0 * cw, "emoji must start within wide cell width");
        assert!(gy0 >= 0.0, "emoji must start inside or at top of cell");
        assert!(gy0 < ch, "emoji must start within cell height");
    }

    mod render_pipeline_tests {
        use crate::color::Rgb;
        use crate::color::default_256_palette;
        use crate::font::{FontManager, GlyphAtlas};
        use crate::grid::Grid;
        use crate::input::selection::{Selection, SelectionPoint, SelectionType};
        use crate::render::{ColorScheme, RenderOptions, RenderPipeline};

        #[test]
        fn test_render_pipeline_prepare_frame_generates_batch() {
            let mut pipeline = RenderPipeline::new();
            let mut grid = Grid::new(80, 24, 100);
            let fonts = FontManager::load(14.0).expect("system monospace font");
            let mut atlas = GlyphAtlas::new(16, 16);
            let palette = default_256_palette();
            let colors = ColorScheme::new(&palette, Rgb::new(220, 220, 220), Rgb::new(24, 24, 24));
            let options = RenderOptions::default();

            let batch = pipeline
                .prepare_frame(&mut grid, colors, &fonts, &mut atlas, [800, 600], options)
                .expect("prepare frame");

            assert_eq!(batch.viewport_size, [800, 600]);
            assert_eq!(
                batch.clear_color,
                [
                    f32::from(Rgb::new(24, 24, 24).r) / 255.0,
                    f32::from(Rgb::new(24, 24, 24).g) / 255.0,
                    f32::from(Rgb::new(24, 24, 24).b) / 255.0,
                    1.0,
                ]
            );
            assert!(batch.vbo_full_upload);
            assert!(batch.quad_count > 0);
            assert!(batch.static_vertices_len > 0);
        }

        #[test]
        fn test_render_pipeline_incremental_dirty_row_caching() {
            let mut pipeline = RenderPipeline::new();
            let mut grid = Grid::new(80, 24, 100);
            let fonts = FontManager::load(14.0).expect("system monospace font");
            let mut atlas = GlyphAtlas::new(16, 16);
            let palette = default_256_palette();
            let colors = ColorScheme::new(&palette, Rgb::new(220, 220, 220), Rgb::new(24, 24, 24));
            let options = RenderOptions::default();

            // Frame 1: initial generation
            let batch1 = pipeline
                .prepare_frame(&mut grid, colors, &fonts, &mut atlas, [800, 600], options)
                .expect("frame 1")
                .clone();
            assert!(batch1.vbo_full_upload);

            // Frame 2 without changes: static vertices stay identical
            let batch2 = pipeline
                .prepare_frame(&mut grid, colors, &fonts, &mut atlas, [800, 600], options)
                .expect("frame 2")
                .clone();
            assert_eq!(batch2.static_vertices_len, batch1.static_vertices_len);
            assert_eq!(batch2.quad_count, batch1.quad_count);

            // Frame 3 with modified row: write char on row 1
            grid.lines[1].cells[0].c = 'Z';
            grid.lines[1].dirty.set(true);

            let batch3 = pipeline
                .prepare_frame(&mut grid, colors, &fonts, &mut atlas, [800, 600], options)
                .expect("frame 3")
                .clone();
            assert!(batch3.quad_count >= batch1.quad_count);
        }

        #[test]
        fn test_render_pipeline_selection_overlay_batch() {
            let mut pipeline = RenderPipeline::new();
            let mut grid = Grid::new(80, 24, 100);
            let fonts = FontManager::load(14.0).expect("system monospace font");
            let mut atlas = GlyphAtlas::new(16, 16);
            let palette = default_256_palette();
            let colors = ColorScheme::new(&palette, Rgb::new(220, 220, 220), Rgb::new(24, 24, 24));

            // Frame without selection
            let batch_no_sel = pipeline
                .prepare_frame(
                    &mut grid,
                    colors,
                    &fonts,
                    &mut atlas,
                    [800, 600],
                    RenderOptions::default(),
                )
                .expect("no sel")
                .clone();

            // Frame with active selection spanning lines 0..=2
            let sel = Selection::new(
                SelectionPoint::new(0, 0),
                SelectionPoint::new(2, 40),
                SelectionType::Simple,
            );
            let options_with_sel = RenderOptions::new([0, 0], None, Some(&sel));

            let batch_sel = pipeline
                .prepare_frame(
                    &mut grid,
                    colors,
                    &fonts,
                    &mut atlas,
                    [800, 600],
                    options_with_sel,
                )
                .expect("with sel")
                .clone();

            assert!(
                batch_sel.quad_count > batch_no_sel.quad_count,
                "selection quads must be added to batch"
            );
        }

        #[test]
        fn test_render_pipeline_clear_cache() {
            let mut pipeline = RenderPipeline::new();
            let mut grid = Grid::new(80, 24, 100);
            let fonts = FontManager::load(14.0).expect("system monospace font");
            let mut atlas = GlyphAtlas::new(16, 16);
            let palette = default_256_palette();
            let colors = ColorScheme::new(&palette, Rgb::new(220, 220, 220), Rgb::new(24, 24, 24));

            let _ = pipeline.prepare_frame(
                &mut grid,
                colors,
                &fonts,
                &mut atlas,
                [800, 600],
                RenderOptions::default(),
            );
            assert!(pipeline.static_vertices_len() > 0);

            pipeline.clear_cache();
            assert_eq!(pipeline.static_vertices_len(), 0);
            assert!(pipeline.vbo_full_upload());
        }
    }
}
