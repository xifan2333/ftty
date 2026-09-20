use xkbcommon::xkb::{self, KEYMAP_FORMAT_TEXT_V1, Keymap, keysyms};

use crate::config::KeybindingsConfig;
use crate::input::{KeyAction, KeyboardHandler, KittyKeyboardFlags, parse_key_combo};

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
            crate::config::ActionDef::Simple("none".to_string()),
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
            crate::config::ActionDef::Simple("none".to_string()),
        )]),
    };
    assert_eq!(handler.check_action(110, &custom_primary_config), None);

    // User override: disabling clipboard_copy with "none"
    let custom_config = KeybindingsConfig {
        bindings: std::collections::HashMap::from([(
            "Ctrl+Shift+C".to_string(),
            crate::config::ActionDef::Simple("none".to_string()),
        )]),
    };
    handler.update_modifiers(5, 0, 0, 0);
    assert_eq!(handler.check_action(46, &custom_config), None);

    // Custom pipe action
    let custom_pipe_config = KeybindingsConfig {
        bindings: std::collections::HashMap::from([(
            "Ctrl+Shift+U".to_string(),
            crate::config::ActionDef::Pipe(crate::config::PipeActionDef::PipeVisible(
                crate::config::CommandDef::List(vec!["urlscan".to_string()]),
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
        crate::input::Modifiers::default(),
        1,
    );
    assert_eq!(f13_bytes, Some(b"\x1b[57376u".to_vec()));

    let f35_bytes = handler.encode_kitty_key(
        xkbcommon::xkb::keysyms::KEY_F35,
        xkbcommon::xkb::Keycode::new(0),
        crate::input::Modifiers::default(),
        1,
    );
    assert_eq!(f35_bytes, Some(b"\x1b[57398u".to_vec()));
}
