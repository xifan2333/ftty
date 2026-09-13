//! ftty - Ultra-lightweight, Suckless Wayland terminal emulator with native Kitty graphics protocol

pub mod color;
pub mod grid;
pub mod parser;
pub mod pty;

pub use color::{Color, Rgb, default_256_palette};
pub use grid::{Cell, CellFlags, ClearMode, Cursor, CursorShape, Grid, Row};
pub use parser::Terminal;
pub use pty::Pty;
