//! Subsystem for managing Kitty graphics image decodes, memory budgets, and on-screen placements.

use std::collections::{HashMap, HashSet};

use crate::color::Color;
use crate::grid::Row;
use crate::kitty::{
    DeleteTarget, ImageData, ImagePlacement, KittyAction, KittyEvent, kitty_response_full,
};

/// Sentinel Unicode character identifying Kitty virtual image placeholders.
pub const KITTY_PLACEHOLDER: char = '\u{10EEEE}';

/// Maximum number of active image placements tracked in the grid.
pub const MAX_PLACEMENTS: usize = 1024;

/// Maximum number of unique Kitty images retained in memory.
pub const MAX_STORED_IMAGES: usize = 256;

/// Total memory budget (64 MiB) for stored image RGBA payloads.
pub const MAX_STORED_IMAGE_BYTES: usize = 64 * 1024 * 1024;

/// Layout and cursor context for processing Kitty graphics placement events.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GraphicContext {
    pub cell_width: f32,
    pub cell_height: f32,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub scrollback_len: usize,
}

/// Semantic action resulting from processing a Kitty graphics protocol event.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GraphicOutcome {
    /// Escape sequence response to write back to the PTY (e.g. OK acknowledgements, query responses).
    pub pty_response: Option<Vec<u8>>,
    /// Cursor movement requested by the image command: `(cols, rows, do_not_move)`.
    pub cursor_advance: Option<(usize, usize, bool)>,
    /// Whether the terminal screen needs to be redrawn.
    pub needs_redraw: bool,
}

/// Collects all low-24-bit Kitty placeholder IDs referenced across the provided rows.
#[must_use]
pub fn collect_placeholder_ids<'a>(rows: impl IntoIterator<Item = &'a Row>) -> HashSet<u32> {
    let mut ids = HashSet::new();
    for row in rows {
        for cell in &row.cells {
            if cell.c != KITTY_PLACEHOLDER {
                continue;
            }
            let id_low24 = match cell.fg {
                Color::Rgb(r, g, b) => ((r as u32) << 16) | ((g as u32) << 8) | (b as u32),
                Color::Indexed(idx) => idx as u32,
                _ => 0,
            } & 0x00FF_FFFF;
            if id_low24 != 0 {
                ids.insert(id_low24);
            }
        }
    }
    ids
}

/// Deep module managing image decode storage, spatial placement indices, and LRU budgets.
#[derive(Debug, Clone, Default)]
pub struct GraphicStore {
    images: HashMap<u32, ImageData>,
    image_bytes_total: usize,
    image_versions: HashMap<u32, u64>,
    image_lru: Vec<u32>,
    placements: Vec<ImagePlacement>,
    alt_placements: Vec<ImagePlacement>,
    virtual_placements: HashMap<u32, (usize, usize)>,
}

impl GraphicStore {
    /// Creates an empty `GraphicStore`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a read-only reference to all stored images.
    #[must_use]
    pub fn images(&self) -> &HashMap<u32, ImageData> {
        &self.images
    }

    /// Returns a read-only reference to a specific image by id.
    #[must_use]
    pub fn image(&self, id: u32) -> Option<&ImageData> {
        self.images.get(&id)
    }

    /// Returns `true` if an image with the given id exists.
    #[must_use]
    pub fn contains_image(&self, id: u32) -> bool {
        self.images.contains_key(&id)
    }

    /// Returns the total decoded image byte size currently retained in memory.
    #[must_use]
    pub fn total_image_bytes(&self) -> usize {
        self.image_bytes_total
    }

    /// Returns the monotonic mutation version for the given image id.
    #[must_use]
    pub fn image_version(&self, id: u32) -> Option<u64> {
        self.image_versions.get(&id).copied()
    }

    /// Returns the active on-screen image placements.
    #[must_use]
    pub fn placements(&self) -> &[ImagePlacement] {
        &self.placements
    }

    /// Returns a mutable slice of active on-screen image placements.
    pub fn placements_mut(&mut self) -> &mut [ImagePlacement] {
        &mut self.placements
    }

    /// Returns the virtual placement mappings (keyed by image id -> `(cols, rows)`).
    #[must_use]
    pub fn virtual_placements(&self) -> &HashMap<u32, (usize, usize)> {
        &self.virtual_placements
    }

    /// Switches placement state to the alternate screen buffer.
    pub fn switch_to_alt_screen(&mut self) {
        self.alt_placements = std::mem::take(&mut self.placements);
    }

    /// Restores placement state from the alternate screen back to the primary screen.
    pub fn restore_from_alt_screen(&mut self) {
        self.placements = std::mem::take(&mut self.alt_placements);
    }

    /// Internal helper removing an image and updating the byte budget and placement references.
    fn remove_image_internal(&mut self, k: u32) {
        if let Some(image) = self.images.remove(&k) {
            self.image_bytes_total = self.image_bytes_total.saturating_sub(image.byte_size());
        }
        self.image_versions.remove(&k);
        self.placements.retain(|p| p.image_id != k);
        self.alt_placements.retain(|p| p.image_id != k);
        self.virtual_placements.remove(&k);
        self.image_lru.retain(|&id| id != k);
    }

    /// Removes an image and cleans up all associated placements.
    pub fn remove_image(&mut self, id: u32) {
        self.remove_image_internal(id);
    }

    /// Adds a decoded image to the store, enforcing memory budget and LRU eviction.
    pub fn add_image(&mut self, image: ImageData, active_placeholders: &HashSet<u32>) {
        let id = image.id;
        if image.byte_size() > MAX_STORED_IMAGE_BYTES {
            self.remove_image_internal(id);
            return;
        }

        self.image_lru.retain(|&k| k != id);
        self.image_lru.push(id);
        let byte_size = image.byte_size();
        if let Some(previous) = self.images.insert(id, image) {
            self.image_bytes_total = self.image_bytes_total.saturating_sub(previous.byte_size());
        }
        self.image_bytes_total = self.image_bytes_total.saturating_add(byte_size);
        let ver = self.image_versions.entry(id).or_insert(0);
        *ver = ver.wrapping_add(1);

        if self.images.len() > MAX_STORED_IMAGES
            || self.total_image_bytes() > MAX_STORED_IMAGE_BYTES
        {
            let mut active_ids: HashSet<u32> = self
                .placements
                .iter()
                .chain(self.alt_placements.iter())
                .map(|p| p.image_id)
                .collect();

            for &id_low24 in active_placeholders {
                active_ids.insert(id_low24);
                if let Some(&real_id) = self
                    .virtual_placements
                    .keys()
                    .find(|&&k| (k & 0x00FF_FFFF) == id_low24)
                {
                    active_ids.insert(real_id);
                }
            }

            self.virtual_placements
                .retain(|id, _| active_ids.contains(id));

            // First pass: evict unreferenced images in LRU order
            let unplaced: Vec<u32> = self
                .image_lru
                .iter()
                .copied()
                .filter(|&k| k != id && !active_ids.contains(&k))
                .collect();
            for k in unplaced {
                if self.images.len() <= MAX_STORED_IMAGES
                    && self.total_image_bytes() <= MAX_STORED_IMAGE_BYTES
                {
                    break;
                }
                self.remove_image_internal(k);
            }

            // Second pass: if still exceeding budget, evict oldest referenced images
            while self.images.len() > MAX_STORED_IMAGES
                || self.total_image_bytes() > MAX_STORED_IMAGE_BYTES
            {
                let oldest = self.image_lru.iter().copied().find(|&k| k != id);
                let Some(k) = oldest else { break };
                self.remove_image_internal(k);
            }
        }
    }

    /// Adds an image placement instance anchored to grid cells with FIFO eviction.
    pub fn add_placement(&mut self, placement: ImagePlacement) {
        if placement.placement_id != 0
            && let Some(existing) = self.placements.iter_mut().find(|p| {
                p.image_id == placement.image_id && p.placement_id == placement.placement_id
            })
        {
            *existing = placement;
            return;
        }
        if self.placements.len() >= MAX_PLACEMENTS {
            self.placements.remove(0);
        }
        self.placements.push(placement);
    }

    /// Registers a virtual placement dimension mapping for Kitty placeholder cells.
    pub fn insert_virtual_placement(&mut self, id: u32, cols: usize, rows: usize) {
        self.virtual_placements.insert(id, (cols, rows));
    }

    fn delete_placements_at(&mut self, matches: impl Fn(&ImagePlacement) -> bool, free: bool) {
        let mut affected = Vec::new();
        for placement in &self.placements {
            if matches(placement) && !affected.contains(&placement.image_id) {
                affected.push(placement.image_id);
            }
        }
        self.placements.retain(|placement| !matches(placement));
        if free {
            self.free_images_without_placements(&affected);
        }
    }

    fn free_images_without_placements(&mut self, image_ids: &[u32]) {
        for &id in image_ids {
            let placed = self.placements.iter().any(|p| p.image_id == id)
                || self.alt_placements.iter().any(|p| p.image_id == id)
                || self.virtual_placements.contains_key(&id);
            if !placed {
                self.remove_image_internal(id);
            }
        }
    }

    /// Deletes images and/or placements matching the given delete target.
    pub fn delete_images(&mut self, target: DeleteTarget, cursor_line: usize, cursor_col: usize) {
        match target {
            DeleteTarget::All => {
                self.placements.clear();
                self.alt_placements.clear();
            }
            DeleteTarget::AllAndFree => {
                self.images.clear();
                self.image_bytes_total = 0;
                self.image_versions.clear();
                self.placements.clear();
                self.alt_placements.clear();
                self.virtual_placements.clear();
                self.image_lru.clear();
            }
            DeleteTarget::ById(id) => {
                self.placements.retain(|p| p.image_id != id);
                self.alt_placements.retain(|p| p.image_id != id);
            }
            DeleteTarget::ByIdAndFree(id) => {
                self.remove_image_internal(id);
            }
            DeleteTarget::ByIdPlacement(id, placement) => {
                self.placements
                    .retain(|p| !(p.image_id == id && p.placement_id == placement));
                self.alt_placements
                    .retain(|p| !(p.image_id == id && p.placement_id == placement));
            }
            DeleteTarget::ByIdPlacementAndFree(id, placement) => {
                self.placements
                    .retain(|p| !(p.image_id == id && p.placement_id == placement));
                self.alt_placements
                    .retain(|p| !(p.image_id == id && p.placement_id == placement));
                self.free_images_without_placements(&[id]);
            }
            DeleteTarget::ByNumber { .. } => {}
            DeleteTarget::AtCursor => {
                self.delete_placements_at(
                    |p| {
                        p.line <= cursor_line
                            && cursor_line < p.line + p.rows
                            && p.col <= cursor_col
                            && cursor_col < p.col + p.cols
                    },
                    false,
                );
            }
            DeleteTarget::AtCursorAndFree => {
                self.delete_placements_at(
                    |p| {
                        p.line <= cursor_line
                            && cursor_line < p.line + p.rows
                            && p.col <= cursor_col
                            && cursor_col < p.col + p.cols
                    },
                    true,
                );
            }
            DeleteTarget::AtAbsoluteCell { x, y, z, free } => {
                let row = y.saturating_sub(1) as usize;
                let col = x.saturating_sub(1) as usize;
                self.delete_placements_at(
                    |p| {
                        p.line <= row
                            && row < p.line + p.rows
                            && p.col <= col
                            && col < p.col + p.cols
                            && z.is_none_or(|z_idx| p.z_index == z_idx)
                    },
                    free,
                );
            }
            DeleteTarget::ByColumn { x, free } => {
                let col = x.saturating_sub(1) as usize;
                self.delete_placements_at(|p| p.col <= col && col < p.col + p.cols, free);
            }
            DeleteTarget::ByRow { y, free } => {
                let row = y.saturating_sub(1) as usize;
                self.delete_placements_at(|p| p.line <= row && row < p.line + p.rows, free);
            }
            DeleteTarget::ByZIndex(z_index) => {
                self.placements.retain(|p| p.z_index != z_index);
                self.alt_placements.retain(|p| p.z_index != z_index);
            }
            DeleteTarget::ByZIndexAndFree(z_index) => {
                self.delete_placements_at(|p| p.z_index == z_index, true);
            }
            DeleteTarget::ByIdRange { lo, hi, free } => {
                self.delete_placements_at(|p| lo <= p.image_id && p.image_id <= hi, free);
            }
        }
    }

    /// Shifts placement lines down when lines are inserted at the top of a scroll margin.
    pub fn shift_on_margin_scroll_down(&mut self, top: usize, bottom: usize, amount: usize) {
        for placements in [&mut self.placements, &mut self.alt_placements] {
            placements.retain_mut(|p| {
                if p.line >= top && p.line < bottom {
                    p.line += amount;
                    p.line < bottom
                } else {
                    true
                }
            });
        }
    }

    /// Shifts placement lines when content is scrolled up.
    pub fn shift_on_scroll_up(&mut self, amount: usize) {
        for placements in [&mut self.placements, &mut self.alt_placements] {
            placements.retain_mut(|p| {
                if p.line >= amount {
                    p.line -= amount;
                    true
                } else {
                    false
                }
            });
        }
    }

    /// Retains only placements satisfying the predicate across both active and alternate buffers.
    pub fn retain_mut_placements(
        &mut self,
        mut predicate: impl FnMut(&mut ImagePlacement) -> bool,
    ) {
        self.placements.retain_mut(&mut predicate);
        self.alt_placements.retain_mut(predicate);
    }

    /// Updates placements with the results of a grid column resize and reflow.
    pub fn update_placements_after_reflow(&mut self, new_placements: Vec<ImagePlacement>) {
        self.placements = new_placements;
    }

    /// Synchronizes newly decoded images with the GPU, providing RGBA slices to the uploader closure.
    /// Once uploaded, releases the CPU-side RGBA memory while retaining dimensional metadata.
    pub fn sync_gpu_textures<F>(&mut self, mut uploader: F)
    where
        F: FnMut(u32, u32, u32, u64, &[u8]) -> bool,
    {
        for (id, img) in &mut self.images {
            if let Some(rgba) = &img.rgba {
                let ver = self.image_versions.get(id).copied().unwrap_or(0);
                if uploader(*id, img.width, img.height, ver, rgba) {
                    img.rgba = None;
                }
            }
        }
    }

    /// Processes a Kitty graphics protocol event and updates store state accordingly.
    #[must_use]
    pub fn handle_event(
        &mut self,
        event: KittyEvent,
        ctx: GraphicContext,
        active_placeholders: &HashSet<u32>,
    ) -> GraphicOutcome {
        match event {
            KittyEvent::Transmit { command, image } => {
                let image_id = image.id;
                let placement_id = command.placement_id.unwrap_or(0);
                let ack_id = command.placement_id.filter(|id| *id != 0);
                let src_w = command
                    .src_w
                    .unwrap_or(image.width.saturating_sub(command.src_x.unwrap_or(0)));
                let src_h = command
                    .src_h
                    .unwrap_or(image.height.saturating_sub(command.src_y.unwrap_or(0)));
                let img_w = (src_w as f32).max(1.0);
                let img_h = (src_h as f32).max(1.0);

                self.add_image(image, active_placeholders);

                let cw = ctx.cell_width.max(1.0);
                let ch = ctx.cell_height.max(1.0);

                let (cols, rows) = match (command.cols, command.rows) {
                    (Some(c), Some(r)) => (c as usize, r as usize),
                    (Some(c), None) => {
                        let pixel_w = c as f32 * cw;
                        let pixel_h = pixel_w * (img_h / img_w);
                        let r = (pixel_h / ch).ceil().max(1.0) as usize;
                        (c as usize, r)
                    }
                    (None, Some(r)) => {
                        let pixel_h = r as f32 * ch;
                        let pixel_w = pixel_h * (img_w / img_h);
                        let c = (pixel_w / cw).ceil().max(1.0) as usize;
                        (c, r as usize)
                    }
                    (None, None) => {
                        let c = (img_w / cw).ceil().max(1.0) as usize;
                        let r = (img_h / ch).ceil().max(1.0) as usize;
                        (c, r)
                    }
                };

                let should_place = command.action == KittyAction::TransmitAndDisplay
                    || command.action == KittyAction::TransmitAndDisplayWithResponse;

                if command.is_virtual {
                    self.insert_virtual_placement(image_id, cols, rows);
                } else if should_place {
                    let abs_line = ctx.scrollback_len + ctx.cursor_row;
                    self.add_placement(ImagePlacement {
                        image_id,
                        placement_id,
                        line: abs_line,
                        col: ctx.cursor_col,
                        cols,
                        rows,
                        src_x: command.src_x.unwrap_or(0),
                        src_y: command.src_y.unwrap_or(0),
                        src_w: command.src_w,
                        src_h: command.src_h,
                        offset_x: command.offset_x,
                        offset_y: command.offset_y,
                        z_index: command.z_index,
                    });
                }

                let wants_ack = command.action == KittyAction::TransmitAndDisplayWithResponse
                    || command.id_explicit
                    || command.number_explicit;
                let pty_response = if wants_ack && command.quiet == 0 {
                    Some(kitty_response_full(
                        image_id,
                        command.image_number,
                        ack_id,
                        "OK",
                    ))
                } else {
                    None
                };

                let cursor_advance = if should_place {
                    Some((cols, rows, command.do_not_move_cursor || command.is_virtual))
                } else {
                    None
                };

                GraphicOutcome {
                    pty_response,
                    cursor_advance,
                    needs_redraw: should_place,
                }
            }
            KittyEvent::Place { command } => {
                let Some(image_id) = command.image_id else {
                    return GraphicOutcome::default();
                };
                let placement_id = command.placement_id.unwrap_or(0);
                let ack_id = command.placement_id.filter(|id| *id != 0);
                let Some(image) = self.image(image_id) else {
                    let pty_response = if command.quiet < 2 {
                        Some(kitty_response_full(
                            image_id,
                            command.image_number,
                            ack_id,
                            "ENOENT:image not found",
                        ))
                    } else {
                        None
                    };
                    return GraphicOutcome {
                        pty_response,
                        cursor_advance: None,
                        needs_redraw: false,
                    };
                };

                let src_w = command
                    .src_w
                    .unwrap_or(image.width.saturating_sub(command.src_x.unwrap_or(0)));
                let src_h = command
                    .src_h
                    .unwrap_or(image.height.saturating_sub(command.src_y.unwrap_or(0)));
                let img_w = (src_w as f32).max(1.0);
                let img_h = (src_h as f32).max(1.0);

                let cw = ctx.cell_width.max(1.0);
                let ch = ctx.cell_height.max(1.0);

                let (cols, rows) = match (command.cols, command.rows) {
                    (Some(c), Some(r)) => (c as usize, r as usize),
                    (Some(c), None) => {
                        let pixel_w = c as f32 * cw;
                        let pixel_h = pixel_w * (img_h / img_w);
                        let r = (pixel_h / ch).ceil().max(1.0) as usize;
                        (c as usize, r)
                    }
                    (None, Some(r)) => {
                        let pixel_h = r as f32 * ch;
                        let pixel_w = pixel_h * (img_w / img_h);
                        let c = (pixel_w / cw).ceil().max(1.0) as usize;
                        (c, r as usize)
                    }
                    (None, None) => {
                        let c = (img_w / cw).ceil().max(1.0) as usize;
                        let r = (img_h / ch).ceil().max(1.0) as usize;
                        (c, r)
                    }
                };

                if command.is_virtual {
                    self.insert_virtual_placement(image_id, cols, rows);
                } else {
                    let abs_line = ctx.scrollback_len + ctx.cursor_row;
                    self.add_placement(ImagePlacement {
                        image_id,
                        placement_id,
                        line: abs_line,
                        col: ctx.cursor_col,
                        cols,
                        rows,
                        src_x: command.src_x.unwrap_or(0),
                        src_y: command.src_y.unwrap_or(0),
                        src_w: command.src_w,
                        src_h: command.src_h,
                        offset_x: command.offset_x,
                        offset_y: command.offset_y,
                        z_index: command.z_index,
                    });
                }

                let wants_ack = command.action == KittyAction::TransmitAndDisplayWithResponse
                    || command.id_explicit
                    || command.number_explicit;
                let pty_response = if wants_ack && command.quiet == 0 {
                    Some(kitty_response_full(
                        image_id,
                        command.image_number,
                        ack_id,
                        "OK",
                    ))
                } else {
                    None
                };

                GraphicOutcome {
                    pty_response,
                    cursor_advance: Some((
                        cols,
                        rows,
                        command.do_not_move_cursor || command.is_virtual,
                    )),
                    needs_redraw: true,
                }
            }
            KittyEvent::Delete { target } => {
                let abs_line = ctx.scrollback_len + ctx.cursor_row;
                self.delete_images(target, abs_line, ctx.cursor_col);
                GraphicOutcome {
                    pty_response: None,
                    cursor_advance: None,
                    needs_redraw: true,
                }
            }
            KittyEvent::Response(resp) => GraphicOutcome {
                pty_response: Some(resp),
                cursor_advance: None,
                needs_redraw: false,
            },
        }
    }
}
