//! ftty - Ultra-lightweight, minimalist Wayland terminal emulator with native Kitty graphics protocol

use std::path::PathBuf;

use ftty::{AppState, Config, Pty, Terminal, run_event_loop_with_connection};

fn print_help() {
    println!(
        "ftty v{} — Ultra-lightweight minimalist Wayland terminal emulator\n\n\
        Usage: ftty [options] [-e <command> [args...]]\n\n\
        Options:\n  \
          -a, --app-id <id>              Set Wayland window app-id (default: ftty)\n  \
          -T, --title <title>            Set initial window title (default: ftty)\n  \
          -d, --working-directory <path> Initial working directory\n  \
          -c, --config <path>            Path to configuration file\n  \
          -e <command> ...               Execute command instead of default shell\n  \
          -h, --help                     Print this help message\n  \
          -v, --version                  Print version information\n",
        env!("CARGO_PKG_VERSION")
    );
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut config_path = None;
    let mut custom_command: Option<Vec<String>> = None;
    let mut app_id = None;
    let mut title = None;
    let mut working_dir = None;

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
            "-a" | "--app-id" => {
                if let Some(id) = args.next() {
                    app_id = Some(id);
                } else {
                    eprintln!("ftty: missing argument for --app-id");
                    std::process::exit(1);
                }
            }
            "-T" | "--title" => {
                if let Some(t) = args.next() {
                    title = Some(t);
                } else {
                    eprintln!("ftty: missing argument for --title");
                    std::process::exit(1);
                }
            }
            "-d" | "--working-directory" => {
                if let Some(dir) = args.next() {
                    let path = PathBuf::from(dir);
                    if !path.is_dir() {
                        eprintln!(
                            "ftty: working directory '{}' does not exist or is not a directory",
                            path.display()
                        );
                        std::process::exit(1);
                    }
                    working_dir = Some(path);
                } else {
                    eprintln!("ftty: missing argument for --working-directory");
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

    // Step 1: Connect to Wayland
    let conn = match wayland_client::Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ftty: failed to connect to Wayland: {e}");
            std::process::exit(1);
        }
    };
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    conn.display().get_registry(&qh, ());
    let _ = conn.flush();

    // Step 2: Load font manager synchronously with 100% fidelity
    let font_mgr =
        match ftty::FontManager::load_with_families(&config.font_families(), config.font_size()) {
            Ok(fm) => fm,
            Err(e) => {
                eprintln!("ftty: failed to load fonts: {e}");
                std::process::exit(1);
            }
        };

    let cols = 80;
    let rows = 24;

    let term = Terminal::new(cols as usize, rows as usize, config.scrollback_lines());
    let cmd_slice: Option<Vec<&str>> = custom_command
        .as_ref()
        .map(|v| v.iter().map(String::as_str).collect());
    let pty = match Pty::spawn_with_dir(cmd_slice.as_deref(), cols, rows, working_dir.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ftty: failed to spawn PTY: {e}");
            std::process::exit(1);
        }
    };

    let mut app_state =
        match AppState::with_font_and_config(term, pty, font_mgr, config, config_path) {
            Ok(state) => state,
            Err(e) => {
                eprintln!("ftty: failed to initialize state: {e}");
                std::process::exit(1);
            }
        };

    if let Some(id) = app_id {
        app_state.wayland.app_id = id;
    }
    if let Some(t) = title {
        app_state.terminal.title = t.clone();
        app_state.wayland.title = t;
    }

    // Step 3: Dispatch any compositor globals that arrived during font/pty loading.
    // If the surface was already committed during dispatch, flush immediately to avoid
    // an extra synchronous roundtrip stall; otherwise, perform roundtrip as fallback.
    let _ = event_queue.dispatch_pending(&mut app_state);
    if app_state.wayland.surface.is_none()
        && let Err(e) = event_queue.roundtrip(&mut app_state)
    {
        eprintln!("ftty: initial Wayland roundtrip failed: {e}");
        std::process::exit(1);
    }
    if let Err(e) = conn.flush() {
        eprintln!("ftty: initial Wayland flush failed: {e}");
        std::process::exit(1);
    }

    ftty::trim_memory();

    if let Err(e) = run_event_loop_with_connection(app_state, conn, event_queue) {
        eprintln!("ftty error: {e}");
        std::process::exit(1);
    }
}
