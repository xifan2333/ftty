//! ftty - Ultra-lightweight, minimalist Wayland terminal emulator with native Kitty graphics protocol

use std::path::PathBuf;

use ftty::{AppState, Config, Pty, Terminal, run_event_loop_with_connection};

fn print_help() {
    println!(
        "ftty v{} — Ultra-lightweight minimalist Wayland terminal emulator\n\n\
        Usage: ftty [options] [-e <command> [args...]]\n\n\
        Options:\n  \
          -c, --config <path>    Path to configuration file\n  \
          -e <command> ...       Execute command instead of default shell\n  \
          -h, --help             Print this help message\n  \
          -v, --version          Print version information\n",
        env!("CARGO_PKG_VERSION")
    );
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut config_path = None;
    let mut custom_command: Option<Vec<String>> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                return;
            }
            "-v" | "--version" => {
                println!("ftty v{}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "-c" | "--config" => {
                if let Some(path) = args.next() {
                    config_path = Some(PathBuf::from(path));
                } else {
                    eprintln!("ftty: missing argument for --config");
                    std::process::exit(1);
                }
            }
            "-e" => {
                let rest: Vec<String> = args.collect();
                if rest.is_empty() {
                    eprintln!("ftty: missing command for -e");
                    std::process::exit(1);
                }
                custom_command = Some(rest);
                break;
            }
            other => {
                eprintln!("ftty: unrecognized option '{other}'. Run with --help for usage.");
                std::process::exit(1);
            }
        }
    }

    let config = match Config::load_from_path_or_default(config_path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ftty: failed to load configuration: {e}");
            std::process::exit(1);
        }
    };

    let has_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
        || std::env::var_os("WAYLAND_SOCKET").is_some_and(|v| !v.is_empty());

    if !has_wayland {
        println!(
            "ftty v{} - Minimalist Wayland Terminal Emulator",
            env!("CARGO_PKG_VERSION")
        );
        println!("No active Wayland compositor detected. Core initialized successfully.");
        return;
    }

    // Step 1: Connect to Wayland and flush registry request over IPC at t = 1ms, overlapping
    // socket negotiation and compositor global announcements with PTY spawning and font loading.
    let conn = match wayland_client::Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ftty: failed to connect to Wayland: {e}");
            std::process::exit(1);
        }
    };
    let event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    conn.display().get_registry(&qh, ());
    let _ = conn.flush();

    // Step 2: Concurrently spawn font loader thread and PTY child process
    let font_families = config.font_families();
    let font_size = config.font_size();
    let font_thread = std::thread::Builder::new()
        .name("font-loader".to_string())
        .spawn(move || ftty::FontManager::load_with_families(&font_families, font_size));

    let cols = config.columns();
    let rows = config.rows();

    let term = Terminal::new(cols as usize, rows as usize, config.scrollback_lines());
    let cmd_slice: Option<Vec<&str>> = custom_command
        .as_ref()
        .map(|v| v.iter().map(String::as_str).collect());
    let pty = match Pty::spawn(cmd_slice.as_deref(), cols, rows) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ftty: failed to spawn PTY: {e}");
            std::process::exit(1);
        }
    };

    // Step 3: Await font loader before constructing complete AppState
    let font_mgr = match font_thread.map(|h| h.join()) {
        Ok(Ok(Ok(mgr))) => mgr,
        Ok(Ok(Err(e))) => {
            eprintln!("ftty: failed to initialize font: {e}");
            std::process::exit(1);
        }
        _ => {
            eprintln!("ftty: font loader thread failed");
            std::process::exit(1);
        }
    };

    let app_state = match AppState::with_font_and_config(term, pty, font_mgr, config, config_path) {
        Ok(state) => state,
        Err(e) => {
            eprintln!("ftty: failed to initialize state: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = run_event_loop_with_connection(app_state, conn, event_queue) {
        eprintln!("ftty error: {e}");
        std::process::exit(1);
    }
}
