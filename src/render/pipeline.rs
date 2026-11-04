//! Deep module managing vertex geometry generation, incremental row caching, and atlas preparation.

use super::*;

/// Returns `true` if the incremental row cache should be invalidated and reallocated.
#[must_use]
pub fn row_cache_needs_reset(
    cached_rows: usize,
    current_rows: usize,
    last_cols: usize,
    current_cols: usize,
    padding_changed: bool,
) -> bool {
    cached_rows != current_rows || last_cols != current_cols || padding_changed
}

/// Prepared GPU drawing payload for a frame.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RenderBatch {
    /// Vertex buffer data containing backgrounds, text glyphs, and dynamic overlays.
    pub vertices: Vec<f32>,
    /// Number of static row vertices (backgrounds and foreground text) in the buffer.
    pub static_vertices_len: usize,
    /// Whether the GPU VBO requires a full upload (true on reallocate or invalidate) or can use subdata.
    pub vbo_full_upload: bool,
    /// Viewport dimensions in physical pixels: `[width, height]`.
    pub viewport_size: [i32; 2],
    /// Background clear color in RGBA floats: `[r, g, b, a]`.
    pub clear_color: [f32; 4],
    /// Whether the glyph atlas texture was repacked (requiring full texture upload and row cache reset).
    pub atlas_repacked: bool,
    /// Number of quads generated in this frame (`vertices.len() / 48`).
    pub quad_count: usize,
}

/// Deep module managing incremental row vertex caching, geometry assembly, and overlay rendering.
#[derive(Debug, Default)]
pub struct RenderPipeline {
    row_bg: Vec<Vec<f32>>,
    row_fg: Vec<Vec<f32>>,
    row_valid: Vec<bool>,
    batch: RenderBatch,
    last_cursor: Option<(usize, usize, usize)>,
    last_cursor_shape: Option<CursorShape>,
    last_selection: Option<Selection>,
    last_viewport_offset: usize,
    last_hovered_span: Option<HoveredHyperlinkSpan>,
    last_padding: [u16; 2],
    last_cols: usize,
    last_preedit: Option<Preedit>,
}

impl RenderPipeline {
    /// Creates a new `RenderPipeline` with cleared row caches.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Clears all cached per-row geometry across all rows.
    pub fn clear_cache(&mut self) {
        self.row_bg.clear();
        self.row_fg.clear();
        self.row_valid.clear();
        self.last_cols = 0;
        self.batch.vertices.clear();
        self.batch.static_vertices_len = 0;
        self.batch.vbo_full_upload = true;
        self.batch.quad_count = 0;
        self.last_preedit = None;
    }

    /// Read-only reference to the latest generated render batch.
    #[must_use]
    pub fn batch(&self) -> &RenderBatch {
        &self.batch
    }

    /// Read-only slice of vertex floats in the current batch.
    #[must_use]
    pub fn vertices(&self) -> &[f32] {
        &self.batch.vertices
    }

    /// Number of static row vertices in the current batch.
    #[must_use]
    pub fn static_vertices_len(&self) -> usize {
        self.batch.static_vertices_len
    }

    /// Clear color for the current batch: `[r, g, b, a]`.
    #[must_use]
    pub fn clear_color(&self) -> [f32; 4] {
        self.batch.clear_color
    }

    /// Viewport size in pixels for the current batch: `[width, height]`.
    #[must_use]
    pub fn viewport_size(&self) -> [i32; 2] {
        self.batch.viewport_size
    }

    /// Whether the current batch requires a full VBO upload.
    #[must_use]
    pub fn vbo_full_upload(&self) -> bool {
        self.batch.vbo_full_upload
    }

    /// Whether the glyph atlas was repacked during this frame.
    #[must_use]
    pub fn atlas_repacked(&self) -> bool {
        self.batch.atlas_repacked
    }

    /// Prepares a full frame render batch, incrementally regenerating only modified rows.
    ///
    /// # Errors
    /// Returns [`RenderError`] if viewport dimensions are invalid.
    pub fn prepare_frame(
        &mut self,
        grid: &mut Grid,
        colors: ColorScheme<'_>,
        fonts: &FontManager,
        atlas: &mut GlyphAtlas,
        size: [u32; 2],
        options: RenderOptions<'_>,
    ) -> Result<&RenderBatch, RenderError> {
        let [width, height] = native_size(size)?;
        let repacked = prepare_atlas(grid, fonts, atlas, options.preedit);
        if repacked {
            self.clear_cache();
            grid.mark_all_dirty();
        }

        self.build_incremental_vertices(grid, colors, fonts.metrics, fonts, atlas, options);

        self.batch.viewport_size = [width, height];
        self.batch.clear_color = rgba(colors.background);
        self.batch.atlas_repacked = repacked;
        self.batch.quad_count = self.batch.vertices.len() / 48;

        Ok(&self.batch)
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
            self.batch.static_vertices_len = 0;
            self.batch.vbo_full_upload = true;
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

        if any_row_regenerated || self.batch.static_vertices_len == 0 {
            let total_floats: usize = self.row_bg[..rows].iter().map(Vec::len).sum::<usize>()
                + self.row_fg[..rows].iter().map(Vec::len).sum::<usize>()
                + 192;
            self.batch.vertices.clear();
            self.batch.vertices.reserve(total_floats);
            // 1. All row backgrounds first (prevents lower row background from covering upper row descenders)
            for r in 0..rows {
                self.batch.vertices.extend_from_slice(&self.row_bg[r]);
            }
            // 2. All row foregrounds (glyphs, underlines, borders)
            for r in 0..rows {
                self.batch.vertices.extend_from_slice(&self.row_fg[r]);
            }
            self.batch.static_vertices_len = self.batch.vertices.len();
            // 3. Dynamic overlays (cursor, preedit)
            build_dynamic_overlays(&mut self.batch.vertices, &ctx);
            self.batch.vbo_full_upload = true;
        } else if overlays_changed {
            self.batch.vertices.truncate(self.batch.static_vertices_len);
            build_dynamic_overlays(&mut self.batch.vertices, &ctx);
            self.batch.vbo_full_upload = false;
        }

        self.last_cursor = cursor;
        self.last_selection = options.selection.cloned();
        self.last_hovered_span = options.hovered_span;
        self.last_preedit = options.preedit.cloned();
    }
}
