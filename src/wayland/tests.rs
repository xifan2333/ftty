use std::io::Write;
use std::os::unix::net::UnixStream;

use wayland_client::protocol::wl_data_device_manager::WlDataDeviceManager;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Proxy};

use crate::event_loop::AppState;
use crate::input::mouse::{MouseEncoding, MouseTracking};
use crate::input::selection::SelectionPoint;
use crate::parser::Terminal;
use crate::pty::Pty;
use crate::wayland::{WaylandState, WindowState};

#[test]
fn test_wayland_state_initialization() {
    let state = WaylandState::new();
    assert_eq!(state.width, 720);
    assert_eq!(state.height, 480);
    assert!(!state.configured);
    assert_eq!(state.window_state, WindowState::Unmapped);
    assert!(!state.close_requested);
    assert!(state.surface.is_none());
}

#[test]
fn test_window_state_lifecycle_transitions() {
    let mut state = WaylandState::new();
    assert_eq!(state.window_state, WindowState::Unmapped);
    assert!(!state.configured);

    // Valid progression: Unmapped -> Initializing -> Configured -> Active
    assert!(
        state
            .transition_window_to(WindowState::Initializing)
            .is_ok()
    );
    assert_eq!(state.window_state, WindowState::Initializing);
    assert!(!state.configured);

    assert!(state.transition_window_to(WindowState::Configured).is_ok());
    assert_eq!(state.window_state, WindowState::Configured);
    assert!(state.configured);

    assert!(state.transition_window_to(WindowState::Active).is_ok());
    assert_eq!(state.window_state, WindowState::Active);
    assert!(state.configured);

    // Re-configure during active state
    assert!(state.transition_window_to(WindowState::Configured).is_ok());
    assert_eq!(state.window_state, WindowState::Configured);
    assert!(state.configured);

    assert!(state.transition_window_to(WindowState::Active).is_ok());

    // Close
    assert!(state.transition_window_to(WindowState::Closed).is_ok());
    assert_eq!(state.window_state, WindowState::Closed);
    assert!(!state.configured);
}

#[test]
fn test_invalid_window_state_transitions_rejected() {
    let mut state = WaylandState::new();

    // Cannot jump Unmapped -> Active
    let err = state.transition_window_to(WindowState::Active).unwrap_err();
    assert!(
        err.to_string()
            .contains("invalid window state transition from Unmapped to Active")
    );

    // Cannot jump Unmapped -> Configured
    assert!(state.transition_window_to(WindowState::Configured).is_err());

    // Initializing -> Active without configure must be rejected
    assert!(
        state
            .transition_window_to(WindowState::Initializing)
            .is_ok()
    );
    assert!(state.transition_window_to(WindowState::Active).is_err());

    // Closed cannot transition back to Initializing
    assert!(state.transition_window_to(WindowState::Closed).is_ok());
    assert!(
        state
            .transition_window_to(WindowState::Initializing)
            .is_err()
    );
}

#[test]
fn test_window_state_as_str() {
    assert_eq!(WindowState::Unmapped.as_str(), "Unmapped");
    assert_eq!(WindowState::Initializing.as_str(), "Initializing");
    assert_eq!(WindowState::Configured.as_str(), "Configured");
    assert_eq!(WindowState::Active.as_str(), "Active");
    assert_eq!(WindowState::Closed.as_str(), "Closed");
}

#[test]
fn test_xdg_toplevel_configure_sizing_behavior() {
    use wayland_protocols::xdg::shell::client::xdg_toplevel::{Event, XdgToplevel};

    let (client, server) = std::os::unix::net::UnixStream::pair().expect("socketpair");
    let conn = Connection::from_socket(client).expect("conn");
    let queue = conn.new_event_queue::<AppState>();
    let qh = queue.handle();
    let registry = conn.display().get_registry(&qh, ());
    let term = Terminal::new(80, 24, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), 80, 24).expect("PTY spawn");
    let mut app = AppState::new(term, pty).expect("app");

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
    app.wayland.init_window(&qh);
    let toplevel = app.wayland.xdg_toplevel.clone().expect("toplevel");

    // 1. Positive configure event dispatched through production handler schedules 1351x735
    <AppState as wayland_client::Dispatch<XdgToplevel, ()>>::event(
        &mut app,
        &toplevel,
        Event::Configure {
            width: 1351,
            height: 735,
            states: Vec::new(),
        },
        &(),
        &conn,
        &qh,
    );
    assert_eq!(app.pending_size, Some([1351, 735]));

    // 2. Zero-sized configure event supersedes queued size and preserves active dimensions
    <AppState as wayland_client::Dispatch<XdgToplevel, ()>>::event(
        &mut app,
        &toplevel,
        Event::Configure {
            width: 0,
            height: 0,
            states: Vec::new(),
        },
        &(),
        &conn,
        &qh,
    );
    assert_eq!(
        app.pending_size,
        Some([app.wayland.width, app.wayland.height])
    );
    drop(server);
}

#[test]
fn clipboard_offer_is_created_and_dispatched_from_the_wire() {
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
fn pointer_events_from_the_wire_drive_mouse_reports() {
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
fn test_find_url_at_col() {
    use crate::grid::Row;
    use crate::wayland::seat::find_url_at_col;

    let mut row = Row::new(50);
    let s = "Check https://github.com/xifan2333/ftty. Great!";
    for (i, c) in s.chars().enumerate() {
        row.cells[i].c = c;
    }

    // Col 5 is on " " before URL -> None
    assert_eq!(find_url_at_col(&row, 5), None);

    // Col 10 is on "https://..." -> Some
    let url_opt = find_url_at_col(&row, 10);
    assert!(url_opt.is_some());
    let (start, end, url) = url_opt.unwrap();
    assert_eq!(url, "https://github.com/xifan2333/ftty");
    assert_eq!(start, 6);
    assert_eq!(end, 38);

    // Col 45 is on "Great!" -> None
    assert_eq!(find_url_at_col(&row, 45), None);
}

#[test]
fn test_find_url_in_grid_wrapped_lines() {
    use crate::grid::Grid;
    use crate::wayland::seat::find_url_in_grid;

    let mut grid = Grid::new(20, 2, 0);
    for (i, c) in "https://example.com/".chars().enumerate() {
        grid.lines[0].cells[i].c = c;
    }
    grid.lines[0].wrapped = true;
    for (i, c) in "repo/page and more".chars().enumerate() {
        grid.lines[1].cells[i].c = c;
    }

    let hit0 = find_url_in_grid(&grid, 0, 0, 5);
    assert!(hit0.is_some());
    let (_, _, url0) = hit0.unwrap();
    assert_eq!(url0, "https://example.com/repo/page");

    let hit1 = find_url_in_grid(&grid, 1, 1, 2);
    assert!(hit1.is_some());
    let (_, _, url1) = hit1.unwrap();
    assert_eq!(url1, "https://example.com/repo/page");
}

#[test]
fn test_find_url_hidden_and_wide_char() {
    use crate::grid::{CellFlags, Grid};
    use crate::wayland::seat::find_url_in_grid;

    let mut grid = Grid::new(30, 1, 0);
    let prefix = "https://example.com/";
    for (i, c) in prefix.chars().enumerate() {
        grid.lines[0].cells[i].c = c;
    }
    grid.lines[0].cells[20].c = '文';
    grid.lines[0].cells[20].flags = CellFlags::WIDE_CHAR;
    grid.lines[0].cells[21].c = ' ';
    grid.lines[0].cells[21].flags = CellFlags::WIDE_CHAR_SPACER;

    let hit = find_url_in_grid(&grid, 0, 0, 5);
    assert!(hit.is_some());
    let (_, _, url) = hit.unwrap();
    assert_eq!(url, "https://example.com/文");

    let mut grid_hidden = Grid::new(30, 1, 0);
    for (i, c) in "https://secret.com/page".chars().enumerate() {
        grid_hidden.lines[0].cells[i].c = c;
        if (8..=13).contains(&i) {
            grid_hidden.lines[0].cells[i].flags = CellFlags::HIDDEN;
        }
    }
    assert_eq!(find_url_in_grid(&grid_hidden, 0, 0, 2), None);
}
