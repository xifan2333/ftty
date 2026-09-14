//! Keyboard input handling and key-to-VT escape sequence translation via xkbcommon.

use std::io::Read;
use std::os::fd::OwnedFd;
use xkbcommon::xkb::{self, Context, KEYMAP_FORMAT_TEXT_V1, Keycode, Keymap, State, keysyms};

use crate::config::KeybindingsConfig;

/// Semantic actions triggered by key combinations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyAction {
    ScrollbackUpPage,
    ScrollbackDownPage,
    ScrollbackUpLine,
    ScrollbackDownLine,
    ScrollbackHome,
    ScrollbackEnd,
    FontIncrease,
    FontDecrease,
    FontReset,
    ClipboardCopy,
    ClipboardPaste,
    PrimaryPaste,
}

/// Keyboard modifiers state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
}

/// Parses a key combination string (e.g. `"Shift+PageUp"` or `"Ctrl+Shift+C"`).
#[must_use]
pub fn parse_key_combo(s: &str) -> Option<(Modifiers, xkb::Keysym)> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("none") {
        return None;
    }

    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut logo = false;

    // Handle trailing '+' in combo like "Ctrl++"
    let (mod_part, key_str) = if let Some(prefix) = s.strip_suffix("++") {
        (prefix, "+")
    } else {
        match s.rfind('+') {
            Some(idx) => (&s[..idx], &s[idx + 1..]),
            None => ("", s),
        }
    };

    if !mod_part.is_empty() {
        for part in mod_part.split('+') {
            let p = part.trim();
            if p.eq_ignore_ascii_case("ctrl") || p.eq_ignore_ascii_case("control") {
                ctrl = true;
            } else if p.eq_ignore_ascii_case("alt") || p.eq_ignore_ascii_case("mod1") {
                alt = true;
            } else if p.eq_ignore_ascii_case("shift") {
                shift = true;
            } else if p.eq_ignore_ascii_case("super")
                || p.eq_ignore_ascii_case("mod4")
                || p.eq_ignore_ascii_case("logo")
            {
                logo = true;
            } else {
                return None;
            }
        }
    }

    let key_trimmed = key_str.trim();
    let sym = match key_trimmed.to_ascii_lowercase().as_str() {
        "pageup" | "page_up" => xkb::Keysym::new(keysyms::KEY_Page_Up),
        "pagedown" | "page_down" => xkb::Keysym::new(keysyms::KEY_Page_Down),
        "kp_pageup" | "kp_page_up" => xkb::Keysym::new(keysyms::KEY_KP_Page_Up),
        "kp_pagedown" | "kp_page_down" => xkb::Keysym::new(keysyms::KEY_KP_Page_Down),
        "home" => xkb::Keysym::new(keysyms::KEY_Home),
        "end" => xkb::Keysym::new(keysyms::KEY_End),
        "up" => xkb::Keysym::new(keysyms::KEY_Up),
        "down" => xkb::Keysym::new(keysyms::KEY_Down),
        "left" => xkb::Keysym::new(keysyms::KEY_Left),
        "right" => xkb::Keysym::new(keysyms::KEY_Right),
        "insert" => xkb::Keysym::new(keysyms::KEY_Insert),
        "+" | "plus" | "kp_add" => xkb::Keysym::new(keysyms::KEY_plus),
        "=" | "equal" => xkb::Keysym::new(keysyms::KEY_equal),
        "-" | "minus" | "kp_subtract" => xkb::Keysym::new(keysyms::KEY_minus),
        "0" | "kp_0" => xkb::Keysym::new(keysyms::KEY_0),
        other => {
            if other.len() == 1 {
                let ch = other.chars().next()?;
                xkb::utf32_to_keysym(ch as u32)
            } else {
                xkb::keysym_from_name(other, xkb::KEYSYM_CASE_INSENSITIVE)
            }
        }
    };

    if sym == xkb::Keysym::new(keysyms::KEY_NoSymbol) {
        None
    } else {
        Some((
            Modifiers {
                ctrl,
                alt,
                shift,
                logo,
            },
            sym,
        ))
    }
}

fn sym_matches(pressed: xkb::Keysym, target: xkb::Keysym) -> bool {
    if pressed == target {
        return true;
    }
    let p_char = char::from_u32(xkb::keysym_to_utf32(pressed));
    let t_char = char::from_u32(xkb::keysym_to_utf32(target));
    if let (Some(p), Some(t)) = (p_char, t_char) {
        p.eq_ignore_ascii_case(&t)
    } else {
        false
    }
}

/// Handles key events and produces VT escape sequences or UTF-8 byte streams.
pub struct KeyboardHandler {
    context: Context,
    keymap: Option<Keymap>,
    state: Option<State>,
}

impl Default for KeyboardHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyboardHandler {
    #[must_use]
    pub fn new() -> Self {
        let context = Context::new(xkb::CONTEXT_NO_FLAGS);
        let (keymap, state) =
            Keymap::new_from_names(&context, "", "", "", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS)
                .map(|km| {
                    let state = State::new(&km);
                    (Some(km), Some(state))
                })
                .unwrap_or((None, None));

        Self {
            context,
            keymap,
            state,
        }
    }

    /// Initializes keymap from a string (e.g. from Wayland `wl_keyboard.keymap`).
    pub fn set_keymap_from_string(&mut self, keymap_str: &str) {
        if let Some(keymap) = Keymap::new_from_string(
            &self.context,
            keymap_str.to_string(),
            KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        ) {
            self.state = Some(State::new(&keymap));
            self.keymap = Some(keymap);
        }
    }

    /// Reads at most `size` bytes from an owned Wayland keymap descriptor, then closes it.
    pub fn set_keymap_from_fd(&mut self, fd: OwnedFd, size: usize) {
        let mut buf = Vec::new();
        if let Err(err) = std::fs::File::from(fd)
            .take(size as u64)
            .read_to_end(&mut buf)
        {
            eprintln!("ftty: failed reading keymap from fd: {err}");
            return;
        }

        if let Some(&0) = buf.last() {
            buf.pop();
        }
        match std::str::from_utf8(&buf) {
            Ok(s) => self.set_keymap_from_string(s),
            Err(e) => eprintln!("ftty: invalid UTF-8 in keymap: {e}"),
        }
    }

    /// Updates modifier state from Wayland `wl_keyboard.modifiers`.
    pub fn update_modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        if let Some(state) = &mut self.state {
            state.update_mask(depressed, latched, locked, 0, 0, group);
        }
    }

    /// Returns the currently effective modifier state.
    #[must_use]
    pub fn modifiers(&self) -> Modifiers {
        let Some(state) = self.state.as_ref() else {
            return Modifiers::default();
        };
        Modifiers {
            ctrl: state.mod_name_is_active("Control", xkb::STATE_MODS_EFFECTIVE),
            alt: state.mod_name_is_active("Mod1", xkb::STATE_MODS_EFFECTIVE),
            shift: state.mod_name_is_active("Shift", xkb::STATE_MODS_EFFECTIVE),
            logo: state.mod_name_is_active("Mod4", xkb::STATE_MODS_EFFECTIVE),
        }
    }

    /// Checks if a keycode matches an action in `KeybindingsConfig`.
    #[must_use]
    pub fn check_action(&self, key: u32, config: &KeybindingsConfig) -> Option<KeyAction> {
        let state = self.state.as_ref()?;
        let keycode = Keycode::new(key + 8);
        let sym = state.key_get_one_sym(keycode);
        let current_mods = self.modifiers();

        let bindings = [
            (
                KeyAction::ScrollbackUpPage,
                config.scrollback_up_page.as_ref(),
                &["Shift+PageUp", "Shift+KP_PageUp"][..],
            ),
            (
                KeyAction::ScrollbackDownPage,
                config.scrollback_down_page.as_ref(),
                &["Shift+PageDown", "Shift+KP_PageDown"][..],
            ),
            (
                KeyAction::ScrollbackUpLine,
                config.scrollback_up_line.as_ref(),
                &["Ctrl+Shift+Up"][..],
            ),
            (
                KeyAction::ScrollbackDownLine,
                config.scrollback_down_line.as_ref(),
                &["Ctrl+Shift+Down"][..],
            ),
            (
                KeyAction::ScrollbackHome,
                config.scrollback_home.as_ref(),
                &["Shift+Home"][..],
            ),
            (
                KeyAction::ScrollbackEnd,
                config.scrollback_end.as_ref(),
                &["Shift+End"][..],
            ),
            (
                KeyAction::FontIncrease,
                config.font_increase.as_ref(),
                &["Ctrl+Plus", "Ctrl+Equal"][..],
            ),
            (
                KeyAction::FontDecrease,
                config.font_decrease.as_ref(),
                &["Ctrl+Minus"][..],
            ),
            (
                KeyAction::FontReset,
                config.font_reset.as_ref(),
                &["Ctrl+0"][..],
            ),
            (
                KeyAction::ClipboardCopy,
                config.clipboard_copy.as_ref(),
                &[][..],
            ),
            (
                KeyAction::ClipboardPaste,
                config.clipboard_paste.as_ref(),
                &[][..],
            ),
            (
                KeyAction::PrimaryPaste,
                config.primary_paste.as_ref(),
                &[][..],
            ),
        ];

        for (action, configured, defaults) in bindings {
            let combos: Vec<&str> = match configured {
                Some(c) => c.to_combos(),
                None => defaults.to_vec(),
            };

            for combo_str in combos {
                if let Some((target_mods, target_sym)) = parse_key_combo(combo_str)
                    && current_mods.ctrl == target_mods.ctrl
                    && current_mods.alt == target_mods.alt
                    && current_mods.logo == target_mods.logo
                    && (current_mods.shift == target_mods.shift
                        || (!target_mods.shift
                            && current_mods.shift
                            && target_sym == xkb::Keysym::new(keysyms::KEY_plus)))
                    && sym_matches(sym, target_sym)
                {
                    return Some(action);
                }
            }
        }

        None
    }

    /// Translates a raw Linux keycode into bytes to write to the PTY.
    /// Note: Wayland keycodes are evdev scancodes (offset by 8 for X11/XKB keycodes).
    pub fn handle_key(&mut self, raw_keycode: u32) -> Option<Vec<u8>> {
        let state = self.state.as_ref()?;
        // Linux evdev scancode -> XKB keycode (+8)
        let keycode = Keycode::new(raw_keycode + 8);
        let keysym = state.key_get_one_sym(keycode);
        let sym = keysym.raw();

        let ctrl = state.mod_name_is_active(&xkb::MOD_NAME_CTRL, xkb::STATE_MODS_EFFECTIVE);
        let alt = state.mod_name_is_active(&xkb::MOD_NAME_ALT, xkb::STATE_MODS_EFFECTIVE);
        let shift = state.mod_name_is_active(&xkb::MOD_NAME_SHIFT, xkb::STATE_MODS_EFFECTIVE);

        // Functional navigation / control keys
        let seq: Option<&[u8]> = match sym {
            keysyms::KEY_Return | keysyms::KEY_KP_Enter => Some(b"\r"),
            keysyms::KEY_BackSpace => Some(b"\x7f"),
            keysyms::KEY_Tab => {
                if shift {
                    Some(b"\x1b[Z")
                } else {
                    Some(b"\t")
                }
            }
            keysyms::KEY_ISO_Left_Tab => Some(b"\x1b[Z"),
            keysyms::KEY_Escape => Some(b"\x1b"),
            keysyms::KEY_Up => Some(b"\x1b[A"),
            keysyms::KEY_Down => Some(b"\x1b[B"),
            keysyms::KEY_Right => Some(b"\x1b[C"),
            keysyms::KEY_Left => Some(b"\x1b[D"),
            keysyms::KEY_Home => Some(b"\x1b[H"),
            keysyms::KEY_End => Some(b"\x1b[F"),
            keysyms::KEY_Insert => Some(b"\x1b[2~"),
            keysyms::KEY_Delete => Some(b"\x1b[3~"),
            keysyms::KEY_Page_Up => Some(b"\x1b[5~"),
            keysyms::KEY_Page_Down => Some(b"\x1b[6~"),
            keysyms::KEY_F1 => Some(b"\x1bOP"),
            keysyms::KEY_F2 => Some(b"\x1bOQ"),
            keysyms::KEY_F3 => Some(b"\x1bOR"),
            keysyms::KEY_F4 => Some(b"\x1bOS"),
            keysyms::KEY_F5 => Some(b"\x1b[15~"),
            keysyms::KEY_F6 => Some(b"\x1b[17~"),
            keysyms::KEY_F7 => Some(b"\x1b[18~"),
            keysyms::KEY_F8 => Some(b"\x1b[19~"),
            keysyms::KEY_F9 => Some(b"\x1b[20~"),
            keysyms::KEY_F10 => Some(b"\x1b[21~"),
            keysyms::KEY_F11 => Some(b"\x1b[23~"),
            keysyms::KEY_F12 => Some(b"\x1b[24~"),
            _ => None,
        };

        if let Some(bytes) = seq {
            let mut out = Vec::new();
            if alt {
                out.push(0x1b);
            }
            out.extend_from_slice(bytes);
            return Some(out);
        }

        // Control key combinations
        if ctrl {
            match sym {
                keysyms::KEY_a..=keysyms::KEY_z => {
                    let byte = (sym - keysyms::KEY_a + 1) as u8;
                    let mut out = Vec::new();
                    if alt {
                        out.push(0x1b);
                    }
                    out.push(byte);
                    return Some(out);
                }
                keysyms::KEY_A..=keysyms::KEY_Z => {
                    let byte = (sym - keysyms::KEY_A + 1) as u8;
                    let mut out = Vec::new();
                    if alt {
                        out.push(0x1b);
                    }
                    out.push(byte);
                    return Some(out);
                }
                keysyms::KEY_space | keysyms::KEY_at => return Some(vec![0x00]),
                keysyms::KEY_bracketleft => return Some(vec![0x1b]),
                keysyms::KEY_backslash => return Some(vec![0x1c]),
                keysyms::KEY_bracketright => return Some(vec![0x1d]),
                keysyms::KEY_asciicircum => return Some(vec![0x1e]),
                keysyms::KEY_underscore => return Some(vec![0x1f]),
                _ => {}
            }
        }

        // Regular character output from xkb state
        let utf8 = state.key_get_utf8(keycode);
        if !utf8.is_empty() {
            let mut out = Vec::new();
            if alt {
                out.push(0x1b);
            }
            out.extend_from_slice(utf8.as_bytes());
            return Some(out);
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let handler = KeyboardHandler::new();
        let config = KeybindingsConfig::default();

        // With no modifiers active, PageUp (evdev 104) is NOT an action
        assert!(handler.check_action(104, &config).is_none());
    }
}
