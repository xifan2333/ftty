//! Application control actions: configuration reload, key shortcuts, and PTY writes.

use std::io::{self, Write};
use std::os::fd::AsFd;

use nix::poll::{PollFd, PollFlags, poll};
use wayland_client::{Connection, QueueHandle};

use crate::config::Config;
use crate::event_loop::AppState;
use crate::font::FontManager;
use crate::input::KeyAction;

static ACTIVE_PIPELINES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const MAX_CONCURRENT_PIPELINES: usize = 8;

struct PipelineGuard;
impl Drop for PipelineGuard {
    fn drop(&mut self) {
        ACTIVE_PIPELINES.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

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

        let active_subpixel = self.is_subpixel_enabled();
        let active_bgr = self.is_bgr_subpixel();
        let maybe_new_font = if families_changed {
            match FontManager::load_with_families_and_subpixel(
                &new_config.font_families(),
                new_config.font_size(),
                active_subpixel,
            ) {
                Ok(mut mgr) => {
                    mgr.bgr = active_bgr;
                    Some(mgr)
                }
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
        self.terminal.set_palette(self.palette);
        self.terminal.allow_osc52_read = new_config.allow_osc52_read();
        self.terminal.allow_osc52_write = new_config.allow_osc52_write();
        self.terminal.grid.cursor.shape = new_config.cursor_shape();
        self.keyboard.update_keybindings(&new_config.keybindings);
        self.config = new_config;

        self.terminal.grid.mark_all_dirty();
        if let Some(renderer) = &mut self.renderer {
            renderer.clear_cache();
        }

        self.logical_font_size = new_font_size;
        let factor = if self.wayland.is_fractional_scale_active() {
            self.wayland.scale_factor
        } else {
            1.0
        };
        let scaled_font_size = (self.logical_font_size * factor as f32).clamp(
            crate::font::MIN_FONT_SIZE,
            crate::font::MAX_RASTER_FONT_SIZE,
        );

        if let Some(new_font_mgr) = maybe_new_font {
            self.font_mgr = new_font_mgr;
            self.atlas.clear();
            self.update_font_size(scaled_font_size);
            let _ = self.resize_terminal();
        } else if size_changed {
            self.update_font_size(scaled_font_size);
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
            KeyAction::PromptPrev => {
                self.terminal.grid.scroll_to_prompt_prev();
                self.needs_redraw = true;
            }
            KeyAction::PromptNext => {
                self.terminal.grid.scroll_to_prompt_next();
                self.needs_redraw = true;
            }
            KeyAction::FontIncrease => {
                let new_size = (self.logical_font_size + 1.0).min(crate::font::MAX_FONT_SIZE);
                self.set_logical_font_size(new_size);
            }
            KeyAction::FontDecrease => {
                let new_size = (self.logical_font_size - 1.0).max(crate::font::MIN_FONT_SIZE);
                self.set_logical_font_size(new_size);
            }
            KeyAction::FontReset => {
                let default_size = self.config.font_size();
                self.set_logical_font_size(default_size);
            }
            KeyAction::ClipboardCopy => {
                self.copy_selection(qh);
            }
            KeyAction::ClipboardPaste | KeyAction::PrimaryPaste => {
                self.paste_clipboard(conn);
            }
            KeyAction::PipeVisible(cmd) => {
                let text = self.terminal.grid.extract_visible_text();
                Self::spawn_pipe_async(cmd, text);
            }
            KeyAction::PipeScrollback(cmd) => {
                let text = self.terminal.grid.extract_scrollback_text();
                Self::spawn_pipe_async(cmd, text);
            }
            KeyAction::PipeSelection(cmd) => {
                let text = self.selection.extract_text(&self.terminal.grid);
                if !text.is_empty() {
                    Self::spawn_pipe_async(cmd, text);
                }
            }
        }
    }

    pub(crate) fn spawn_pipe_async(cmd: Vec<String>, text: String) {
        if cmd.is_empty() {
            return;
        }
        if ACTIVE_PIPELINES.load(std::sync::atomic::Ordering::Relaxed) >= MAX_CONCURRENT_PIPELINES {
            eprintln!(
                "ftty: pipeline concurrency limit reached ({MAX_CONCURRENT_PIPELINES}), ignoring request"
            );
            return;
        }
        ACTIVE_PIPELINES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::thread::spawn(move || {
            let _guard = PipelineGuard;
            let (program, args) = match cmd.split_first() {
                Some((prog, args)) => (prog, args),
                None => return,
            };
            let mut child = match std::process::Command::new(program)
                .args(args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .spawn()
            {
                Ok(child) => child,
                Err(e) => {
                    eprintln!("ftty: failed to spawn pipe command '{program}': {e}");
                    return;
                }
            };

            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                if let Err(e) = stdin.write_all(text.as_bytes()).and_then(|_| stdin.flush())
                    && e.kind() != std::io::ErrorKind::BrokenPipe
                {
                    eprintln!("ftty: pipeline write error for '{program}': {e}");
                }
                drop(stdin);
            }
            let _ = child.wait();
        });
    }

    /// Sets the logical font size and updates font metrics scaled by the active display factor.
    pub fn set_logical_font_size(&mut self, size: f32) {
        self.logical_font_size = size;
        let factor = if self.wayland.is_fractional_scale_active() {
            self.wayland.scale_factor
        } else {
            1.0
        };
        let scaled_size = (size * factor as f32).clamp(
            crate::font::MIN_FONT_SIZE,
            crate::font::MAX_RASTER_FONT_SIZE,
        );
        self.update_font_size(scaled_size);
    }
}
