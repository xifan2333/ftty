#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Read;
use std::time::Duration;

use ftty::parser::Terminal;
use ftty::pty::Pty;
use ftty::session::TerminalSession;

fn create_test_session(cols: u16, rows: u16) -> TerminalSession {
    let term = Terminal::new(cols as usize, rows as usize, 100);
    let pty = Pty::spawn(Some(&["/bin/sh"]), cols, rows).expect("spawn test shell");
    TerminalSession::new(term, pty)
}

#[test]
fn test_session_initialization_and_accessors() {
    let mut session = create_test_session(80, 24);
    assert_eq!(session.terminal().grid.cols, 80);
    assert_eq!(session.terminal().grid.rows, 24);
    assert_eq!(session.terminal_mut().grid.max_scrollback, 100);
    assert!(session.pty().is_alive());
}

#[test]
fn test_session_write_input_bytes_and_pty_roundtrip() {
    let mut session = create_test_session(80, 24);

    session.write_input_bytes(b"echo ftty_session_ok\n");

    let mut found = false;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(20));
        let mut buf = [0u8; 1024];
        if let Ok(n) = session.pty_mut().read(&mut buf)
            && n > 0
        {
            let outcome = session.process_incoming_bytes(&buf[..n], 10.0, 20.0, true);
            assert!(outcome.needs_redraw);

            let text: String = session
                .terminal()
                .grid
                .lines
                .iter()
                .flat_map(|r| r.cells.iter().map(|c| c.c))
                .collect();
            if text.contains("ftty_session_ok") {
                found = true;
                break;
            }
        }
    }
    assert!(found, "Expected shell echo inside terminal session");
}

#[test]
fn test_session_title_update_via_osc() {
    let mut session = create_test_session(80, 24);

    let outcome =
        session.process_incoming_bytes(b"\x1b]0;session_window_title\x07", 10.0, 20.0, false);
    assert_eq!(
        outcome.title_update,
        Some("session_window_title".to_string())
    );
}

#[test]
fn test_session_palette_update_via_osc() {
    let mut session = create_test_session(80, 24);

    // Set palette color 1 to red via OSC 4
    let outcome = session.process_incoming_bytes(b"\x1b]4;1;rgb:ff/00/00\x07", 10.0, 20.0, false);
    assert!(outcome.palette_changed);
    assert!(outcome.needs_redraw);
}

#[test]
fn test_session_osc52_clipboard_integration() {
    let mut session = create_test_session(80, 24);
    session.terminal_mut().allow_osc52_write = true;

    // "hello" in base64 is "aGVsbG8="
    let outcome = session.process_incoming_bytes(b"\x1b]52;c;aGVsbG8=\x07", 10.0, 20.0, false);
    assert_eq!(outcome.clipboard_update, Some(Some("hello".to_string())));
}

#[test]
fn test_session_mode_2026_synchronized_output_timeout() {
    let mut session = create_test_session(80, 24);

    // Initially inactive
    assert!(!session.is_synchronized_output_active());

    // Enter Mode 2026
    let _ = session.process_incoming_bytes(b"\x1b[?2026h", 10.0, 20.0, false);
    assert!(session.terminal().synchronized_output);
    assert!(session.is_synchronized_output_active());

    // Wait past the 150ms timeout
    std::thread::sleep(Duration::from_millis(160));

    // Must automatically time out and disable synchronized output
    assert!(!session.is_synchronized_output_active());
    assert!(!session.terminal().synchronized_output);
}

#[test]
fn test_session_atomic_resize() {
    let mut session = create_test_session(80, 24);

    assert_eq!(session.terminal().grid.cols, 80);
    assert_eq!(session.terminal().grid.rows, 24);

    session
        .resize(100, 30, [10, 20], [1000, 600])
        .expect("session resize");

    assert_eq!(session.terminal().grid.cols, 100);
    assert_eq!(session.terminal().grid.rows, 30);
    assert_eq!(session.terminal().cell_pixels, [10, 20]);
    assert_eq!(session.terminal().viewport_pixels, [1000, 600]);
}

#[test]
fn test_session_kitty_stream_placement_and_cursor_advance() {
    use base64::Engine;
    use base64::prelude::BASE64_STANDARD;

    let mut session = create_test_session(80, 24);

    let raw = [0u8, 0, 255, 255];
    let b64 = BASE64_STANDARD.encode(raw);
    let payload = format!("\x1b_Ga=T,f=32,s=1,v=1,i=77;{b64}\x1b\\");
    let outcome = session.process_incoming_bytes(payload.as_bytes(), 10.0, 20.0, false);

    assert!(outcome.needs_redraw);
    assert_eq!(session.terminal().grid.graphics.placements().len(), 1);
}

#[test]
fn test_session_read_and_process_pty_title_aggregation() {
    let mut session = create_test_session(80, 24);
    session.write_input_bytes(b"printf '\\033]0;aggregated_title\\007'\n");

    let mut found_title = false;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(20));
        if let Ok(outcome) = session.read_and_process_pty(10.0, 20.0, false)
            && outcome.title_update == Some("aggregated_title".to_string())
        {
            found_title = true;
            break;
        }
    }
    assert!(
        found_title,
        "read_and_process_pty must aggregate title notifications"
    );
}
