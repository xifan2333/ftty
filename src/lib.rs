//! ftty - Ultra-lightweight, minimalist Wayland terminal emulator with native Kitty graphics protocol

// `unwrap`/`expect` are only acceptable inside the test suite; production paths must
// propagate errors. Cargo.toml enables `clippy::unwrap_used`/`expect_used` crate-wide.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod color;
pub mod config;
pub mod error;
pub mod event_loop;
pub mod font;
pub mod grid;
pub mod input;
pub mod kitty;
pub mod parser;
// Audited FFI boundaries: EGL/OpenGL, POSIX PTY, and GNU allocator tuning.
#[allow(unsafe_code)]
pub mod alloc;
#[allow(unsafe_code)]
pub mod pty;
#[allow(unsafe_code)]
pub mod render;
pub mod wayland;

pub use alloc::trim_memory;
pub use color::{Color, Rgb, default_256_palette};
pub use config::{Config, FreeTypeLoadFlags, FreeTypeLoadTarget, FreeTypeRenderTarget};
pub use error::{
    ConfigError, FontError, FttyError, KittyError, PtyError, RenderError, WaylandError,
};
pub use event_loop::{AppState, run_event_loop, run_event_loop_with_connection};
pub use font::{CellMetrics, FontManager, FreeTypeConfig, GlyphAtlas};
pub use grid::{Cell, CellFlags, ClearMode, Cursor, CursorShape, Grid, Row};
pub use input::ime::{ImeState, Preedit, calculate_cursor_rect};
pub use input::mouse::{
    MouseEncoding, MouseModifiers, MouseState, MouseTracking, encode_mouse_event,
};
pub use input::selection::{Selection, SelectionPoint, SelectionType, find_word_boundaries};
pub use input::{KeyAction, KeyboardHandler, parse_key_combo};
pub use kitty::{
    DeleteTarget, ImageData, ImagePlacement, KittyAction, KittyCommand, KittyEvent, KittyFormat,
    KittyMedium, KittyParser, kitty_response,
};
pub use parser::{Terminal, VtParser};
pub use pty::Pty;
pub use render::{ColorScheme, HoveredHyperlinkSpan, RenderOptions, Renderer};
pub use wayland::{OfferData, WaylandState, WindowState};
