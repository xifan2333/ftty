//! ftty - Ultra-lightweight, Suckless Wayland terminal emulator with native Kitty graphics protocol

use ftty::Terminal;

fn main() {
    println!(
        "ftty v{} - Suckless Wayland Terminal Emulator",
        env!("CARGO_PKG_VERSION")
    );

    let mut term = Terminal::new(80, 24, 1000);
    term.advance_bytes(b"\x1b[1;32mftty core initialized\x1b[0m\r\n");
    println!("Grid dimensions: {}x{}", term.grid.cols, term.grid.rows);
}
