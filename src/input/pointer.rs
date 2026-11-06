//! Stateful controller for pointer interactions, text selection lifecycles, and mouse protocols.

use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::Shape;

use crate::font::CellMetrics;
use crate::grid::{CellFlags, Grid};
use crate::input::Modifiers;
use crate::input::mouse::{MouseModifiers, MouseState, encode_mouse_event};
use crate::input::selection::{Selection, SelectionPoint, SelectionType, find_word_boundaries};
use crate::render::HoveredHyperlinkSpan;

/// Translates a Linux evdev button code to an X11 button index.
#[must_use]
pub fn x11_button_index(button: u32) -> Option<u8> {
    match button {
        0x110 => Some(0), // BTN_LEFT -> 0
        0x111 => Some(2), // BTN_RIGHT -> 2
        0x112 => Some(1), // BTN_MIDDLE -> 1
        _ => None,
    }
}

/// Transient layout metrics and terminal configuration required to process pointer events.
pub struct PointerContext<'a> {
    pub grid: &'a Grid,
    pub metrics: CellMetrics,
    pub scale: f64,
    pub padding: [u16; 2],
    pub modifiers: Modifiers,
    pub mouse_mode: &'a MouseState,
    pub scroll_multiplier: f64,
    pub is_alt_screen: bool,
    pub hyperlink_resolver: Option<&'a dyn Fn(u32) -> Option<&'a str>>,
}

impl<'a> PointerContext<'a> {
    /// Creates a new `PointerContext` borrowing the necessary layout and terminal environment.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        grid: &'a Grid,
        metrics: CellMetrics,
        scale: f64,
        padding: [u16; 2],
        modifiers: Modifiers,
        mouse_mode: &'a MouseState,
        scroll_multiplier: f64,
        is_alt_screen: bool,
        hyperlink_resolver: Option<&'a dyn Fn(u32) -> Option<&'a str>>,
    ) -> Self {
        Self {
            grid,
            metrics,
            scale,
            padding,
            modifiers,
            mouse_mode,
            scroll_multiplier,
            is_alt_screen,
            hyperlink_resolver,
        }
    }
}

/// Composite set of semantic actions and compositor cursor updates produced by evaluating a pointer event.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PointerOutcome {
    /// Byte sequence to write to the PTY (e.g. SGR 1006 mouse report, alt-screen arrow sequences).
    pub pty_write: Option<Vec<u8>>,
    /// Updated mouse cursor shape for the compositor, if changed.
    pub cursor_shape: Option<Shape>,
    /// URL to open when Ctrl+Clicking a detected or explicit hyperlink.
    pub open_url: Option<String>,
    /// Request to paste the primary selection buffer (e.g. middle-click).
    pub paste_primary: bool,
    /// Whether the terminal grid or visual overlay needs to be redrawn.
    pub needs_redraw: bool,
    /// Number of lines to scroll the viewport in regular scrollback mode (negative = up, positive = down).
    pub scroll_lines: Option<i32>,
}

/// Stateful controller that encapsulates pointer interactions, selection lifecycles, and mouse protocols.
#[derive(Debug, Clone)]
pub struct PointerController {
    pub mouse_pos: [f64; 2],
    pub pointer_in_surface: bool,
    pub mouse_pressed: bool,
    pub mouse_buttons_held: u8,
    pub mouse_reported: bool,
    pub mouse_button: u8,
    pub last_click_time: u32,
    pub click_count: u8,
    pub last_click_cell: Option<(usize, usize)>,
    pub scroll_accumulator: f64,
    pub selection: Selection,
    pub hovered_span: Option<HoveredHyperlinkSpan>,
    pub current_cursor_shape: Option<Shape>,
}

impl Default for PointerController {
    fn default() -> Self {
        Self::new()
    }
}

impl PointerController {
    /// Creates a new `PointerController` with default initial state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            mouse_pos: [0.0, 0.0],
            pointer_in_surface: false,
            mouse_pressed: false,
            mouse_buttons_held: 0,
            mouse_reported: false,
            mouse_button: 0,
            last_click_time: 0,
            click_count: 0,
            last_click_cell: None,
            scroll_accumulator: 0.0,
            selection: Selection::default(),
            hovered_span: None,
            current_cursor_shape: None,
        }
    }

    /// Read-only reference to the current selection.
    #[must_use]
    pub fn selection(&self) -> &Selection {
        &self.selection
    }

    /// Mutable reference to the current selection.
    pub fn selection_mut(&mut self) -> &mut Selection {
        &mut self.selection
    }

    /// Clears the current selection.
    pub fn clear_selection(&mut self) {
        self.selection = Selection::default();
    }

    /// Sets the selection directly.
    pub fn set_selection(&mut self, selection: Selection) {
        self.selection = selection;
    }

    /// Returns the currently hovered hyperlink span, if any.
    #[must_use]
    pub fn hovered_span(&self) -> Option<HoveredHyperlinkSpan> {
        self.hovered_span
    }

    /// Returns the current mouse cursor shape.
    #[must_use]
    pub fn current_cursor_shape(&self) -> Option<Shape> {
        self.current_cursor_shape
    }

    /// Returns whether the pointer is currently within the terminal surface.
    #[must_use]
    pub fn is_in_surface(&self) -> bool {
        self.pointer_in_surface
    }

    /// Returns the current surface-relative mouse coordinates.
    #[must_use]
    pub fn mouse_pos(&self) -> [f64; 2] {
        self.mouse_pos
    }

    /// Returns whether the primary mouse button is currently pressed down.
    #[must_use]
    pub fn is_pressed(&self) -> bool {
        self.mouse_pressed
    }

    /// Returns the absolute `(line, screen_row, col)` grid coordinates for the given surface coordinates.
    #[must_use]
    pub fn cell_at_coords(
        surface_x: f64,
        surface_y: f64,
        ctx: &PointerContext<'_>,
    ) -> (usize, usize, usize) {
        let scale = ctx.scale.max(0.1);
        let cw = f64::from(ctx.metrics.cell_width) / scale;
        let ch = f64::from(ctx.metrics.cell_height) / scale;
        let pad_x = f64::from(ctx.padding[0]);
        let pad_y = f64::from(ctx.padding[1]);

        let col = ((surface_x - pad_x) / cw).max(0.0) as usize;
        let col = col.min(ctx.grid.cols.saturating_sub(1));

        let screen_row = ((surface_y - pad_y) / ch).max(0.0) as usize;
        let screen_row = screen_row.min(ctx.grid.rows.saturating_sub(1));

        let abs_line = ctx.grid.scrollback.len() + screen_row - ctx.grid.viewport_offset;
        (abs_line, screen_row, col)
    }

    /// Returns the absolute `(line, screen_row, col)` grid coordinates under the current pointer position.
    #[must_use]
    pub fn cell_at_pointer(&self, ctx: &PointerContext<'_>) -> (usize, usize, usize) {
        Self::cell_at_coords(self.mouse_pos[0], self.mouse_pos[1], ctx)
    }

    /// Returns the resolved URL under the current pointer position, if one exists.
    #[must_use]
    pub fn url_at_pointer(&self, ctx: &PointerContext<'_>) -> Option<String> {
        let (line, screen_row, col) = self.cell_at_pointer(ctx);
        let row = ctx.grid.visible_line(screen_row);
        let cell = row.cells.get(col);
        if let Some(cell) = cell
            && let Some(id) = cell.hyperlink_id
            && let Some(resolver) = ctx.hyperlink_resolver
            && let Some(url) = resolver(id.get())
        {
            return Some(url.to_string());
        }
        find_url_in_grid(ctx.grid, line, screen_row, col).map(|(_, _, url)| url)
    }

    /// Computes the expected cursor shape based on current hover and mouse reporting modes.
    #[must_use]
    pub fn expected_cursor_shape(&self, ctx: &PointerContext<'_>) -> Shape {
        if self.hovered_span.is_some() {
            Shape::Pointer
        } else if ctx.mouse_mode.is_reporting() {
            Shape::Default
        } else {
            Shape::Text
        }
    }

    /// Updates the hovered hyperlink span based on pointer position and modifiers.
    pub fn update_hover_state(&mut self, ctx: &PointerContext<'_>) -> (bool, Option<Shape>) {
        let new_span = if !self.pointer_in_surface || self.mouse_pressed {
            None
        } else {
            let (line, screen_row, col) = self.cell_at_pointer(ctx);
            let row = ctx.grid.visible_line(screen_row);
            if let Some(cell) = row.cells.get(col)
                && let Some(id) = cell.hyperlink_id
                && let Some(resolver) = ctx.hyperlink_resolver
                && resolver(id.get()).is_some()
            {
                let mut start_col = col;
                while start_col > 0
                    && row.cells.get(start_col - 1).and_then(|c| c.hyperlink_id) == Some(id)
                {
                    start_col -= 1;
                }
                let mut end_col = col;
                while end_col + 1 < row.cells.len()
                    && row.cells.get(end_col + 1).and_then(|c| c.hyperlink_id) == Some(id)
                {
                    end_col += 1;
                }
                Some(HoveredHyperlinkSpan {
                    line,
                    start_col,
                    end_col,
                })
            } else if ctx.modifiers.ctrl
                && let Some((start_col, end_col, _)) =
                    find_url_in_grid(ctx.grid, line, screen_row, col)
            {
                Some(HoveredHyperlinkSpan {
                    line,
                    start_col,
                    end_col,
                })
            } else {
                None
            }
        };

        let span_changed = self.hovered_span != new_span;
        if span_changed {
            self.hovered_span = new_span;
        }
        let shape_update = self.evaluate_cursor_shape(ctx);
        (span_changed, shape_update)
    }

    /// Evaluates the active cursor shape against the current surface context and updates cached state.
    fn evaluate_cursor_shape(&mut self, ctx: &PointerContext<'_>) -> Option<Shape> {
        if !self.pointer_in_surface {
            self.current_cursor_shape = None;
            return None;
        }
        let shape = self.expected_cursor_shape(ctx);
        if self.current_cursor_shape == Some(shape) {
            None
        } else {
            self.current_cursor_shape = Some(shape);
            Some(shape)
        }
    }

    /// Handles pointer entry into the Wayland surface.
    pub fn handle_enter(
        &mut self,
        x: f64,
        y: f64,
        _serial: u32,
        ctx: &PointerContext<'_>,
    ) -> PointerOutcome {
        self.pointer_in_surface = true;
        self.mouse_pos = [x, y];
        self.current_cursor_shape = None;
        let (needs_redraw, cursor_shape) = self.update_hover_state(ctx);
        PointerOutcome {
            cursor_shape,
            needs_redraw,
            ..Default::default()
        }
    }

    /// Handles pointer leaving the Wayland surface.
    pub fn handle_leave(&mut self) -> PointerOutcome {
        self.pointer_in_surface = false;
        let needs_redraw = self.hovered_span.is_some();
        self.hovered_span = None;
        self.current_cursor_shape = None;
        PointerOutcome {
            needs_redraw,
            ..Default::default()
        }
    }

    /// Handles pointer motion within the Wayland surface.
    pub fn handle_motion(&mut self, x: f64, y: f64, ctx: &PointerContext<'_>) -> PointerOutcome {
        self.mouse_pos = [x, y];
        let (mut needs_redraw, cursor_shape) = self.update_hover_state(ctx);

        let held = self.mouse_buttons_held != 0 || self.mouse_reported;
        if ctx.mouse_mode.reports_motion(held) {
            let button = if held {
                if self.mouse_buttons_held != 0 {
                    self.mouse_buttons_held.trailing_zeros() as u8
                } else {
                    self.mouse_button
                }
            } else {
                3
            };
            if let Some(bytes) = self.encode_mouse(button, true, true, ctx) {
                return PointerOutcome {
                    pty_write: Some(bytes),
                    cursor_shape,
                    needs_redraw,
                    ..Default::default()
                };
            }
        }

        if self.mouse_pressed {
            let (line, _, col) = Self::cell_at_coords(x, y, ctx);
            self.selection.end = SelectionPoint::new(line, col);
            needs_redraw = true;
        }

        PointerOutcome {
            cursor_shape,
            needs_redraw,
            ..Default::default()
        }
    }

    /// Handles pointer button press or release events.
    pub fn handle_button(
        &mut self,
        button: u32,
        pressed: bool,
        time: u32,
        _serial: u32,
        ctx: &PointerContext<'_>,
    ) -> PointerOutcome {
        let Some(index) = x11_button_index(button) else {
            return PointerOutcome::default();
        };

        if pressed {
            if index == 0
                && ctx.modifiers.ctrl
                && let Some(url_owned) = self.url_at_pointer(ctx)
            {
                return PointerOutcome {
                    open_url: Some(url_owned),
                    ..Default::default()
                };
            }

            if let Some(bytes) = self.encode_mouse(index, true, false, ctx) {
                self.mouse_buttons_held |= 1 << index;
                self.mouse_reported = true;
                self.mouse_button = index;
                return PointerOutcome {
                    pty_write: Some(bytes),
                    ..Default::default()
                };
            }

            if index == 1 {
                return PointerOutcome {
                    paste_primary: true,
                    ..Default::default()
                };
            }

            if index != 0 {
                return PointerOutcome::default();
            }

            let (line, screen_row, col) = self.cell_at_pointer(ctx);
            let same_cell = self.last_click_cell == Some((line, col));
            if same_cell && time.saturating_sub(self.last_click_time) < 350 {
                self.click_count = (self.click_count % 3) + 1;
            } else {
                self.click_count = 1;
            }
            self.last_click_time = time;
            self.last_click_cell = Some((line, col));
            self.mouse_pressed = true;
            let (_, cursor_shape) = self.update_hover_state(ctx);

            match self.click_count {
                1 => {
                    self.selection = Selection::new(
                        SelectionPoint::new(line, col),
                        SelectionPoint::new(line, col),
                        SelectionType::Simple,
                    );
                }
                2 => {
                    let row = ctx.grid.visible_line(screen_row);
                    let (w_start, w_end) = find_word_boundaries(row, col);
                    self.selection = Selection::new(
                        SelectionPoint::new(line, w_start),
                        SelectionPoint::new(line, w_end),
                        SelectionType::Word,
                    );
                }
                3 => {
                    self.selection = Selection::new(
                        SelectionPoint::new(line, 0),
                        SelectionPoint::new(line, ctx.grid.cols.saturating_sub(1)),
                        SelectionType::Line,
                    );
                }
                _ => {}
            }

            PointerOutcome {
                cursor_shape,
                needs_redraw: true,
                ..Default::default()
            }
        } else {
            let was_held = (self.mouse_buttons_held & (1 << index)) != 0;
            let mut pty_write = None;
            if was_held {
                self.mouse_buttons_held &= !(1 << index);
                self.mouse_reported = self.mouse_buttons_held != 0;
                pty_write = self.encode_mouse(index, false, false, ctx);
            }
            if index == 0 {
                self.mouse_pressed = false;
                let (_, cursor_shape) = self.update_hover_state(ctx);
                return PointerOutcome {
                    pty_write,
                    cursor_shape,
                    needs_redraw: true,
                    ..Default::default()
                };
            }

            PointerOutcome {
                pty_write,
                ..Default::default()
            }
        }
    }

    /// Handles pointer vertical scroll axis events.
    pub fn handle_axis(&mut self, value: f64, ctx: &PointerContext<'_>) -> PointerOutcome {
        self.scroll_accumulator += (value / 15.0) * ctx.scroll_multiplier;
        let lines = self.scroll_accumulator.trunc() as i32;
        if lines == 0 {
            return PointerOutcome::default();
        }
        self.scroll_accumulator -= f64::from(lines);
        let count = (lines.unsigned_abs() as usize).min(100);

        if ctx.mouse_mode.is_reporting() && !ctx.modifiers.shift {
            let button = if lines < 0 { 64 } else { 65 };
            let mut batch = Vec::new();
            for _ in 0..count {
                if let Some(bytes) = self.encode_mouse(button, true, false, ctx) {
                    batch.extend_from_slice(&bytes);
                } else {
                    break;
                }
            }
            PointerOutcome {
                pty_write: if batch.is_empty() { None } else { Some(batch) },
                ..Default::default()
            }
        } else if ctx.is_alt_screen {
            let seq: &[u8] = if lines < 0 { b"\x1b[A" } else { b"\x1b[B" };
            let batch = seq.repeat(count);
            PointerOutcome {
                pty_write: Some(batch),
                ..Default::default()
            }
        } else {
            let clamped_lines = if lines < 0 {
                -(count as i32)
            } else {
                count as i32
            };
            PointerOutcome {
                scroll_lines: Some(clamped_lines),
                needs_redraw: true,
                ..Default::default()
            }
        }
    }

    /// Handles axis stop events.
    pub fn handle_axis_stop(&mut self) -> PointerOutcome {
        self.scroll_accumulator = 0.0;
        PointerOutcome::default()
    }

    /// Encodes the pointer event as escape sequences according to active terminal mouse tracking mode.
    fn encode_mouse(
        &self,
        button: u8,
        pressed: bool,
        motion: bool,
        ctx: &PointerContext<'_>,
    ) -> Option<Vec<u8>> {
        if !ctx.mouse_mode.is_reporting() {
            return None;
        }
        if ctx.modifiers.shift && pressed {
            return None;
        }
        let (_, screen_row, col) = Self::cell_at_coords(self.mouse_pos[0], self.mouse_pos[1], ctx);
        encode_mouse_event(
            ctx.mouse_mode.encoding,
            button,
            col,
            screen_row,
            pressed,
            motion,
            MouseModifiers {
                shift: ctx.modifiers.shift,
                alt: ctx.modifiers.alt,
                ctrl: ctx.modifiers.ctrl,
            },
        )
    }
}

/// Returns `(start_col, end_col, url)` where `start_col..=end_col` is the span on `target_line`.
#[must_use]
pub fn find_url_in_grid(
    grid: &Grid,
    target_line: usize,
    target_screen_row: usize,
    target_col: usize,
) -> Option<(usize, usize, String)> {
    let mut start_row = target_screen_row;
    while start_row > 0 {
        if grid.visible_line(start_row - 1).wrapped {
            start_row -= 1;
        } else {
            break;
        }
    }
    let mut end_row = target_screen_row;
    while end_row + 1 < grid.rows {
        if grid.visible_line(end_row).wrapped {
            end_row += 1;
        } else {
            break;
        }
    }

    let mut text = String::new();
    let mut coords: Vec<(usize, usize, usize)> = Vec::new(); // (abs_line, col, width)

    for r in start_row..=end_row {
        let line = grid.visible_line(r);
        let abs_line = grid.scrollback.len() + r - grid.viewport_offset;
        for (c_idx, cell) in line.cells.iter().enumerate() {
            if cell.flags.contains(CellFlags::HIDDEN) {
                text.push(' ');
                coords.push((abs_line, c_idx, 1));
            } else if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                // Skip wide character continuation spacers
            } else {
                let width = if cell.flags.contains(CellFlags::WIDE_CHAR) {
                    2
                } else {
                    1
                };
                if let Some(cluster) = line.cluster_text(c_idx) {
                    for ch in cluster.chars() {
                        text.push(ch);
                        coords.push((abs_line, c_idx, width));
                    }
                } else {
                    text.push(cell.c);
                    coords.push((abs_line, c_idx, width));
                }
            }
        }
    }

    let schemes = ["https://", "http://", "file://", "gemini://"];
    for scheme in &schemes {
        let mut search_from = 0;
        while let Some(pos) = text[search_from..].find(scheme) {
            let start = search_from + pos;
            let mut end = start;
            for (idx, ch) in text[start..].char_indices() {
                if ch.is_whitespace()
                    || ch == '<'
                    || ch == '>'
                    || ch == '"'
                    || ch == '`'
                    || ch == '^'
                    || ch == '\\'
                    || ch == '|'
                {
                    break;
                }
                end = start + idx + ch.len_utf8();
            }

            while end > start {
                let Some(last_char) = text[..end].chars().next_back() else {
                    break;
                };
                if matches!(
                    last_char,
                    '.' | ',' | '!' | '?' | ';' | ':' | ')' | ']' | '}' | '\'' | '"'
                ) {
                    if last_char == ')'
                        && text[start..end].matches('(').count()
                            == text[start..end].matches(')').count()
                    {
                        break;
                    }
                    end -= last_char.len_utf8();
                } else {
                    break;
                }
            }

            let start_char_idx = text[..start].chars().count();
            let end_char_idx = text[..end].chars().count();

            if start_char_idx < coords.len() && end_char_idx <= coords.len() {
                let url_coords = &coords[start_char_idx..end_char_idx];
                let is_hit = url_coords.iter().any(|&(line, c, w)| {
                    line == target_line && target_col >= c && target_col < c + w
                });

                if is_hit && end > start + scheme.len() {
                    let mut min_col = usize::MAX;
                    let mut max_col = 0;
                    for &(line, c, w) in url_coords {
                        if line == target_line {
                            min_col = min_col.min(c);
                            max_col = max_col.max(c + w - 1);
                        }
                    }
                    if min_col <= max_col {
                        let url = text[start..end].to_string();
                        return Some((min_col, max_col, url));
                    }
                }
            }

            search_from = start + scheme.len();
        }
    }

    None
}

/// Extracts a plaintext URL and its column span on a given row if `col` falls within it.
#[must_use]
#[cfg(test)]
pub fn find_url_at_col(row: &crate::grid::Row, col: usize) -> Option<(usize, usize, String)> {
    let mut grid = Grid::new(row.cells.len(), 1, 0);
    grid.lines[0] = row.clone();
    find_url_in_grid(&grid, 0, 0, col)
}
