//! ftty - Ultra-lightweight, minimalist Wayland terminal emulator with native Kitty graphics protocol

// `unwrap`/`expect` are only acceptable inside the test suite; production paths must
// propagate errors. Cargo.toml enables `clippy::unwrap_used`/`expect_used` crate-wide.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod color;
pub mod config;
pub mod event_loop;
pub mod font;
pub mod grid;
pub mod input;
pub use input::ime;
pub use input::mouse;
pub use input::selection;
pub mod kitty;
pub mod parser;
// Audited FFI boundaries: EGL/OpenGL and the PTY ioctl wrappers.
#[allow(unsafe_code)]
pub mod pty;
#[allow(unsafe_code)]
pub mod render;
pub use render::box_drawing;
pub mod wayland;

pub use color::{Color, Rgb, default_256_palette};
pub use config::Config;
pub use event_loop::{AppState, run_event_loop};
pub use font::{CellMetrics, FontManager, GlyphAtlas};
pub use grid::{Cell, CellFlags, ClearMode, Cursor, CursorShape, Grid, Row};
pub use ime::{ImeState, Preedit, calculate_cursor_rect};
pub use input::{KeyAction, KeyboardHandler, parse_key_combo};
pub use kitty::{
    DeleteTarget, ImageData, ImagePlacement, KittyAction, KittyCommand, KittyEvent, KittyFormat,
    KittyMedium, KittyParser, kitty_response,
};
pub use mouse::{MouseEncoding, MouseModifiers, MouseState, MouseTracking, encode_mouse_event};
pub use parser::Terminal;
pub use pty::Pty;
pub use render::{ColorScheme, HoveredHyperlinkSpan, RenderOptions, Renderer};
pub use selection::{Selection, SelectionPoint, SelectionType, find_word_boundaries};
pub use wayland::{OfferData, WaylandState};
