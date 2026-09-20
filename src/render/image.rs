//! Kitty graphics protocol texture management, image quad rendering, and placeholder matching.

use glow::HasContext;

use crate::color::Color;
use crate::font::CellMetrics;
use crate::grid::Grid;
use crate::render::text::{KITTY_PLACEHOLDER, push_quad};
use crate::render::{RenderOptions, Renderer};

#[must_use]
pub fn placeholder_image_id(color: Color) -> u32 {
    match color {
        Color::Rgb(r, g, b) => ((r as u32) << 16) | ((g as u32) << 8) | (b as u32),
        Color::Indexed(idx) => idx as u32,
        _ => 0,
    }
}

impl Renderer {
    pub(crate) fn sync_image_textures(&mut self, grid: &mut Grid) {
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

        for (id, img) in &mut grid.images {
            if !self.image_textures.contains_key(id) {
                let ver = grid.image_versions.get(id).copied().unwrap_or(0);
                if let Some(rgba) = &img.rgba {
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
                                glow::PixelUnpackData::Slice(Some(rgba)),
                            );
                            self.image_textures
                                .insert(*id, (tex, img.width, img.height, ver));
                            img.rgba = None;
                        }
                    }
                }
            }
        }
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

    pub(crate) fn render_single_image(
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
}
