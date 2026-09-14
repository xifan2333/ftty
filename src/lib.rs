//! ftty - Ultra-lightweight, Suckless Wayland terminal emulator with native Kitty graphics protocol

pub mod color;
pub mod config;
pub mod event_loop;
pub mod font;
pub mod grid;
pub mod ime;
pub mod input;
pub mod parser;
pub mod pty;
pub mod render;
pub mod selection;
pub mod wayland;

pub use color::{Color, Rgb, default_256_palette};
pub use config::Config;
pub use event_loop::{AppState, run_event_loop};
pub use font::{CellMetrics, FontManager, GlyphAtlas};
pub use grid::{Cell, CellFlags, ClearMode, Cursor, CursorShape, Grid, Row};
pub use ime::{ImeState, Preedit, calculate_cursor_rect};
pub use input::{KeyAction, KeyboardHandler, parse_key_combo};
pub use parser::Terminal;
pub use pty::Pty;
pub use render::{ColorScheme, RenderOptions, Renderer};
pub use selection::{Selection, SelectionPoint, SelectionType, find_word_boundaries};
pub use wayland::{OfferData, WaylandState};
