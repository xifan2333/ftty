//! ftty - Ultra-lightweight, Suckless Wayland terminal emulator with native Kitty graphics protocol

pub mod color;
pub mod event_loop;
pub mod grid;
pub mod input;
pub mod parser;
pub mod pty;
pub mod wayland;

pub use color::{Color, Rgb, default_256_palette};
pub use event_loop::{AppState, run_event_loop};
pub use grid::{Cell, CellFlags, ClearMode, Cursor, CursorShape, Grid, Row};
pub use input::KeyboardHandler;
pub use parser::Terminal;
pub use pty::Pty;
pub use wayland::WaylandState;
