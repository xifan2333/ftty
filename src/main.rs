//! ftty - Ultra-lightweight, Suckless Wayland terminal emulator with native Kitty graphics protocol

use ftty::{AppState, Pty, Terminal, run_event_loop};

fn main() {
    let term = Terminal::new(80, 24, 1000);
    let pty = match Pty::spawn(None, 80, 24) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ftty: failed to spawn PTY: {e}");
            std::process::exit(1);
        }
    };

    let app_state = AppState::new(term, pty);

    if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("WAYLAND_SOCKET").is_none()
    {
        println!(
            "ftty v{} - Suckless Wayland Terminal Emulator",
            env!("CARGO_PKG_VERSION")
        );
        println!("No active Wayland compositor detected. Core initialized successfully.");
        return;
    }

    if let Err(e) = run_event_loop(app_state) {
        eprintln!("ftty error: {e}");
        std::process::exit(1);
    }
}
