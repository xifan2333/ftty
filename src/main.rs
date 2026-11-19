//! ftty - Ultra-lightweight, minimalist Wayland terminal emulator with native Kitty graphics protocol

use std::path::PathBuf;

use ftty::{AppState, Config, Pty, Terminal, TerminalSession, run_event_loop_with_connection};

/// How the CLI invocation resolved.
#[derive(Debug, PartialEq)]
enum CliAction {
    Help,
    Version,
    Run(CliOptions),
}

/// Parsed CLI options for launching a terminal session.
#[derive(Debug, PartialEq)]
struct CliOptions {
    config_path: Option<PathBuf>,
    custom_command: Option<Vec<String>>,
    app_id: Option<String>,
    title: Option<String>,
    working_dir: Option<PathBuf>,
}

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
    match parse_cli_args() {
        Ok(CliAction::Help) => print_help(),
        Ok(CliAction::Version) => println!("ftty v{}", env!("CARGO_PKG_VERSION")),
        Ok(CliAction::Run(opts)) => run_terminal(opts),
        Err(msg) => fatal(msg),
    }
}

/// Parses the process command line (excluding argv[0]) into a [`CliAction`].
fn parse_cli_args() -> Result<CliAction, String> {
    parse_cli_args_from(std::env::args().skip(1))
}

/// Parses CLI arguments from an injected iterator, so the parser is
/// unit-testable without touching the test process's own argv.
fn parse_cli_args_from(args: impl Iterator<Item = String>) -> Result<CliAction, String> {
    let mut args = args;
    let mut opts = CliOptions {
        config_path: None,
        custom_command: None,
        app_id: None,
        title: None,
        working_dir: None,
    };

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(CliAction::Help),
            "-v" | "--version" => return Ok(CliAction::Version),
            "-c" | "--config" => {
                opts.config_path = Some(PathBuf::from(next_value(&mut args, "--config")?));
            }
            "-a" | "--app-id" => {
                opts.app_id = Some(next_value(&mut args, "--app-id")?);
            }
            "-T" | "--title" => {
                opts.title = Some(next_value(&mut args, "--title")?);
            }
            "-d" | "--working-directory" => {
                let path = PathBuf::from(next_value(&mut args, "--working-directory")?);
                if !path.is_dir() {
                    return Err(format!(
                        "ftty: working directory '{}' does not exist or is not a directory",
                        path.display()
                    ));
                }
                opts.working_dir = Some(path);
            }
            "-e" => {
                let rest: Vec<String> = args.collect();
                if rest.is_empty() {
                    return Err("ftty: missing command for -e".to_string());
                }
                opts.custom_command = Some(rest);
                break;
            }
            other => {
                return Err(format!(
                    "ftty: unrecognized option '{other}'. Run with --help for usage."
                ));
            }
        }
    }

    Ok(CliAction::Run(opts))
}

/// Returns the next argument for an option, or an error when it is missing.
fn next_value(args: &mut impl Iterator<Item = String>, option: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("ftty: missing argument for {option}"))
}

/// Runs the terminal with the given parsed options until exit.
fn run_terminal(opts: CliOptions) {
    let config = match Config::load_from_path_or_default(opts.config_path.as_deref()) {
        Ok(c) => c,
        Err(e) => fatal(format!("ftty: failed to load configuration: {e}")),
    };

    if !wayland_env_present() {
        println!(
            "ftty v{} - Minimalist Wayland Terminal Emulator",
            env!("CARGO_PKG_VERSION")
        );
        println!("No active Wayland compositor detected. Core initialized successfully.");
        return;
    }

    let (conn, mut event_queue) = connect_to_wayland();
    let font_mgr =
        match ftty::FontManager::load_with_families(&config.font_families(), config.font_size()) {
            Ok(fm) => fm,
            Err(e) => fatal(format!("ftty: failed to load fonts: {e}")),
        };

    let cols = 80;
    let rows = 24;
    let session = spawn_session(&opts, &config, cols, rows);

    let mut app_state =
        match AppState::with_font_and_config(session, font_mgr, config, opts.config_path) {
            Ok(state) => state,
            Err(e) => fatal(format!("ftty: failed to initialize state: {e}")),
        };

    if let Some(id) = opts.app_id {
        app_state.wayland.app_id = id;
    }
    if let Some(title) = opts.title {
        app_state.session.terminal.title = title.clone();
        app_state.wayland.title = title;
    }

    initial_wayland_roundtrip(&conn, &mut event_queue, &mut app_state);

    ftty::trim_memory();

    match run_event_loop_with_connection(app_state, conn, event_queue) {
        Ok(()) => {}
        Err(e) => fatal(format!("ftty error: {e}")),
    }
}

/// Whether a usable Wayland display socket is present in the environment.
fn wayland_env_present() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
        || std::env::var_os("WAYLAND_SOCKET").is_some_and(|v| !v.is_empty())
}

/// Connects to the Wayland compositor and registers the display registry.
fn connect_to_wayland() -> (
    wayland_client::Connection,
    wayland_client::EventQueue<AppState>,
) {
    let conn = match wayland_client::Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => fatal(format!("ftty: failed to connect to Wayland: {e}")),
    };
    let event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    conn.display().get_registry(&qh, ());
    let _ = conn.flush();
    (conn, event_queue)
}

/// Spawns the PTY-backed terminal session with the requested command or the
/// default shell, in the requested working directory.
fn spawn_session(opts: &CliOptions, config: &Config, cols: u16, rows: u16) -> TerminalSession {
    let cmd_slice: Option<Vec<&str>> = opts
        .custom_command
        .as_ref()
        .map(|v| v.iter().map(String::as_str).collect());
    let term = Terminal::new(cols as usize, rows as usize, config.scrollback_lines());
    let pty = match Pty::spawn_with_dir(
        cmd_slice.as_deref(),
        cols,
        rows,
        opts.working_dir.as_deref(),
    ) {
        Ok(p) => p,
        Err(e) => fatal(format!("ftty: failed to spawn PTY: {e}")),
    };
    TerminalSession::new(term, pty)
}

/// Initial compositor roundtrip: dispatch any globals that arrived during
/// font/PTY loading, then flush. If the surface was already committed during
/// dispatch, the roundtrip is skipped to avoid a synchronous stall.
fn initial_wayland_roundtrip(
    conn: &wayland_client::Connection,
    event_queue: &mut wayland_client::EventQueue<AppState>,
    app_state: &mut AppState,
) {
    let _ = event_queue.dispatch_pending(app_state);
    if app_state.wayland.surface.is_none()
        && let Err(e) = event_queue.roundtrip(app_state)
    {
        fatal(format!("ftty: initial Wayland roundtrip failed: {e}"));
    }
    if let Err(e) = conn.flush() {
        fatal(format!("ftty: initial Wayland flush failed: {e}"));
    }
}

/// Prints a fatal error message and terminates with exit code 1.
fn fatal(msg: impl std::fmt::Display) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<CliAction, String> {
        parse_cli_args_from(args.iter().map(|s| s.to_string()))
    }

    fn parse_ok(args: &[&str]) -> CliOptions {
        match parse(args) {
            Ok(CliAction::Run(opts)) => opts,
            other => panic!("expected Run, got {other:?}"),
        }
    }

    fn parse_err(args: &[&str]) -> String {
        match parse(args) {
            Err(msg) => msg,
            Ok(action) => panic!("expected error for {args:?}, got {action:?}"),
        }
    }

    #[test]
    fn test_parse_help_and_version() {
        assert!(matches!(parse(&["-h"]), Ok(CliAction::Help)));
        assert!(matches!(parse(&["--help"]), Ok(CliAction::Help)));
        assert!(matches!(parse(&["-v"]), Ok(CliAction::Version)));
        assert!(matches!(parse(&["--version"]), Ok(CliAction::Version)));
    }

    #[test]
    fn test_parse_options() {
        let o = parse_ok(&[
            "-c",
            "/tmp/c.toml",
            "-a",
            "my-app",
            "-T",
            "My Title",
            "-d",
            "/tmp",
            "-e",
            "vim",
            "a.txt",
        ]);
        assert_eq!(
            o.config_path.as_deref(),
            Some(std::path::Path::new("/tmp/c.toml"))
        );
        assert_eq!(o.app_id.as_deref(), Some("my-app"));
        assert_eq!(o.title.as_deref(), Some("My Title"));
        assert_eq!(o.working_dir.as_deref(), Some(std::path::Path::new("/tmp")));
        assert_eq!(
            o.custom_command.as_deref(),
            Some(["vim".to_string(), "a.txt".to_string()].as_slice())
        );
    }

    #[test]
    fn test_parse_errors() {
        assert!(parse_err(&["--bogus"]).contains("unrecognized option"));
        assert!(parse_err(&["-c"]).contains("missing argument for --config"));
        assert!(parse_err(&["-e"]).contains("missing command for -e"));
        assert!(parse_err(&["-d", "/definitely/not/a/real/dir-xyz123"]).contains("does not exist"));
    }

    #[test]
    fn test_defaults_when_no_options() {
        let o = parse_ok(&[]);
        assert!(o.config_path.is_none());
        assert!(o.custom_command.is_none());
        assert!(o.app_id.is_none());
        assert!(o.title.is_none());
        assert!(o.working_dir.is_none());
    }
}
