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
pub mod session;
pub mod theme;
// Audited FFI boundaries: EGL/OpenGL, POSIX PTY, and GNU allocator tuning.
#[allow(unsafe_code)]
pub mod alloc;
#[allow(unsafe_code)]
pub mod pty;
#[allow(unsafe_code)]
pub mod render;
pub mod wayland;
pub mod width;

// Crate prelude: only names the binary or the test suite reaches for at the root.
// Everything else is addressed through its module (`grid::Grid`, `error::FttyError`), so
// each item keeps exactly one path. Adding a name here for convenience gives it a second
// one; add it where it is defined instead.
pub use alloc::trim_memory;
pub use config::Config;
pub use event_loop::{AppState, run_event_loop_with_connection};
pub use font::FontManager;
pub use parser::Terminal;
pub use pty::Pty;
pub use session::TerminalSession;
pub use theme::Theme;
