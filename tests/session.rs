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

#[test]
fn test_encode_paste_wraps_only_when_bracketed_mode_active() {
    let mut session = create_test_session(80, 24);

    // Bracketed paste is off by default: bytes pass through untouched.
    assert!(!session.terminal().bracketed_paste);
    assert_eq!(session.encode_paste(b"hello"), b"hello".to_vec());

    // Enabling DEC mode 2004 wraps the payload in the bracketed markers.
    let _ = session.process_incoming_bytes(b"\x1b[?2004h", 10.0, 20.0, false);
    assert!(session.terminal().bracketed_paste);
    assert_eq!(
        session.encode_paste(b"hello"),
        b"\x1b[200~hello\x1b[201~".to_vec()
    );

    // Disabling the mode restores pass-through behaviour.
    let _ = session.process_incoming_bytes(b"\x1b[?2004l", 10.0, 20.0, false);
    assert_eq!(session.encode_paste(b"hello"), b"hello".to_vec());
}

#[test]
fn test_session_ime_commit_encodes_erasure_then_text() {
    use ftty::input::ime::encode_ime_edits;

    assert_eq!(encode_ime_edits(None, None), Vec::<u8>::new());
    assert_eq!(encode_ime_edits(Some((0, 0)), None), Vec::<u8>::new());
    // Two characters erased before the cursor emit two DEL bytes, matching the Backspace key.
    assert_eq!(encode_ime_edits(Some((2, 0)), None), b"\x7f\x7f".to_vec());
    // One character erased after the cursor emits a single forward delete.
    assert_eq!(encode_ime_edits(Some((0, 1)), None), b"\x1b[3~".to_vec());
    // Erasure always precedes the committed text.
    assert_eq!(
        encode_ime_edits(Some((1, 1)), Some("\u{4f60}")),
        b"\x7f\x1b[3~\xe4\xbd\xa0".to_vec()
    );
    assert_eq!(encode_ime_edits(None, Some("hi")), b"hi".to_vec());
}

#[test]
fn test_encode_ime_edits_clamps_hostile_delete_lengths() {
    use ftty::input::ime::{MAX_IME_ERASE_CHARS, encode_ime_edits};

    // A hostile compositor must not force an unbounded allocation: output stays bounded.
    let bytes = encode_ime_edits(Some((u32::MAX, u32::MAX)), None);
    let expected = MAX_IME_ERASE_CHARS as usize * (1 + 4);
    assert_eq!(bytes.len(), expected);
}

#[test]
fn test_session_paste_writes_payload_to_pty() {
    let mut session = create_test_session(80, 24);

    // Small payloads stay synchronous on the calling thread.
    session.paste(b"echo ftty_paste_ok\n");

    let mut echoed = String::new();
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(20));
        let _ = session.read_and_process_pty(10.0, 20.0, false);
        echoed = session
            .terminal()
            .grid
            .lines
            .iter()
            .flat_map(|row| row.cells.iter().map(|c| c.c))
            .collect();
        if echoed.contains("ftty_paste_ok") {
            break;
        }
    }
    assert!(
        echoed.contains("ftty_paste_ok"),
        "pasted command should reach the shell"
    );
}

#[test]
fn test_session_paste_from_fd_streams_to_pty() {
    let mut session = create_test_session(80, 24);

    // Simulate a clipboard owner writing into the read end of a pipe.
    let (read_fd, write_fd) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).expect("pipe2");
    nix::unistd::write(&write_fd, b"echo ftty_fd_paste_ok\n").expect("write pipe");
    drop(write_fd);

    session.paste_from_fd(read_fd);

    let mut echoed = String::new();
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(20));
        let _ = session.read_and_process_pty(10.0, 20.0, false);
        echoed = session
            .terminal()
            .grid
            .lines
            .iter()
            .flat_map(|row| row.cells.iter().map(|c| c.c))
            .collect();
        if echoed.contains("ftty_fd_paste_ok") {
            break;
        }
    }
    assert!(
        echoed.contains("ftty_fd_paste_ok"),
        "paste_from_fd should drain the clipboard fd into the shell"
    );
}
