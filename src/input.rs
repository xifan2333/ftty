//! Keyboard input handling and key-to-VT escape sequence translation via xkbcommon.

use std::os::fd::RawFd;
use xkbcommon::xkb::{self, Context, KEYMAP_FORMAT_TEXT_V1, Keycode, Keymap, State, keysyms};

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

    /// Initializes keymap from a raw file descriptor (e.g. from Wayland `wl_keyboard.keymap`).
    ///
    /// # Safety
    /// Caller must ensure `fd` is a valid, readable file descriptor representing a keymap.
    pub unsafe fn set_keymap_from_fd(&mut self, fd: RawFd, size: usize) {
        let mut buf = vec![0u8; size];
        let mut total_read = 0;

        while total_read < size {
            let res =
                unsafe { libc::read(fd, buf[total_read..].as_mut_ptr().cast(), size - total_read) };
            if res > 0 {
                total_read += res as usize;
            } else if res == 0 {
                break;
            } else {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                eprintln!("ftty: failed reading keymap from fd: {err}");
                return;
            }
        }

        buf.truncate(total_read);
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
}
