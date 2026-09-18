//! Application control actions: configuration reload, key shortcuts, and PTY writes.

use std::io::{self, Write};
use std::os::fd::AsFd;

use nix::poll::{PollFd, PollFlags, poll};
use wayland_client::{Connection, QueueHandle};

use crate::config::Config;
use crate::event_loop::AppState;
use crate::font::FontManager;
use crate::input::KeyAction;

impl AppState {
    /// Writes bytes to the non-blocking PTY master with a bounded readiness loop to prevent truncation.
    pub fn write_pty_blocking(&mut self, mut bytes: &[u8]) {
        let start = std::time::Instant::now();
        while !bytes.is_empty() && start.elapsed() < std::time::Duration::from_millis(100) {
            match self.pty.write(bytes) {
                Ok(0) => break,
                Ok(written) => bytes = &bytes[written..],
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    let mut fds = [PollFd::new(self.pty.as_fd(), PollFlags::POLLOUT)];
                    if !poll(&mut fds, 20u16).is_ok_and(|ready| ready > 0) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }

    /// Reloads the configuration file and dynamically updates palette, fonts, cursor, and metrics.
    pub fn reload_config(&mut self) {
        let new_config = match Config::load_from_path_or_default(self.config_path.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("ftty: failed to reload config: {e}");
                return;
            }
        };

        let families_changed = self.config.font_families() != new_config.font_families();
        let subpixel_changed = self.config.font_subpixel() != new_config.font_subpixel();
        let new_font_size = new_config.font_size();
        let size_changed = (self.config.font_size() - new_font_size).abs() > f32::EPSILON;

        if size_changed
            && (!new_font_size.is_finite()
                || new_font_size < crate::font::MIN_FONT_SIZE
                || new_font_size > crate::font::MAX_FONT_SIZE)
        {
            eprintln!("ftty: failed to reload font size: invalid size {new_font_size}");
            return;
        }

        let maybe_new_font = if families_changed || subpixel_changed {
            match FontManager::load_with_families_and_subpixel(
                &new_config.font_families(),
                new_config.font_size(),
                new_config.font_subpixel(),
            ) {
                Ok(mgr) => Some(mgr),
                Err(e) => {
                    eprintln!("ftty: failed to reload font face: {e}");
                    return;
                }
            }
        } else {
            None
        };

        let max_sb = new_config.scrollback_lines();
        self.terminal.grid.max_scrollback = max_sb;
        if self.terminal.grid.scrollback.len() > max_sb {
            let overflow = self.terminal.grid.scrollback.len() - max_sb;
            for _ in 0..overflow {
                self.terminal.grid.scrollback.pop_front();
            }
            self.terminal.grid.viewport_offset = self.terminal.grid.viewport_offset.min(max_sb);
        }

        let padding_changed = self.config.padding_x() != new_config.padding_x()
            || self.config.padding_y() != new_config.padding_y();

        self.palette = new_config.build_palette();
        self.default_fg = new_config.foreground();
        self.default_bg = new_config.background();
        self.terminal
            .set_default_colors(self.default_fg, self.default_bg);
        self.terminal.grid.cursor.shape = new_config.cursor_shape();
        self.config = new_config;

        self.terminal.grid.mark_all_dirty();
        if let Some(renderer) = &mut self.renderer {
            renderer.clear_cache();
        }

        if let Some(new_font_mgr) = maybe_new_font {
            self.font_mgr = new_font_mgr;
            self.atlas.clear();
            let _ = self.resize_terminal();
        } else if size_changed {
            self.update_font_size(new_font_size);
        } else if padding_changed {
            let _ = self.resize_terminal();
        }

        self.needs_redraw = true;
        crate::alloc::trim_memory();
    }

    /// Executes a semantic shortcut action (e.g. scroll page up, zoom font).
    pub fn handle_key_action(
        &mut self,
        action: KeyAction,
        qh: Option<&QueueHandle<Self>>,
        conn: Option<&Connection>,
    ) {
        match action {
            KeyAction::ScrollbackUpPage => {
                self.terminal
                    .grid
                    .scroll_viewport_up(self.terminal.grid.rows);
                self.needs_redraw = true;
            }
            KeyAction::ScrollbackDownPage => {
                self.terminal
                    .grid
                    .scroll_viewport_down(self.terminal.grid.rows);
                self.needs_redraw = true;
            }
            KeyAction::ScrollbackUpLine => {
                self.terminal.grid.scroll_viewport_up(1);
                self.needs_redraw = true;
            }
            KeyAction::ScrollbackDownLine => {
                self.terminal.grid.scroll_viewport_down(1);
                self.needs_redraw = true;
            }
            KeyAction::ScrollbackHome => {
                self.terminal.grid.scroll_viewport_top();
                self.needs_redraw = true;
            }
            KeyAction::ScrollbackEnd => {
                self.terminal.grid.scroll_viewport_bottom();
                self.needs_redraw = true;
            }
            KeyAction::FontIncrease => {
                let new_size = (self.font_mgr.font_size() + 1.0).min(crate::font::MAX_FONT_SIZE);
                self.update_font_size(new_size);
            }
            KeyAction::FontDecrease => {
                let new_size = (self.font_mgr.font_size() - 1.0).max(crate::font::MIN_FONT_SIZE);
                self.update_font_size(new_size);
            }
            KeyAction::FontReset => {
                let default_size = self.config.font_size();
                self.update_font_size(default_size);
            }
            KeyAction::ClipboardCopy => {
                self.copy_selection(qh);
            }
            KeyAction::ClipboardPaste | KeyAction::PrimaryPaste => {
                self.paste_clipboard(conn);
            }
        }
    }
}
