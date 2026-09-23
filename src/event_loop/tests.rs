use std::io::{Read, Write};
use std::time::Duration;

use crate::color::Rgb;
use crate::event_loop::{AppState, terminal_size};
use crate::font::CellMetrics;
use crate::input::KeyAction;
use crate::input::selection::{Selection, SelectionPoint, SelectionType};
use crate::parser::Terminal;
use crate::pty::Pty;
use crate::render::HoveredHyperlinkSpan;

#[test]
fn terminal_dimensions_use_metrics_and_fit_the_pty() {
    let metrics = CellMetrics {
        cell_width: 9,
        cell_height: 18,
        ascent: 14,
    };
    assert_eq!(terminal_size([720, 480], metrics, [0, 0]), (80, 26));
    assert_eq!(terminal_size([720, 480], metrics, [18, 18]), (76, 24));
    assert_eq!(terminal_size([1, 1], metrics, [0, 0]), (1, 1));
    assert_eq!(
        terminal_size([u32::MAX, u32::MAX], metrics, [0, 0]),
        (u16::MAX, u16::MAX)
    );
}

#[test]
fn test_app_state_initialization() {
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let app = AppState::new(term, pty).expect("AppState new");

    assert!(app.running);
    assert_eq!(app.terminal.grid.cols, 80);
    assert_eq!(app.terminal.grid.rows, 24);
    assert!(app.font_mgr.metrics.cell_width > 0);
    assert!(app.font_mgr.metrics.cell_height > 0);
    assert!(!app.wayland.configured);
}

#[test]
fn test_pty_and_terminal_roundtrip() {
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app = AppState::new(term, pty).expect("AppState new");

    // Send a command to shell via PTY
    app.pty
        .write_all(b"echo ftty_test_ok\n")
        .expect("write to pty");

    // Wait briefly and read output back into terminal
    let mut received = false;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(20));
        let mut buf = [0u8; 1024];
        if let Ok(n) = app.pty.read(&mut buf)
            && n > 0
        {
            app.terminal.advance_bytes(&mut app.vt_parser, &buf[..n]);
            let full_screen: String = app
                .terminal
                .grid
                .lines
                .iter()
                .flat_map(|r| r.cells.iter().map(|c| c.c))
                .collect();
            if full_screen.contains("ftty_test_ok") {
                received = true;
                break;
            }
        }
    }
    assert!(received, "Expected shell echo in terminal grid");
}

#[test]
fn test_app_state_reload_config() {
    use crate::grid::CursorShape;

    let temp_dir = std::env::temp_dir().join(format!("ftty_reload_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&temp_dir);
    let config_path = temp_dir.join("ftty.toml");

    std::fs::write(
        &config_path,
        r##"
        [cursor]
        shape = "block"

        [colors]
        foreground = "#ffffff"
        background = "#000000"
        "##,
    )
    .unwrap();

    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app =
        AppState::with_config(term, pty, Some(config_path.clone())).expect("AppState with_config");

    assert_eq!(app.default_bg, Rgb::new(0, 0, 0));
    assert_eq!(app.default_fg, Rgb::new(255, 255, 255));
    assert_eq!(app.terminal.grid.cursor.shape, CursorShape::Block);

    // Update config file with new colors, cursor, and padding
    std::fs::write(
        &config_path,
        r##"
        [window]
        padding = [20, 20]

        [cursor]
        shape = "underline"

        [colors]
        foreground = "#ff0000"
        background = "#123456"
        "##,
    )
    .unwrap();

    app.needs_redraw = false;
    app.reload_config();

    assert_eq!(app.default_bg, Rgb::new(18, 52, 86));
    assert_eq!(app.default_fg, Rgb::new(255, 0, 0));
    assert_eq!(app.terminal.grid.cursor.shape, CursorShape::Underline);
    assert_eq!(app.config.padding_x(), 20);
    assert_eq!(app.config.padding_y(), 20);
    assert!(app.needs_redraw);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_combined_reload_updates_size_freetype_and_padding() {
    use crate::config::{FreeTypeLoadFlags, FreeTypeLoadTarget};

    let temp_dir = std::env::temp_dir().join(format!("ftty_reload_comb_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&temp_dir);
    let config_path = temp_dir.join("ftty.toml");

    std::fs::write(
        &config_path,
        r##"
        [font]
        size = 13.0
        freetype_load_flags = "DEFAULT"
        freetype_load_target = "Light"

        [window]
        padding = [10, 10]
        "##,
    )
    .unwrap();

    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app =
        AppState::with_config(term, pty, Some(config_path.clone())).expect("AppState with_config");

    assert_eq!(app.font_mgr.font_size(), 13.0);
    assert_eq!(
        app.font_mgr.ft_config.load_flags,
        FreeTypeLoadFlags::Default
    );

    // Concurrently change font size, FreeType options, and window padding in one reload
    std::fs::write(
        &config_path,
        r##"
        [font]
        size = 15.0
        freetype_load_flags = "NO_HINTING"
        freetype_load_target = "Normal"

        [window]
        padding = [25, 25]
        "##,
    )
    .unwrap();

    app.reload_config();

    // Verify all 3 changes were applied coordinately without dropping any setting
    assert_eq!(app.font_mgr.font_size(), 15.0);
    assert_eq!(
        app.font_mgr.ft_config.load_flags,
        FreeTypeLoadFlags::NoHinting
    );
    assert_eq!(
        app.font_mgr.ft_config.load_target,
        FreeTypeLoadTarget::Normal
    );
    assert_eq!(app.config.padding_x(), 25);
    assert_eq!(app.config.padding_y(), 25);
    assert!(app.needs_redraw);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_reload_config_synchronizes_subpixel_mode() {
    let temp_dir =
        std::env::temp_dir().join(format!("ftty_subpixel_reload_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&temp_dir);
    let config_path = temp_dir.join("ftty.toml");

    std::fs::write(
        &config_path,
        r##"
        [font]
        size = 9.0
        freetype_render_target = "Normal"
        "##,
    )
    .unwrap();

    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app =
        AppState::with_config(term, pty, Some(config_path.clone())).expect("AppState with_config");

    // Initially with Normal render target, subpixel must be disabled
    assert!(!app.is_subpixel_enabled());
    assert!(!app.font_mgr.subpixel);

    // Reload with HorizontalLcd render target
    std::fs::write(
        &config_path,
        r##"
        [font]
        size = 9.0
        freetype_render_target = "HorizontalLcd"
        "##,
    )
    .unwrap();

    app.reload_config();
    assert_eq!(
        app.font_mgr.ft_config.render_target,
        crate::config::FreeTypeRenderTarget::HorizontalLcd
    );
    // In headless test without a renderer, is_subpixel_enabled is safely false
    assert_eq!(app.font_mgr.subpixel, app.is_subpixel_enabled());

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_failed_reload_preserves_state() {
    use crate::grid::CursorShape;

    let temp_dir = std::env::temp_dir().join(format!("ftty_reload_fail_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&temp_dir);
    let config_path = temp_dir.join("ftty.toml");

    std::fs::write(
        &config_path,
        r##"
        [scrollback]
        lines = 100

        [font]
        size = 14.0

        [cursor]
        shape = "block"

        [colors]
        foreground = "#ffffff"
        background = "#000000"
        "##,
    )
    .unwrap();

    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app =
        AppState::with_config(term, pty, Some(config_path.clone())).expect("AppState with_config");

    // Populate scrollback with lines
    for _ in 0..10 {
        app.terminal
            .grid
            .scrollback
            .push_back(crate::grid::Row::new(80));
    }
    assert_eq!(app.terminal.grid.scrollback.len(), 10);

    // Write an invalid font size (0.0), along with changed colors, cursor, and smaller scrollback
    std::fs::write(
        &config_path,
        r##"
        [scrollback]
        lines = 2

        [font]
        size = 0.0

        [cursor]
        shape = "underline"

        [colors]
        foreground = "#ff0000"
        background = "#123456"
        "##,
    )
    .unwrap();

    app.needs_redraw = false;
    app.reload_config();

    // Ensure state was NOT partially committed (scrollback must not be truncated!)
    assert_eq!(app.terminal.grid.scrollback.len(), 10);
    assert_eq!(app.terminal.grid.max_scrollback, 100);
    assert_eq!(app.default_bg, Rgb::new(0, 0, 0));
    assert_eq!(app.default_fg, Rgb::new(255, 255, 255));
    assert_eq!(app.terminal.grid.cursor.shape, CursorShape::Block);
    assert_eq!(app.font_mgr.font_size(), 14.0);
    assert!(!app.needs_redraw);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_handle_key_actions_scrolling_and_zoom() {
    use crate::color::Color;
    use crate::grid::CellFlags;

    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app = AppState::new(term, pty).expect("AppState new");

    // Populate lines and scrollback
    for _ in 0..50 {
        app.terminal.grid.write_char(
            'A',
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
        app.terminal.grid.newline();
    }

    assert_eq!(app.terminal.grid.viewport_offset(), 0);

    // Page Up
    app.handle_key_action(KeyAction::ScrollbackUpPage, None, None);
    assert_eq!(app.terminal.grid.viewport_offset(), 24);

    // Scroll to Top
    app.handle_key_action(KeyAction::ScrollbackHome, None, None);
    assert_eq!(
        app.terminal.grid.viewport_offset(),
        app.terminal.grid.scrollback.len()
    );

    // Line Down
    let top = app.terminal.grid.viewport_offset();
    app.handle_key_action(KeyAction::ScrollbackDownLine, None, None);
    assert_eq!(app.terminal.grid.viewport_offset(), top - 1);

    // Scroll to Bottom
    app.handle_key_action(KeyAction::ScrollbackEnd, None, None);
    assert_eq!(app.terminal.grid.viewport_offset(), 0);

    // Font zoom actions
    let initial_size = app.font_mgr.font_size();
    app.handle_key_action(KeyAction::FontIncrease, None, None);
    assert_eq!(app.font_mgr.font_size(), initial_size + 1.0);

    app.handle_key_action(KeyAction::FontDecrease, None, None);
    assert_eq!(app.font_mgr.font_size(), initial_size);

    app.handle_key_action(KeyAction::FontReset, None, None);
    assert_eq!(app.font_mgr.font_size(), app.config.font_size());
}

#[test]
fn test_copy_and_paste_clipboard() {
    use crate::color::Color;
    use crate::grid::CellFlags;

    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app = AppState::new(term, pty).expect("AppState new");

    for c in "copied_text".chars() {
        app.terminal.grid.write_char(
            c,
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
    }

    app.selection = Selection::new(
        SelectionPoint::new(0, 0),
        SelectionPoint::new(0, 10),
        SelectionType::Simple,
    );

    app.copy_selection(None);
    assert_eq!(app.clipboard_text, Some("copied_text".to_string()));

    app.paste_clipboard(None);
}

#[test]
fn test_pipe_visible_action_execution() {
    use crate::color::Color;
    use crate::grid::CellFlags;

    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app = AppState::new(term, pty).expect("AppState new");

    for c in "ftty_pipe_test".chars() {
        app.terminal.grid.write_char(
            c,
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
    }

    let temp_file = std::env::temp_dir().join(format!("ftty_pipe_out_{}", std::process::id()));
    let out_path = temp_file.to_string_lossy().to_string();

    let cmd = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!("cat > '{out_path}'"),
    ];
    app.handle_key_action(KeyAction::PipeVisible(cmd), None, None);

    let mut success = false;
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(20));
        if let Ok(content) = std::fs::read_to_string(&temp_file)
            && content.contains("ftty_pipe_test")
        {
            success = true;
            break;
        }
    }
    let _ = std::fs::remove_file(&temp_file);
    assert!(success, "piped output must contain 'ftty_pipe_test'");
}

#[test]
fn x11_buttons_map_to_protocol_indexes() {
    use crate::wayland::seat::x11_button_index;

    assert_eq!(x11_button_index(0x110), Some(0));
    assert_eq!(x11_button_index(0x111), Some(1));
    assert_eq!(x11_button_index(0x112), Some(2));
    assert_eq!(x11_button_index(0x113), None);
}

#[test]
fn mouse_reports_are_forwarded_only_when_tracking_is_enabled() {
    use crate::input::mouse::{MouseEncoding, MouseTracking};

    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app = AppState::new(term, pty).expect("AppState new");
    let cw = f64::from(app.font_mgr.metrics.cell_width);
    let ch = f64::from(app.font_mgr.metrics.cell_height);
    app.mouse_pos = [cw * 2.5, ch * 1.5];

    // Tracking disabled: the pointer stays available for local text selection.
    assert!(app.mouse_report_bytes(0, true, false).is_none());

    app.terminal.mouse.tracking = MouseTracking::Drag;
    app.terminal.mouse.encoding = MouseEncoding::Sgr;
    assert_eq!(
        app.mouse_report_bytes(0, true, false),
        Some(b"\x1b[<0;3;2M".to_vec())
    );
    assert_eq!(
        app.mouse_report_bytes(0, false, false),
        Some(b"\x1b[<0;3;2m".to_vec())
    );
    assert_eq!(
        app.mouse_report_bytes(64, true, false),
        Some(b"\x1b[<64;3;2M".to_vec())
    );

    // Shift is the conventional escape hatch back to local selection.
    app.keyboard.update_modifiers(1, 0, 0, 0);
    assert!(app.keyboard.modifiers().shift);
    assert!(app.mouse_report_bytes(0, true, false).is_none());
}

#[test]
fn test_cell_at_pointer_calculation() {
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let app = AppState::new(term, pty).expect("AppState new");

    let cw = f64::from(app.font_mgr.metrics.cell_width);
    let ch = f64::from(app.font_mgr.metrics.cell_height);

    let (line, screen_row, col) = app.cell_at_pointer(cw * 5.5, ch * 3.5);
    assert_eq!(col, 5);
    assert_eq!(screen_row, 3);
    assert_eq!(line, 3);
}

#[test]
fn test_fractional_scale_pointer_and_state() {
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app = AppState::new(term, pty).expect("AppState new");

    assert_eq!(app.wayland.scale_factor, 1.0);
    assert_eq!(app.wayland.preferred_scale_120, 120);

    app.wayland.scale_factor = 1.5;
    app.wayland.preferred_scale_120 = 180;

    let scale = 1.5;
    let logical_cw = f64::from(app.font_mgr.metrics.cell_width) / scale;
    let logical_ch = f64::from(app.font_mgr.metrics.cell_height) / scale;

    let (line, screen_row, col) = app.cell_at_pointer(logical_cw * 4.5, logical_ch * 2.5);
    assert_eq!(col, 4);
    assert_eq!(screen_row, 2);
    assert_eq!(line, 2);
}

#[test]
fn test_font_chain_reload_and_zoom_preserves_fallbacks() {
    let temp_dir = std::env::temp_dir().join(format!("ftty_font_chain_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&temp_dir);
    let config_path = temp_dir.join("ftty.toml");

    std::fs::write(
        &config_path,
        r##"
        [font]
        families = ["monospace", "sans-serif"]
        size = 15.0
        "##,
    )
    .unwrap();

    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app =
        AppState::with_config(term, pty, Some(config_path.clone())).expect("AppState with_config");

    assert_eq!(
        app.font_mgr.families(),
        &["monospace".to_string(), "sans-serif".to_string()]
    );
    assert_eq!(app.font_mgr.font_size(), 15.0);

    // Zoom font in: families chain must be preserved
    app.handle_key_action(KeyAction::FontIncrease, None, None);
    assert_eq!(app.font_mgr.font_size(), 16.0);
    assert_eq!(
        app.font_mgr.families(),
        &["monospace".to_string(), "sans-serif".to_string()]
    );

    // Reload with single family: families chain updates
    std::fs::write(
        &config_path,
        r##"
        [font]
        family = "monospace"
        size = 14.0
        "##,
    )
    .unwrap();

    app.reload_config();
    assert_eq!(app.font_mgr.families(), &["monospace".to_string()]);
    assert_eq!(app.font_mgr.font_size(), 14.0);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_update_hover_state_and_pointer_shape() {
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app = AppState::new(term, pty).expect("AppState new");

    // Intern a real URL so hyperlink_id 1 is resolvable
    let id = app
        .terminal
        .get_or_intern_hyperlink("https://example.com".to_string());
    assert_eq!(id, 1);

    // Cells at cols 4..=6 have hyperlink ID 1
    for col in 4..=6 {
        app.terminal.grid.lines[0].cells[col].set_hyperlink_id(Some(1));
    }
    // Cell at col 8 has hyperlink ID 0 (invalid/exhausted sentinel)
    app.terminal.grid.lines[0].cells[8].set_hyperlink_id(Some(0));

    let cw = f64::from(app.font_mgr.metrics.cell_width);
    let ch = f64::from(app.font_mgr.metrics.cell_height);

    // If pointer is not in surface, hover must be None
    app.pointer_in_surface = false;
    app.mouse_pos = [cw * 5.5, ch * 0.5];
    app.update_hover_state();
    assert_eq!(app.hovered_span, None);

    // Enter surface
    app.pointer_in_surface = true;

    // Pointer over cell (row 0, col 0) has no hyperlink
    app.mouse_pos = [cw * 0.5, ch * 0.5];
    app.update_hover_state();
    assert_eq!(app.hovered_span, None);

    // Move pointer over cell (row 0, col 5): detects contiguous span [4..=6]
    app.mouse_pos = [cw * 5.5, ch * 0.5];
    app.needs_redraw = false;
    app.update_hover_state();
    assert_eq!(
        app.hovered_span,
        Some(HoveredHyperlinkSpan {
            line: 0,
            start_col: 4,
            end_col: 6,
        })
    );
    assert!(app.needs_redraw);

    // Pointer over cell with sentinel id 0 (pool exhaustion) must produce no hover
    app.mouse_pos = [cw * 8.5, ch * 0.5];
    app.needs_redraw = false;
    app.update_hover_state();
    assert_eq!(app.hovered_span, None);
    assert!(app.needs_redraw);

    // Back to col 5
    app.mouse_pos = [cw * 5.5, ch * 0.5];
    app.update_hover_state();
    assert!(app.hovered_span.is_some());

    // If mouse is pressed (dragging selection), hover is suppressed
    app.mouse_pressed = true;
    app.needs_redraw = false;
    app.update_hover_state();
    assert_eq!(app.hovered_span, None);
    assert!(app.needs_redraw);

    // When mouse is released, hover is restored
    app.mouse_pressed = false;
    app.update_hover_state();
    assert_eq!(
        app.hovered_span,
        Some(HoveredHyperlinkSpan {
            line: 0,
            start_col: 4,
            end_col: 6,
        })
    );
}

#[test]
fn test_app_state_with_loaded_config() {
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let config = crate::config::Config::default();
    let app = AppState::with_loaded_config(term, pty, config, None).expect("with_loaded_config");
    assert_eq!(app.terminal.grid.cols, 80);
    assert_eq!(app.terminal.grid.rows, 24);
    assert!(app.font_mgr.metrics.cell_width > 0);
}

#[test]
fn test_app_state_with_font_and_config() {
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let config = crate::config::Config::default();
    let font_mgr = crate::font::FontManager::load(14.0).expect("load font");
    let app = AppState::with_font_and_config(term, pty, font_mgr, config, None)
        .expect("with_font_and_config");
    assert_eq!(app.terminal.grid.cols, 80);
    assert_eq!(app.terminal.grid.rows, 24);
    assert!(app.font_mgr.metrics.cell_width > 0);
    assert!(app.renderer.is_none());
}

#[test]
fn test_app_state_with_font_and_config_initialization() {
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let config = crate::config::Config::default();
    let font_mgr =
        crate::font::FontManager::load_with_families(&config.font_families(), config.font_size())
            .expect("load font");
    let app = AppState::with_font_and_config(term, pty, font_mgr, config, None)
        .expect("with_font_and_config");
    assert!(app.font_mgr.metrics.cell_width > 0);
    assert!(app.font_mgr.metrics.cell_height > 0);
}

#[test]
fn test_pre_event_loop_window_creation_and_surface_setup() {
    let (client, server) = std::os::unix::net::UnixStream::pair().expect("socketpair");
    let conn = wayland_client::Connection::from_socket(client).expect("conn");
    let queue = conn.new_event_queue::<AppState>();
    let qh = queue.handle();
    let registry = conn.display().get_registry(&qh, ());
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app = AppState::new(term, pty).expect("app");

    // Before binding globals, window remains unmapped
    assert_eq!(
        app.wayland.window_state,
        crate::wayland::WindowState::Unmapped
    );
    assert!(app.wayland.surface.is_none());

    // Bind compositor and xdg_wm_base globals into app
    let comp =
        registry.bind::<wayland_client::protocol::wl_compositor::WlCompositor, _, _>(1, 4, &qh, ());
    app.wayland.compositor = Some(comp);
    let xdg = registry.bind::<wayland_protocols::xdg::shell::client::xdg_wm_base::XdgWmBase, _, _>(
        2,
        1,
        &qh,
        (),
    );
    app.wayland.xdg_wm_base = Some(xdg);

    // Initializing window creates surface and transitions to Initializing
    app.wayland.init_window(&qh);
    assert!(app.wayland.surface.is_some());
    assert!(app.wayland.xdg_surface.is_some());
    assert!(app.wayland.xdg_toplevel.is_some());
    assert_eq!(
        app.wayland.window_state,
        crate::wayland::WindowState::Initializing
    );

    // Flushing connection succeeds without error
    assert!(conn.flush().is_ok());
    drop(server);
}

#[test]
fn test_pty_registration_guarded_by_wayland_configured() {
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app = AppState::new(term, pty).expect("app");

    // 1. Unmapped state: must reject registration
    assert!(!app.wayland.configured);
    assert!(!app.should_register_pty());

    // 2. Initializing state: still unconfigured, must reject registration
    assert!(
        app.wayland
            .transition_window_to(crate::wayland::WindowState::Initializing)
            .is_ok()
    );
    assert!(!app.should_register_pty());

    // 3. Configured state: must accept registration
    assert!(
        app.wayland
            .transition_window_to(crate::wayland::WindowState::Configured)
            .is_ok()
    );
    assert!(app.wayland.configured);
    assert!(app.should_register_pty());

    // 4. Once registered: must not register a second time
    app.pty_registered = true;
    assert!(!app.should_register_pty());
}

#[test]
fn test_configure_renderer_requires_window_surface() {
    let (client, server) = std::os::unix::net::UnixStream::pair().expect("socketpair");
    let conn = wayland_client::Connection::from_socket(client).expect("conn");
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app = AppState::new(term, pty).expect("app");

    // Without a window surface, configure_renderer must fail with WindowNotCreated
    assert!(app.wayland.surface.is_none());
    assert!(matches!(
        app.configure_renderer(&conn),
        Err(crate::error::FttyError::Wayland(
            crate::error::WaylandError::WindowNotCreated
        ))
    ));
    drop(server);
}

#[test]
fn test_plaintext_url_hover_with_ctrl() {
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app = AppState::new(term, pty).expect("AppState new");

    let url = "https://github.com/xifan2333/ftty";
    for (i, c) in format!("Open {url} now!").chars().enumerate() {
        app.terminal.grid.lines[0].cells[i].c = c;
    }

    let cw = f64::from(app.font_mgr.metrics.cell_width);
    let ch = f64::from(app.font_mgr.metrics.cell_height);

    app.pointer_in_surface = true;
    // Over the URL (col 10)
    app.mouse_pos = [cw * 10.5, ch * 0.5];

    // Without Ctrl: no hover span
    app.update_hover_state();
    assert_eq!(app.hovered_span, None);

    // With Ctrl held: hover span detected!
    app.keyboard.update_modifiers(4, 0, 0, 0);
    assert!(app.keyboard.modifiers().ctrl);
    app.update_hover_state();
    assert_eq!(
        app.hovered_span,
        Some(HoveredHyperlinkSpan {
            line: 0,
            start_col: 5,
            end_col: 37,
        })
    );

    // Release Ctrl: hover span cleared
    app.keyboard.update_modifiers(0, 0, 0, 0);
    assert!(!app.keyboard.modifiers().ctrl);
    app.update_hover_state();
    assert_eq!(app.hovered_span, None);
}
