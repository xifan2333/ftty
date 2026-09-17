use std::io::{Read, Write};
use std::time::Duration;

use wayland_client::Connection;
use wayland_client::protocol::wl_data_device_manager::WlDataDeviceManager;
use wayland_client::protocol::wl_seat::WlSeat;

use crate::color::Rgb;
use crate::event_loop::{AppState, terminal_size};
use crate::font::CellMetrics;
use crate::input::KeyAction;
use crate::input::selection::{Selection, SelectionPoint, SelectionType};
use crate::parser::Terminal;
use crate::pty::Pty;
use crate::render::HoveredHyperlinkSpan;

#[test]
fn clipboard_offer_is_created_and_dispatched_from_the_wire() {
    use std::os::unix::net::UnixStream;
    use wayland_client::Proxy;

    let (client, mut server) = UnixStream::pair().unwrap();
    let conn = Connection::from_socket(client).unwrap();
    let mut queue = conn.new_event_queue::<AppState>();
    let qh = queue.handle();
    let registry = conn.display().get_registry(&qh, ());
    let manager = registry.bind::<WlDataDeviceManager, _, _>(1, 3, &qh, ());
    let seat = registry.bind::<WlSeat, _, _>(2, 5, &qh, ());
    let device = manager.get_data_device(&seat, &qh, ());

    // Encode the startup clipboard sequence from a compositor without requiring
    // a desktop session in CI: data_offer(new_id), offer(MIME), selection(id).
    let offer_id = 0xff00_0000u32;
    let device_id = device.id().protocol_id();
    let mime = b"text/plain;charset=utf-8\0";
    let mut events = Vec::new();
    for word in [device_id, 12 << 16, offer_id] {
        events.extend_from_slice(&word.to_ne_bytes());
    }
    let padded_len = mime.len().next_multiple_of(4);
    for word in [
        offer_id,
        ((12 + padded_len) as u32) << 16,
        mime.len() as u32,
    ] {
        events.extend_from_slice(&word.to_ne_bytes());
    }
    events.extend_from_slice(mime);
    events.resize(events.len() + padded_len - mime.len(), 0);
    for word in [device_id, (12 << 16) | 5, offer_id] {
        events.extend_from_slice(&word.to_ne_bytes());
    }
    server.write_all(&events).unwrap();

    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).unwrap();
    let mut app = AppState::new(term, pty).unwrap();
    conn.prepare_read().unwrap().read().unwrap();
    queue.dispatch_pending(&mut app).unwrap();

    let offer = app.wayland.current_offer.as_ref().unwrap();
    assert_eq!(offer.offer.id().protocol_id(), offer_id);
    assert_eq!(offer.mime_types, ["text/plain;charset=utf-8"]);
    assert!(app.pending_offers.is_empty());
}

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
            app.terminal.advance_bytes(&buf[..n]);
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
        padding_x = 20
        padding_y = 20

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
fn pointer_events_from_the_wire_drive_mouse_reports() {
    use std::os::unix::net::UnixStream;
    use wayland_client::Proxy;

    use crate::input::mouse::{MouseEncoding, MouseTracking};

    let (client, mut server) = UnixStream::pair().unwrap();
    let conn = Connection::from_socket(client).unwrap();
    let mut queue = conn.new_event_queue::<AppState>();
    let qh = queue.handle();
    let registry = conn.display().get_registry(&qh, ());
    let seat = registry.bind::<WlSeat, _, _>(1, 5, &qh, ());
    let mut events = Vec::new();
    for word in [seat.id().protocol_id(), 12 << 16, 1] {
        events.extend_from_slice(&word.to_ne_bytes());
    }
    server.write_all(&events).unwrap();

    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).unwrap();
    let mut app = AppState::new(term, pty).unwrap();
    app.terminal.mouse.tracking = MouseTracking::Drag;
    app.terminal.mouse.encoding = MouseEncoding::Sgr;
    conn.prepare_read().unwrap().read().unwrap();
    queue.dispatch_pending(&mut app).unwrap();

    let pointer = app.wayland.pointer.clone().expect("wl_pointer bound");
    let pointer_id = pointer.id().protocol_id();
    let cw = app.font_mgr.metrics.cell_width as f64;
    let ch = app.font_mgr.metrics.cell_height as f64;
    let x = ((cw * 2.5) * 256.0) as i32 as u32;
    let y = ((ch * 1.5) * 256.0) as i32 as u32;

    // wl_pointer.motion(time, x, y) followed by wl_pointer.button(serial, time, BTN_LEFT, pressed).
    let mut events = Vec::new();
    for word in [pointer_id, (20 << 16) | 2, 7, x, y] {
        events.extend_from_slice(&word.to_ne_bytes());
    }
    for word in [pointer_id, (24 << 16) | 3, 9, 9, 0x110, 1] {
        events.extend_from_slice(&word.to_ne_bytes());
    }
    server.write_all(&events).unwrap();
    conn.prepare_read().unwrap().read().unwrap();
    queue.dispatch_pending(&mut app).unwrap();

    assert!(app.mouse_reported, "left press was not forwarded");
    assert_eq!(app.mouse_button, 0);
    assert!(
        !app.mouse_pressed,
        "local selection must stay idle while reporting"
    );
    assert_eq!(app.last_serial, 9);

    // Turning tracking off hands the very same press back to local selection.
    app.terminal.mouse.tracking = MouseTracking::Disabled;
    app.mouse_reported = false;
    let mut events = Vec::new();
    for word in [pointer_id, (24 << 16) | 3, 10, 10, 0x110, 1] {
        events.extend_from_slice(&word.to_ne_bytes());
    }
    server.write_all(&events).unwrap();
    conn.prepare_read().unwrap().read().unwrap();
    queue.dispatch_pending(&mut app).unwrap();
    assert!(app.mouse_pressed);
    assert!(!app.mouse_reported);
    assert_eq!(app.selection.start, SelectionPoint::new(1, 2));
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
        app.terminal.grid.lines[0].cells[col].hyperlink_id = Some(1);
    }
    // Cell at col 8 has hyperlink ID 0 (invalid/exhausted sentinel)
    app.terminal.grid.lines[0].cells[8].hyperlink_id = Some(0);

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
fn test_stashed_floating_size_initialization() {
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).unwrap();
    let app = AppState::new(term, pty).unwrap();

    assert!(app.wayland.stashed_floating_size.is_some());
    let [w, h] = app.wayland.stashed_floating_size.unwrap();
    assert_eq!(w, app.wayland.width);
    assert_eq!(h, app.wayland.height);
    assert!(w >= 720);
    assert!(h >= 400);
}
