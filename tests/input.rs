#![allow(clippy::unwrap_used, clippy::expect_used)]

use xkbcommon::xkb::{self, KEYMAP_FORMAT_TEXT_V1, Keymap, keysyms};

use ftty::config::KeybindingsConfig;
use ftty::input::{KeyAction, KeyboardHandler, KittyKeyboardFlags, parse_key_combo};

#[test]
fn keymap_fd_loads_the_advertised_bytes_and_strips_the_trailing_nul() {
    use nix::sys::memfd::{MFdFlags, memfd_create};
    use std::io::{Seek, Write};

    let mut handler = KeyboardHandler::new();
    let keymap = Keymap::new_from_names(
        &handler.context,
        "",
        "",
        "de",
        "",
        None,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .unwrap();
    let mut bytes = keymap.get_as_string(KEYMAP_FORMAT_TEXT_V1).into_bytes();
    bytes.push(0);
    let size = bytes.len();
    bytes.extend_from_slice(b"ignored bytes after the advertised keymap");

    let fd = memfd_create(c"ftty-keymap-test", MFdFlags::MFD_CLOEXEC).unwrap();
    let mut file = std::fs::File::from(fd);
    file.write_all(&bytes).unwrap();
    file.rewind().unwrap();
    handler.set_keymap_from_fd(file.into(), size);

    // The German layout maps the physical Y key to Z.
    assert_eq!(handler.handle_key(21), Some(b"z".to_vec()));
}

#[test]
fn test_set_keymap_from_fd_handles_multiple_trailing_nuls_and_padding() {
    use nix::sys::memfd::{MFdFlags, memfd_create};
    use std::io::{Seek, Write};

    let mut handler = KeyboardHandler::new();
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = Keymap::new_from_names(
        &context,
        "",
        "",
        "fr",
        "",
        None,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .unwrap();
    let mut bytes = keymap.get_as_string(KEYMAP_FORMAT_TEXT_V1).into_bytes();
    // Simulate compositor page-alignment with multiple trailing zeroes
    bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
    let size = bytes.len();

    let fd = memfd_create(c"ftty-keymap-padding-test", MFdFlags::MFD_CLOEXEC).unwrap();
    let mut file = std::fs::File::from(fd);
    file.write_all(&bytes).unwrap();
    file.rewind().unwrap();

    // Reset handler state to verify new keymap is loaded and active
    handler.keymap = None;
    handler.state = None;

    handler.set_keymap_from_fd(file.into(), size);

    assert!(handler.keymap.is_some());
    assert!(handler.state.is_some());
    // French AZERTY layout maps physical keycode 16 (Q on US layout) to 'a'
    assert_eq!(handler.handle_key(16), Some(b"a".to_vec()));
}

#[test]
fn test_set_keymap_from_fd_handles_nonzero_seek_offset() {
    use nix::sys::memfd::{MFdFlags, memfd_create};
    use std::io::{Seek, SeekFrom, Write};

    let mut handler = KeyboardHandler::new();
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = Keymap::new_from_names(
        &context,
        "",
        "",
        "de",
        "",
        None,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .unwrap();
    let bytes = keymap.get_as_string(KEYMAP_FORMAT_TEXT_V1).into_bytes();
    let size = bytes.len();

    let fd = memfd_create(c"ftty-keymap-seek-test", MFdFlags::MFD_CLOEXEC).unwrap();
    let mut file = std::fs::File::from(fd);
    file.write_all(&bytes).unwrap();
    // Intentionally leave seek position at EOF (simulating un-rewound compositor memfd)
    file.seek(SeekFrom::End(0)).unwrap();

    handler.keymap = None;
    handler.state = None;

    handler.set_keymap_from_fd(file.into(), size);

    assert!(handler.keymap.is_some());
    assert!(handler.state.is_some());
    // German QWERTZ layout maps physical keycode 21 (Y on US layout) to 'z'
    assert_eq!(handler.handle_key(21), Some(b"z".to_vec()));
}

#[test]
fn test_set_keymap_from_fd_oversized_advertised_size_does_not_abort() {
    use nix::sys::memfd::{MFdFlags, memfd_create};
    use std::io::Write;

    let mut handler = KeyboardHandler::new();
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = Keymap::new_from_names(
        &context,
        "",
        "",
        "de",
        "",
        None,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .unwrap();
    let bytes = keymap.get_as_string(KEYMAP_FORMAT_TEXT_V1).into_bytes();

    let fd = memfd_create(c"ftty-keymap-oversized-test", MFdFlags::MFD_CLOEXEC).unwrap();
    let mut file = std::fs::File::from(fd);
    file.write_all(&bytes).unwrap();

    handler.keymap = None;
    handler.state = None;

    // Pass a huge advertised size (e.g. 500MB) with a small real descriptor
    handler.set_keymap_from_fd(file.into(), 500 * 1024 * 1024);

    assert!(handler.keymap.is_some());
    assert!(handler.state.is_some());
    assert_eq!(handler.handle_key(21), Some(b"z".to_vec()));
}

#[test]
fn test_keyboard_handler_initialization() {
    let handler = KeyboardHandler::new();
    assert!(handler.keymap.is_some());
    assert!(handler.state.is_some());
}

#[test]
fn test_keyboard_keymap_and_translation() {
    let mut handler = KeyboardHandler::new();

    // Test Enter key (evdev code 28)
    let enter = handler.handle_key(28);
    assert_eq!(enter, Some(b"\r".to_vec()));

    // Test Backspace key (evdev code 14)
    let bs = handler.handle_key(14);
    assert_eq!(bs, Some(b"\x7f".to_vec()));

    // Test Tab key (evdev code 15)
    let tab = handler.handle_key(15);
    assert_eq!(tab, Some(b"\t".to_vec()));

    // Test Escape key (evdev code 1)
    let esc = handler.handle_key(1);
    assert_eq!(esc, Some(b"\x1b".to_vec()));

    // Test Arrow Up (evdev code 103)
    let up = handler.handle_key(103);
    assert_eq!(up, Some(b"\x1b[A".to_vec()));

    // Test Arrow Down (evdev code 108)
    let down = handler.handle_key(108);
    assert_eq!(down, Some(b"\x1b[B".to_vec()));
}

#[test]
fn test_parse_key_combo() {
    let (mods, sym) = parse_key_combo("Shift+PageUp").unwrap();
    assert!(mods.shift);
    assert!(!mods.ctrl);
    assert_eq!(sym, xkb::Keysym::new(keysyms::KEY_Page_Up));

    let (mods, sym) = parse_key_combo("Ctrl+Shift+c").unwrap();
    assert!(mods.ctrl);
    assert!(mods.shift);
    assert_eq!(sym, xkb::Keysym::new(keysyms::KEY_c));

    let (mods, sym) = parse_key_combo("Control+Plus").unwrap();
    assert!(mods.ctrl);
    assert_eq!(sym, xkb::Keysym::new(keysyms::KEY_plus));

    assert!(parse_key_combo("none").is_none());
    assert!(parse_key_combo("").is_none());
}

#[test]
fn test_check_action_default_bindings() {
    let mut handler = KeyboardHandler::new();
    let config = KeybindingsConfig::default();

    // With no modifiers active, PageUp (evdev 104) is NOT an action
    assert!(handler.check_action(104, &config).is_none());

    // Shift active (mask 1): PageUp (evdev 104) triggers ScrollbackUpPage
    handler.update_modifiers(1, 0, 0, 0);
    assert_eq!(
        handler.check_action(104, &config),
        Some(KeyAction::ScrollbackUpPage)
    );
    // Shift active: PageDown (evdev 109) triggers ScrollbackDownPage
    assert_eq!(
        handler.check_action(109, &config),
        Some(KeyAction::ScrollbackDownPage)
    );

    // Ctrl + Shift active (mask 4 | 1 = 5):
    handler.update_modifiers(5, 0, 0, 0);
    // 'C' key (evdev 46) triggers ClipboardCopy
    assert_eq!(
        handler.check_action(46, &config),
        Some(KeyAction::ClipboardCopy)
    );
    // 'V' key (evdev 47) triggers ClipboardPaste
    assert_eq!(
        handler.check_action(47, &config),
        Some(KeyAction::ClipboardPaste)
    );

    // Ctrl active (mask 4):
    handler.update_modifiers(4, 0, 0, 0);
    // '=' key (evdev 13) triggers FontIncrease
    assert_eq!(
        handler.check_action(13, &config),
        Some(KeyAction::FontIncrease)
    );
    // '-' key (evdev 12) triggers FontDecrease
    assert_eq!(
        handler.check_action(12, &config),
        Some(KeyAction::FontDecrease)
    );
    // '0' key (evdev 11) triggers FontReset
    assert_eq!(
        handler.check_action(11, &config),
        Some(KeyAction::FontReset)
    );

    // User override: disabling clipboard_paste with "none" keeps primary_paste (Shift+Insert)
    let custom_paste_config = KeybindingsConfig {
        bindings: std::collections::HashMap::from([(
            "Ctrl+Shift+V".to_string(),
            ftty::config::ActionDef::Simple("none".to_string()),
        )]),
    };
    handler.update_modifiers(5, 0, 0, 0);
    assert_eq!(handler.check_action(47, &custom_paste_config), None);
    // Shift active: Insert key (evdev 110) still triggers PrimaryPaste
    handler.update_modifiers(1, 0, 0, 0);
    assert_eq!(
        handler.check_action(110, &custom_paste_config),
        Some(KeyAction::PrimaryPaste)
    );

    // User override: disabling primary_paste with "none" disables Shift+Insert
    let custom_primary_config = KeybindingsConfig {
        bindings: std::collections::HashMap::from([(
            "Shift+Insert".to_string(),
            ftty::config::ActionDef::Simple("none".to_string()),
        )]),
    };
    assert_eq!(handler.check_action(110, &custom_primary_config), None);

    // User override: disabling clipboard_copy with "none"
    let custom_config = KeybindingsConfig {
        bindings: std::collections::HashMap::from([(
            "Ctrl+Shift+C".to_string(),
            ftty::config::ActionDef::Simple("none".to_string()),
        )]),
    };
    handler.update_modifiers(5, 0, 0, 0);
    assert_eq!(handler.check_action(46, &custom_config), None);

    // Custom pipe action
    let custom_pipe_config = KeybindingsConfig {
        bindings: std::collections::HashMap::from([(
            "Ctrl+Shift+U".to_string(),
            ftty::config::ActionDef::Pipe(ftty::config::PipeActionDef::PipeVisible(
                ftty::config::CommandDef::List(vec!["urlscan".to_string()]),
            )),
        )]),
    };
    handler.update_modifiers(5, 0, 0, 0);
    assert_eq!(
        handler.check_action(22, &custom_pipe_config),
        Some(KeyAction::PipeVisible(vec!["urlscan".to_string()]))
    );
}

#[test]
fn test_kitty_keyboard_encoding() {
    let mut handler = KeyboardHandler::new();

    // Default flags = 0: ordinary VT output
    let enter = handler.handle_key_event(28, true, false);
    assert_eq!(enter, Some(b"\r".to_vec()));

    // Release event ignored when REPORT_EVENT_TYPES is off
    let release = handler.handle_key_event(28, false, false);
    assert_eq!(release, None);

    // Enable DISAMBIGUATE (1)
    handler.set_kitty_mode(KittyKeyboardFlags::DISAMBIGUATE, 1);
    let disambiguated_enter = handler.handle_key_event(28, true, false);
    assert_eq!(disambiguated_enter, Some(b"\x1b[13u".to_vec()));

    // Enable REPORT_EVENT_TYPES (2) via union (mode 2)
    handler.set_kitty_mode(KittyKeyboardFlags::REPORT_EVENT_TYPES, 2);
    assert_eq!(
        handler.kitty_flags,
        KittyKeyboardFlags::DISAMBIGUATE | KittyKeyboardFlags::REPORT_EVENT_TYPES
    );

    let press_enter = handler.handle_key_event(28, true, false);
    assert_eq!(press_enter, Some(b"\x1b[13u".to_vec()));

    let release_enter = handler.handle_key_event(28, false, false);
    assert_eq!(release_enter, Some(b"\x1b[13;1:3u".to_vec()));

    // Push and Pop stack
    handler.push_kitty_flags(0);
    assert_eq!(handler.kitty_flags, 0);
    handler.pop_kitty_flags(1);
    assert_eq!(
        handler.kitty_flags,
        KittyKeyboardFlags::DISAMBIGUATE | KittyKeyboardFlags::REPORT_EVENT_TYPES
    );

    // Popping empty stack resets to 0
    handler.pop_kitty_flags(5);
    assert_eq!(handler.kitty_flags, 0);
}

#[test]
fn test_kitty_arrow_and_functional_keys_encoding() {
    let mut handler = KeyboardHandler::new();
    handler.set_kitty_mode(KittyKeyboardFlags::DISAMBIGUATE, 1);

    // Arrow keys: Up (103), Down (108), Left (105), Right (106)
    assert_eq!(
        handler.handle_key_event(103, true, false),
        Some(b"\x1b[A".to_vec())
    );
    assert_eq!(
        handler.handle_key_event(108, true, false),
        Some(b"\x1b[B".to_vec())
    );
    assert_eq!(
        handler.handle_key_event(106, true, false),
        Some(b"\x1b[C".to_vec())
    );
    assert_eq!(
        handler.handle_key_event(105, true, false),
        Some(b"\x1b[D".to_vec())
    );

    // Modified arrow keys: Shift+Up (1;2A), Ctrl+Down (1;5B)
    handler.update_modifiers(1, 0, 0, 0); // Shift (1 << 0)
    assert_eq!(
        handler.handle_key_event(103, true, false),
        Some(b"\x1b[1;2A".to_vec())
    );

    handler.update_modifiers(4, 0, 0, 0); // Ctrl (1 << 2)
    assert_eq!(
        handler.handle_key_event(108, true, false),
        Some(b"\x1b[1;5B".to_vec())
    );
    handler.update_modifiers(0, 0, 0, 0);

    // Functional tilde and letter keys: Home (102), End (107), PageUp (104), PageDown (109), Delete (111)
    assert_eq!(
        handler.handle_key_event(102, true, false),
        Some(b"\x1b[H".to_vec())
    );
    assert_eq!(
        handler.handle_key_event(107, true, false),
        Some(b"\x1b[F".to_vec())
    );
    assert_eq!(
        handler.handle_key_event(104, true, false),
        Some(b"\x1b[5~".to_vec())
    );
    assert_eq!(
        handler.handle_key_event(109, true, false),
        Some(b"\x1b[6~".to_vec())
    );
    assert_eq!(
        handler.handle_key_event(111, true, false),
        Some(b"\x1b[3~".to_vec())
    );

    // Release events with REPORT_EVENT_TYPES: Up release = \x1b[1;1:3A
    handler.set_kitty_mode(KittyKeyboardFlags::REPORT_EVENT_TYPES, 2);
    assert_eq!(
        handler.handle_key_event(103, false, false),
        Some(b"\x1b[1;1:3A".to_vec())
    );

    // Higher function keys F13 (57376) and F35 (57398) boundaries
    let f13_bytes = handler.encode_kitty_key(
        xkbcommon::xkb::keysyms::KEY_F13,
        xkbcommon::xkb::Keycode::new(0),
        ftty::input::Modifiers::default(),
        1,
    );
    assert_eq!(f13_bytes, Some(b"\x1b[57376u".to_vec()));

    let f35_bytes = handler.encode_kitty_key(
        xkbcommon::xkb::keysyms::KEY_F35,
        xkbcommon::xkb::Keycode::new(0),
        ftty::input::Modifiers::default(),
        1,
    );
    assert_eq!(f35_bytes, Some(b"\x1b[57398u".to_vec()));
}

#[test]
fn test_kitty_full_flag_progressive_encoding() {
    let mut h = KeyboardHandler::new();

    // ---- flags == 0: fully legacy, never CSI-encoded ----
    // Plain 'a' (evdev 30) press -> raw byte 0x61
    assert_eq!(h.handle_key_event(30, true, false), Some(b"a".to_vec()));
    // Ctrl+a press -> legacy 0x01
    assert_eq!(h.handle_key_event(30, true, false), Some(b"a".to_vec()));
    // Up (103) press -> legacy \x1b[A
    assert_eq!(
        h.handle_key_event(103, true, false),
        Some(b"\x1b[A".to_vec())
    );
    // Release ignored when no flag set
    assert_eq!(h.handle_key_event(30, false, false), None);

    // ---- REPORT_ALL_KEYS_AS_ESC (8) alone: every key CSI-encoded ----
    h.set_kitty_mode(KittyKeyboardFlags::REPORT_ALL_KEYS_AS_ESC, 1);
    // Plain 'a' -> \x1b[97u (crossterm parse_csi_u -> Char('a'))
    assert_eq!(
        h.handle_key_event(30, true, false),
        Some(b"\x1b[97u".to_vec())
    );
    // Up still CSI form
    assert_eq!(
        h.handle_key_event(103, true, false),
        Some(b"\x1b[A".to_vec())
    );
    // No REPORT_EVENT_TYPES -> release ignored
    assert_eq!(h.handle_key_event(30, false, false), None);

    // ---- flags 11 (1|2|8): all keys + event types ----
    h.set_kitty_mode(
        KittyKeyboardFlags::DISAMBIGUATE | KittyKeyboardFlags::REPORT_EVENT_TYPES,
        2,
    );
    assert_eq!(
        h.kitty_flags & KittyKeyboardFlags::REPORT_ALL_KEYS_AS_ESC,
        8
    );
    // Ctrl+a (evdev 30, mod 4): \x1b[97;5u  (crossterm -> Char('a'), CONTROL)
    h.update_modifiers(4, 0, 0, 0);
    assert_eq!(
        h.handle_key_event(30, true, false),
        Some(b"\x1b[97;5u".to_vec())
    );
    // Release of Ctrl+a: \x1b[97;5:3u
    assert_eq!(
        h.handle_key_event(30, false, false),
        Some(b"\x1b[97;5:3u".to_vec())
    );
    h.update_modifiers(0, 0, 0, 0);

    // Explicit regression for the herdr prefix root cause: Ctrl+a under the
    // classic flags herdr pushes (DISAMBIGUATE | REPORT_EVENT_TYPES = 3) must
    // round-trip to crossterm's parse_csi_u as Char('a') + CONTROL. The codepoint
    // is 97 ('a'), NOT the control byte 0x01.
    h.set_kitty_mode(0, 1);
    h.set_kitty_mode(
        KittyKeyboardFlags::DISAMBIGUATE | KittyKeyboardFlags::REPORT_EVENT_TYPES,
        1,
    );
    h.update_modifiers(4, 0, 0, 0); // Ctrl
    assert_eq!(
        h.handle_key_event(30, true, false),
        Some(b"\x1b[97;5u".to_vec())
    );
    h.update_modifiers(0, 0, 0, 0);

    // Reset and test flags 15 (1|2|4|8): adds REPORT_ALTERNATE_KEYS ----
    h.set_kitty_mode(0, 1);
    h.set_kitty_mode(
        KittyKeyboardFlags::DISAMBIGUATE
            | KittyKeyboardFlags::REPORT_EVENT_TYPES
            | KittyKeyboardFlags::REPORT_ALTERNATE_KEYS
            | KittyKeyboardFlags::REPORT_ALL_KEYS_AS_ESC,
        1,
    );
    assert_eq!(h.kitty_flags, 15);
    // Plain 'a' press (no shift): \x1b[97u
    assert_eq!(
        h.handle_key_event(30, true, false),
        Some(b"\x1b[97u".to_vec())
    );

    // ---- flags 4 alone (REPORT_ALTERNATE_KEYS) is accepted in state ----
    h.set_kitty_mode(0, 1);
    h.set_kitty_mode(KittyKeyboardFlags::REPORT_ALTERNATE_KEYS, 1);
    assert_eq!(h.kitty_flags, 4);
    // With only flag 4 (no disambiguate/all_keys), plain 'a' falls back to legacy
    assert_eq!(h.handle_key_event(30, true, false), Some(b"a".to_vec()));

    // ---- flags 16 alone (REPORT_ASSOCIATED_TEXT) accepted, no crash ----
    h.set_kitty_mode(0, 1);
    h.set_kitty_mode(KittyKeyboardFlags::REPORT_ASSOCIATED_TEXT, 1);
    assert_eq!(h.kitty_flags, 16);
    // Plain 'a' -> legacy (flag 16 doesn't force encoding)
    assert_eq!(h.handle_key_event(30, true, false), Some(b"a".to_vec()));
}

#[test]
fn test_kitty_report_all_keys_escape_does_not_leak_raw_bytes() {
    // Regression: when herdr pushes flag 8 (REPORT_ALL_KEYS_AS_ESC), ftty must NOT
    // emit raw UTF-8 for plain keys (the cause of uncleanable screen marks).
    let mut h = KeyboardHandler::new();
    h.set_kitty_mode(KittyKeyboardFlags::REPORT_ALL_KEYS_AS_ESC, 1);
    // Evdev codes for 'a'..'e' (30..34): every one must be a CSI u sequence,
    // never a bare ASCII byte.
    for code in 30..=34 {
        let bytes = h
            .handle_key_event(code, true, false)
            .expect("press encoded");
        assert_eq!(
            &bytes[..2],
            b"\x1b[",
            "plain key must be CSI-encoded under flag 8"
        );
        assert!(bytes.last() == Some(&b'u'), "must end with 'u'");
    }
}

mod ime_tests {
    use ftty::font::CellMetrics;
    use ftty::grid::{CellFlags, Grid};
    use ftty::input::ime::*;

    #[test]
    fn test_ime_batch_application_order() {
        let mut ime = ImeState::new();

        // Stage delete, commit, and preedit out of order
        ime.stage_commit(Some("你好".to_string()));
        ime.stage_delete(2, 0);
        ime.stage_preedit(Some("test".to_string()), 0, 4);

        let (delete, commit) = ime.apply_done();
        assert_eq!(delete, Some((2, 0)));
        assert_eq!(commit, Some("你好".to_string()));
        assert_eq!(
            ime.preedit,
            Some(Preedit {
                text: "test".to_string(),
                cursor_begin: 0,
                cursor_end: 4,
            })
        );

        // Subsequent commit without preedit update clears preedit
        ime.stage_commit(Some("世界".to_string()));
        let (_, commit) = ime.apply_done();
        assert_eq!(commit, Some("世界".to_string()));
        assert!(ime.preedit.is_none());
    }

    #[test]
    fn test_calculate_cursor_rect_with_padding() {
        let mut grid = Grid::new(80, 24, 0);
        grid.cursor.row = 5;
        grid.cursor.col = 10;

        let metrics = CellMetrics {
            cell_width: 10,
            cell_height: 20,
            ascent: 15,
        };

        // Without padding
        let (x, y, w, h) = calculate_cursor_rect(&grid, metrics, [0, 0]);
        assert_eq!((x, y, w, h), (100, 100, 10, 20));

        // With padding [15, 25]
        let (x, y, w, h) = calculate_cursor_rect(&grid, metrics, [15, 25]);
        assert_eq!((x, y, w, h), (115, 125, 10, 20));
    }

    #[test]
    fn test_calculate_cursor_rect_wide_char_and_spacer() {
        let mut grid = Grid::new(80, 24, 0);
        grid.cursor.row = 2;
        grid.cursor.col = 4;
        grid.lines[2].cells[4].flags = CellFlags::WIDE_CHAR;
        grid.lines[2].cells[5].flags = CellFlags::WIDE_CHAR_SPACER;

        let metrics = CellMetrics {
            cell_width: 9,
            cell_height: 18,
            ascent: 14,
        };

        // Directly on leading wide char
        let (x, y, w, h) = calculate_cursor_rect(&grid, metrics, [5, 5]);
        assert_eq!(x, 5 + 4 * 9);
        assert_eq!(y, 5 + 2 * 18);
        assert_eq!(w, 18);
        assert_eq!(h, 18);

        // Cursor positioned on the spacer cell (index 5) must anchor back to index 4
        grid.cursor.col = 5;
        let (sx, sy, sw, sh) = calculate_cursor_rect(&grid, metrics, [5, 5]);
        assert_eq!(sx, 5 + 4 * 9);
        assert_eq!(sy, 5 + 2 * 18);
        assert_eq!(sw, 18);
        assert_eq!(sh, 18);
    }

    #[test]
    fn test_calculate_cursor_rect_clamping() {
        let mut grid = Grid::new(80, 24, 0);
        grid.cursor.row = 999;
        grid.cursor.col = 999;

        let metrics = CellMetrics {
            cell_width: 10,
            cell_height: 20,
            ascent: 15,
        };

        let (x, y, w, h) = calculate_cursor_rect(&grid, metrics, [0, 0]);
        // Should clamp to (79, 23)
        assert_eq!(x, 79 * 10);
        assert_eq!(y, 23 * 20);
        assert_eq!(w, 10);
        assert_eq!(h, 20);
    }
}

mod mouse_tests {
    use ftty::input::mouse::*;

    fn sgr() -> MouseEncoding {
        MouseEncoding::Sgr
    }

    #[test]
    fn private_modes_toggle_tracking_and_encoding() {
        let mut state = MouseState::default();
        assert!(!state.is_reporting());
        assert!(state.apply_private_mode(1000, true));
        assert_eq!(state.tracking, MouseTracking::Click);
        assert!(state.is_reporting());
        assert!(state.apply_private_mode(1006, true));
        assert_eq!(state.encoding, MouseEncoding::Sgr);
        assert!(state.apply_private_mode(1000, false));
        assert!(!state.is_reporting());
        assert!(state.apply_private_mode(1006, false));
        assert_eq!(state.encoding, MouseEncoding::X10);

        assert!(state.apply_private_mode(1002, true));
        assert_eq!(state.tracking, MouseTracking::Drag);
        assert!(state.apply_private_mode(1003, true));
        assert_eq!(state.tracking, MouseTracking::Motion);
        assert!(state.apply_private_mode(1005, true));
        assert_eq!(state.encoding, MouseEncoding::Utf8);
        assert!(state.apply_private_mode(1015, true));
        assert_eq!(state.encoding, MouseEncoding::Urxvt);

        // Cursor visibility and alternate screen modes are not mouse modes.
        assert!(!state.apply_private_mode(25, true));
        assert!(!state.apply_private_mode(1049, true));
    }

    #[test]
    fn mode_resets_only_disable_matching_active_modes() {
        let mut state = MouseState::default();
        // Enable Drag (1002), then attempt to reset Click (1000)
        state.apply_private_mode(1002, true);
        assert_eq!(state.tracking, MouseTracking::Drag);
        state.apply_private_mode(1000, false);
        assert_eq!(
            state.tracking,
            MouseTracking::Drag,
            "resetting 1000 must not disable 1002"
        );

        // Enable Sgr (1006), then attempt to reset Utf8 (1005)
        state.apply_private_mode(1006, true);
        assert_eq!(state.encoding, MouseEncoding::Sgr);
        state.apply_private_mode(1005, false);
        assert_eq!(
            state.encoding,
            MouseEncoding::Sgr,
            "resetting 1005 must not revert 1006"
        );
    }

    #[test]
    fn utf8_rejects_coordinates_beyond_2015() {
        let none = MouseModifiers::default();
        assert!(
            encode_mouse_event(MouseEncoding::Utf8, 0, 2015, 2015, true, false, none).is_some()
        );
        assert!(encode_mouse_event(MouseEncoding::Utf8, 0, 2016, 0, true, false, none).is_none());
        assert!(encode_mouse_event(MouseEncoding::Utf8, 0, 0, 2016, true, false, none).is_none());
    }

    #[test]
    fn motion_reporting_depends_on_tracking_mode() {
        let mut state = MouseState::default();
        assert!(!state.reports_motion(true));
        state.tracking = MouseTracking::Click;
        assert!(!state.reports_motion(true));
        state.tracking = MouseTracking::Drag;
        assert!(state.reports_motion(true));
        assert!(!state.reports_motion(false));
        state.tracking = MouseTracking::Motion;
        assert!(state.reports_motion(false));
    }

    #[test]
    fn sgr_encodes_press_release_and_motion() {
        let none = MouseModifiers::default();
        assert_eq!(
            encode_mouse_event(sgr(), 0, 4, 2, true, false, none),
            Some(b"\x1b[<0;5;3M".to_vec())
        );
        assert_eq!(
            encode_mouse_event(sgr(), 2, 4, 2, false, false, none),
            Some(b"\x1b[<2;5;3m".to_vec())
        );
        assert_eq!(
            encode_mouse_event(sgr(), 0, 4, 2, true, true, none),
            Some(b"\x1b[<32;5;3M".to_vec())
        );
        assert_eq!(
            encode_mouse_event(sgr(), 64, 0, 0, true, false, none),
            Some(b"\x1b[<64;1;1M".to_vec())
        );
    }

    #[test]
    fn modifiers_are_encoded_in_the_button_field() {
        let mods = MouseModifiers {
            shift: true,
            alt: true,
            ctrl: true,
        };
        assert_eq!(
            encode_mouse_event(sgr(), 1, 0, 0, true, false, mods),
            Some(b"\x1b[<29;1;1M".to_vec())
        );
    }

    #[test]
    fn x10_and_utf8_encodings_use_offset_bytes() {
        let none = MouseModifiers::default();
        assert_eq!(
            encode_mouse_event(MouseEncoding::X10, 0, 0, 0, true, false, none),
            Some(b"\x1b[M \x21\x21".to_vec())
        );
        // Legacy release reports button 3 and ignores the actual button identity.
        assert_eq!(
            encode_mouse_event(MouseEncoding::X10, 0, 0, 0, false, false, none),
            Some(b"\x1b[M#!!".to_vec())
        );
        // 233 = U+00E9, which is two UTF-8 bytes.
        assert_eq!(
            encode_mouse_event(MouseEncoding::Utf8, 0, 200, 0, true, false, none),
            Some(b"\x1b[M \xc3\xa9!".to_vec())
        );
        // Coordinates beyond the single-byte legacy range are rejected instead of wrapped.
        assert_eq!(
            encode_mouse_event(MouseEncoding::X10, 0, 300, 0, true, false, none),
            None
        );
        assert_eq!(
            encode_mouse_event(MouseEncoding::Urxvt, 0, 4, 2, true, false, none),
            Some(b"\x1b[32;5;3M".to_vec())
        );
        // Urxvt and Utf8 normalize release button code to 3 while preserving modifiers.
        assert_eq!(
            encode_mouse_event(MouseEncoding::Urxvt, 1, 4, 2, false, false, none),
            Some(b"\x1b[35;5;3M".to_vec())
        );
        let shift = MouseModifiers {
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            encode_mouse_event(MouseEncoding::Urxvt, 1, 4, 2, false, false, shift),
            Some(b"\x1b[39;5;3M".to_vec())
        );
        assert_eq!(
            encode_mouse_event(MouseEncoding::Utf8, 1, 0, 0, false, false, none),
            Some(b"\x1b[M#!!".to_vec())
        );
        assert_eq!(
            encode_mouse_event(MouseEncoding::Utf8, 1, 0, 0, false, false, shift),
            Some(b"\x1b[M'!!".to_vec())
        );
    }
}

mod selection_tests {
    use ftty::grid::{Grid, Row};
    use ftty::input::selection::*;

    #[test]
    fn test_selection_normalization() {
        let forward = Selection::new(
            SelectionPoint::new(1, 5),
            SelectionPoint::new(2, 10),
            SelectionType::Simple,
        );
        assert_eq!(
            forward.normalized(),
            (SelectionPoint::new(1, 5), SelectionPoint::new(2, 10))
        );

        let backward = Selection::new(
            SelectionPoint::new(3, 15),
            SelectionPoint::new(1, 2),
            SelectionType::Simple,
        );
        assert_eq!(
            backward.normalized(),
            (SelectionPoint::new(1, 2), SelectionPoint::new(3, 15))
        );
    }

    #[test]
    fn test_selection_contains() {
        let sel = Selection::new(
            SelectionPoint::new(2, 5),
            SelectionPoint::new(2, 15),
            SelectionType::Simple,
        );

        assert!(!sel.contains(2, 4));
        assert!(sel.contains(2, 5));
        assert!(sel.contains(2, 10));
        assert!(sel.contains(2, 15));
        assert!(!sel.contains(2, 16));
        assert!(!sel.contains(1, 10));
        assert!(!sel.contains(3, 10));
    }

    #[test]
    fn test_word_boundary_detection() {
        let mut row = Row::new(20);
        let text = "hello_world 123";
        for (i, c) in text.chars().enumerate() {
            row.cells[i].c = c;
        }

        // Inside "hello_world"
        assert_eq!(find_word_boundaries(&row, 4), (0, 10));
        // Inside "123"
        assert_eq!(find_word_boundaries(&row, 13), (12, 14));
    }

    #[test]
    fn test_selection_text_extraction() {
        let mut grid = Grid::new(20, 3, 10);
        // Write line 0
        for (i, c) in "echo hello".chars().enumerate() {
            grid.lines[0].cells[i].c = c;
        }
        // Write line 1
        for (i, c) in "world".chars().enumerate() {
            grid.lines[1].cells[i].c = c;
        }

        let sel = Selection::new(
            SelectionPoint::new(0, 5),
            SelectionPoint::new(1, 4),
            SelectionType::Simple,
        );

        let text = sel.extract_text(&grid);
        assert_eq!(text, "hello\nworld");
    }

    #[test]
    fn test_selection_line_span() {
        let sel = Selection::new(
            SelectionPoint::new(1, 5),
            SelectionPoint::new(3, 10),
            SelectionType::Simple,
        );

        // Outside selection
        assert_eq!(sel.line_span(0, 80), None);
        assert_eq!(sel.line_span(4, 80), None);

        // Start line: col 5 to max_col 79
        assert_eq!(sel.line_span(1, 80), Some((5, 79)));

        // Intermediate line: col 0 to max_col 79
        assert_eq!(sel.line_span(2, 80), Some((0, 79)));

        // End line: col 0 to col 10
        assert_eq!(sel.line_span(3, 80), Some((0, 10)));

        // Empty selection
        let empty = Selection::new(
            SelectionPoint::new(1, 5),
            SelectionPoint::new(1, 5),
            SelectionType::Simple,
        );
        assert_eq!(empty.line_span(1, 80), None);

        // Start column beyond grid width (e.g. after shrink) yields None on start line
        let shrunk = Selection::new(
            SelectionPoint::new(1, 100),
            SelectionPoint::new(2, 20),
            SelectionType::Simple,
        );
        assert_eq!(shrunk.line_span(1, 80), None);
        assert_eq!(shrunk.line_span(2, 80), Some((0, 20)));
    }
}
