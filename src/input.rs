//! Input handling: keyboard, mouse, selection, and IME.

use crate::config::KeybindingsConfig;
use crate::font::CellMetrics;
use crate::grid::{CellFlags, Grid, Row};
use std::collections::HashMap;
use std::io::Read;
use std::os::fd::OwnedFd;
use xkbcommon::xkb::{self, Context, KEYMAP_FORMAT_TEXT_V1, Keycode, Keymap, State, keysyms};

// --- Keyboard Handler & Actions ---

/// Semantic actions triggered by key combinations.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum KeyAction {
    ScrollbackUpPage,
    ScrollbackDownPage,
    ScrollbackUpLine,
    ScrollbackDownLine,
    ScrollbackHome,
    ScrollbackEnd,
    PromptPrev,
    PromptNext,
    FontIncrease,
    FontDecrease,
    FontReset,
    ClipboardCopy,
    ClipboardPaste,
    PrimaryPaste,
    PipeVisible(Vec<String>),
    PipeScrollback(Vec<String>),
    PipeSelection(Vec<String>),
}

/// Keyboard modifiers state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
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

#[must_use]
pub fn canonicalize_sym(sym: xkb::Keysym) -> xkb::Keysym {
    let u = xkb::keysym_to_utf32(sym);
    if let Some(ch) = char::from_u32(u)
        && ch.is_ascii_uppercase()
    {
        return xkb::utf32_to_keysym(ch.to_ascii_lowercase() as u32);
    }
    sym
}

/// Flags controlling the Kitty keyboard protocol progressive enhancement.
pub struct KittyKeyboardFlags;
impl KittyKeyboardFlags {
    pub const DISAMBIGUATE: u8 = 1;
    pub const REPORT_EVENT_TYPES: u8 = 2;
    pub const REPORT_ALTERNATE_KEYS: u8 = 4;
    pub const REPORT_ALL_KEYS_AS_ESC: u8 = 8;
    pub const REPORT_ASSOCIATED_TEXT: u8 = 16;
    /// All supported Kitty keyboard protocol enhancement flags.
    pub const ALL: u8 = KittyKeyboardFlags::DISAMBIGUATE
        | KittyKeyboardFlags::REPORT_EVENT_TYPES
        | KittyKeyboardFlags::REPORT_ALTERNATE_KEYS
        | KittyKeyboardFlags::REPORT_ALL_KEYS_AS_ESC
        | KittyKeyboardFlags::REPORT_ASSOCIATED_TEXT;
}

/// Handles key events and produces VT escape sequences or UTF-8 byte streams.
pub struct KeyboardHandler {
    #[doc(hidden)]
    pub context: Context,
    #[doc(hidden)]
    pub keymap: Option<Keymap>,
    #[doc(hidden)]
    pub state: Option<State>,
    pub kitty_flags: u8,
    pub kitty_stack: Vec<u8>,
    pub(crate) resolved_bindings: HashMap<(Modifiers, xkb::Keysym), KeyAction>,
}

impl Default for KeyboardHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KittyKey {
    Letter(char),
    Tilde(u32),
    Unicode(u32),
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
            kitty_flags: 0,
            kitty_stack: Vec::new(),
            resolved_bindings: HashMap::new(),
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
        use std::io::Seek;
        let mut file = std::fs::File::from(fd);
        let _ = file.rewind();
        let max_read = (size as u64).min(2 * 1024 * 1024);
        let mut buf = Vec::new();
        if let Err(err) = file.take(max_read).read_to_end(&mut buf) {
            eprintln!("ftty: failed reading keymap from fd: {err}");
            return;
        }

        while let Some(&0) = buf.last() {
            buf.pop();
        }
        if buf.is_empty() {
            return;
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

    /// Sets Kitty keyboard mode flags according to mode: 1 (replace), 2 (union), 3 (difference).
    pub fn set_kitty_mode(&mut self, flags: u8, mode: u8) {
        let masked = flags & KittyKeyboardFlags::ALL;
        match mode {
            1 => self.kitty_flags = masked,
            2 => self.kitty_flags |= masked,
            3 => self.kitty_flags &= !masked,
            _ => {}
        }
    }

    /// Pushes current flags and sets new flags.
    pub fn push_kitty_flags(&mut self, flags: u8) {
        if self.kitty_stack.len() >= 64 {
            self.kitty_stack.remove(0);
        }
        self.kitty_stack.push(self.kitty_flags);
        self.kitty_flags = flags & KittyKeyboardFlags::ALL;
    }

    /// Pops `count` frames from the kitty keyboard stack.
    pub fn pop_kitty_flags(&mut self, count: usize) {
        let n = count.max(1);
        for _ in 0..n {
            if let Some(f) = self.kitty_stack.pop() {
                self.kitty_flags = f;
            } else {
                self.kitty_flags = 0;
            }
        }
    }

    /// Translates key press/release events with Kitty keyboard protocol progressive enhancement.
    pub fn handle_key_event(
        &mut self,
        raw_keycode: u32,
        pressed: bool,
        is_repeat: bool,
    ) -> Option<Vec<u8>> {
        let state = self.state.as_ref()?;
        let keycode = Keycode::new(raw_keycode + 8);
        let keysym = state.key_get_one_sym(keycode);
        let sym = keysym.raw();

        let ctrl = state.mod_name_is_active("Control", xkb::STATE_MODS_EFFECTIVE);
        let alt = state.mod_name_is_active("Mod1", xkb::STATE_MODS_EFFECTIVE);
        let shift = state.mod_name_is_active("Shift", xkb::STATE_MODS_EFFECTIVE);
        let logo = state.mod_name_is_active("Mod4", xkb::STATE_MODS_EFFECTIVE);

        let event_type = if !pressed {
            3 // Release
        } else if is_repeat {
            2 // Repeat
        } else {
            1 // Press
        };

        if !pressed && (self.kitty_flags & KittyKeyboardFlags::REPORT_EVENT_TYPES == 0) {
            return None;
        }

        if self.kitty_flags > 0
            && let Some(bytes) = self.encode_kitty_key(
                sym,
                keycode,
                Modifiers {
                    shift,
                    alt,
                    ctrl,
                    logo,
                },
                event_type,
            )
        {
            return Some(bytes);
        }

        if pressed {
            self.handle_key(raw_keycode)
        } else {
            None
        }
    }

    #[doc(hidden)]
    pub fn encode_kitty_key(
        &self,
        sym: u32,
        keycode: Keycode,
        mods: Modifiers,
        event_type: u8,
    ) -> Option<Vec<u8>> {
        let key_format = match sym {
            keysyms::KEY_Return | keysyms::KEY_KP_Enter => KittyKey::Unicode(13),
            keysyms::KEY_Tab | keysyms::KEY_ISO_Left_Tab => KittyKey::Unicode(9),
            keysyms::KEY_BackSpace => KittyKey::Unicode(127),
            keysyms::KEY_Escape => KittyKey::Unicode(27),
            keysyms::KEY_Up => KittyKey::Letter('A'),
            keysyms::KEY_Down => KittyKey::Letter('B'),
            keysyms::KEY_Right => KittyKey::Letter('C'),
            keysyms::KEY_Left => KittyKey::Letter('D'),
            keysyms::KEY_Home => KittyKey::Letter('H'),
            keysyms::KEY_End => KittyKey::Letter('F'),
            keysyms::KEY_F1 => KittyKey::Letter('P'),
            keysyms::KEY_F2 => KittyKey::Letter('Q'),
            keysyms::KEY_F3 => KittyKey::Tilde(13),
            keysyms::KEY_F4 => KittyKey::Letter('S'),
            keysyms::KEY_Insert => KittyKey::Tilde(2),
            keysyms::KEY_Delete => KittyKey::Tilde(3),
            keysyms::KEY_Page_Up => KittyKey::Tilde(5),
            keysyms::KEY_Page_Down => KittyKey::Tilde(6),
            keysyms::KEY_F5 => KittyKey::Tilde(15),
            keysyms::KEY_F6 => KittyKey::Tilde(17),
            keysyms::KEY_F7 => KittyKey::Tilde(18),
            keysyms::KEY_F8 => KittyKey::Tilde(19),
            keysyms::KEY_F9 => KittyKey::Tilde(20),
            keysyms::KEY_F10 => KittyKey::Tilde(21),
            keysyms::KEY_F11 => KittyKey::Tilde(23),
            keysyms::KEY_F12 => KittyKey::Tilde(24),
            keysyms::KEY_F13..=keysyms::KEY_F35 => {
                KittyKey::Unicode(57376 + (sym - keysyms::KEY_F13))
            }
            keysyms::KEY_Caps_Lock => KittyKey::Unicode(57358),
            keysyms::KEY_Scroll_Lock => KittyKey::Unicode(57359),
            keysyms::KEY_Num_Lock => KittyKey::Unicode(57360),
            keysyms::KEY_Print => KittyKey::Unicode(57361),
            keysyms::KEY_Pause => KittyKey::Unicode(57362),
            keysyms::KEY_Menu => KittyKey::Unicode(57363),
            _ => {
                // Use the keysym's Unicode value so the codepoint reflects the key
                // identity (shift-aware, but unaffected by Ctrl/Alt/Logo, which only
                // transform the produced byte — e.g. Ctrl+a must report codepoint
                // 97 ('a'), not the control char 0x01).
                let cp = xkb::keysym_to_utf32(xkb::Keysym::new(sym));
                if cp == 0 {
                    return None;
                }
                KittyKey::Unicode(cp)
            }
        };

        let _has_modifiers = mods.ctrl || mods.alt || mods.shift || mods.logo;
        let is_functional = matches!(key_format, KittyKey::Letter(_) | KittyKey::Tilde(_));
        let is_special_disambiguated = matches!(
            key_format,
            KittyKey::Unicode(13 | 9 | 127 | 27 | 57358..=57398)
        );

        let report_types = self.kitty_flags & KittyKeyboardFlags::REPORT_EVENT_TYPES != 0;
        let disambiguate = self.kitty_flags & KittyKeyboardFlags::DISAMBIGUATE != 0;
        let all_keys = self.kitty_flags & KittyKeyboardFlags::REPORT_ALL_KEYS_AS_ESC != 0;
        let alternate = self.kitty_flags & KittyKeyboardFlags::REPORT_ALTERNATE_KEYS != 0;

        // Decide whether this key event is reported as a CSI u sequence.
        // Per the Kitty keyboard protocol progressive-enhancement spec:
        // - REPORT_ALL_KEYS_AS_ESC (8): every key is CSI-encoded.
        // - Functional/special keys are always CSI-encoded once the protocol is active
        //   (any flag set), because their release/repeat/alternate forms can only be
        //   carried by CSI u.
        // - DISAMBIGUATE (1): special keys + plain keys with a non-shift modifier.
        let is_csi_key = all_keys
            || is_functional
            || (disambiguate && (is_special_disambiguated || mods.ctrl || mods.alt || mods.logo));

        // When REPORT_EVENT_TYPES is off, only presses are reported.
        if !report_types && event_type != 1 {
            return None;
        }

        if !is_csi_key {
            return None;
        }

        let mut mod_val = 1;
        if mods.shift {
            mod_val += 1;
        }
        if mods.alt {
            mod_val += 2;
        }
        if mods.ctrl {
            mod_val += 4;
        }
        if mods.logo {
            mod_val += 8;
        }

        // REPORT_ALTERNATE_KEYS (4): for shifted Unicode keys, the first field becomes
        // `base:shifted` so the receiver can recover the shifted glyph. Aligns with
        // crossterm `parse_csi_u_with_shifted_keycode`. Functional/Tilde keys carry no
        // shifted glyph, so alternate is only applied to Unicode keys.
        let alternate_suffix: Option<String> = if alternate && mods.shift {
            match key_format {
                KittyKey::Unicode(shifted_cp) => self
                    .unshifted_codepoint(keycode)
                    .filter(|&base| base != shifted_cp)
                    .map(|base| format!(":{base}")),
                _ => None,
            }
        } else {
            None
        };

        let seq = match key_format {
            KittyKey::Letter(ch) => {
                if report_types && event_type != 1 {
                    format!("\x1b[1;{mod_val}:{event_type}{ch}")
                } else if mod_val > 1 {
                    format!("\x1b[1;{mod_val}{ch}")
                } else {
                    format!("\x1b[{ch}")
                }
            }
            KittyKey::Tilde(num) => {
                if report_types && event_type != 1 {
                    format!("\x1b[{num};{mod_val}:{event_type}~")
                } else if mod_val > 1 {
                    format!("\x1b[{num};{mod_val}~")
                } else {
                    format!("\x1b[{num}~")
                }
            }
            KittyKey::Unicode(codepoint) => {
                let first = if let Some(suffix) = alternate_suffix {
                    // `base:shifted` — `base` is the unshifted codepoint, `codepoint`
                    // holds the shifted glyph produced under shift.
                    let base = self.unshifted_codepoint(keycode).unwrap_or(codepoint);
                    format!("{base}{suffix}")
                } else {
                    format!("{codepoint}")
                };
                if report_types {
                    if mod_val == 1 && event_type == 1 {
                        format!("\x1b[{first}u")
                    } else if event_type == 1 {
                        format!("\x1b[{first};{mod_val}u")
                    } else {
                        format!("\x1b[{first};{mod_val}:{event_type}u")
                    }
                } else if mod_val == 1 {
                    format!("\x1b[{first}u")
                } else {
                    format!("\x1b[{first};{mod_val}u")
                }
            }
        };

        Some(seq.into_bytes())
    }

    /// Returns the Unicode codepoint a key produces with no modifiers active,
    /// used to populate the `REPORT_ALTERNATE_KEYS` base field for shifted keys.
    ///
    /// Builds a throwaway `State` from the active keymap with an empty modifier
    /// mask, so the returned value reflects the key's base (unshifted) level.
    fn unshifted_codepoint(&self, keycode: Keycode) -> Option<u32> {
        let keymap = self.keymap.as_ref()?;
        let state = State::new(keymap);
        // Empty mask => no modifiers => base layout level.
        state.key_get_utf8(keycode).chars().next().map(u32::from)
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

    /// Pre-compiles and caches resolved keybindings for constant-time key action dispatch.
    pub fn update_keybindings(&mut self, config: &KeybindingsConfig) {
        self.resolved_bindings = config.resolve_bindings_map();
    }

    /// Checks if a keycode matches an action in `KeybindingsConfig`.
    #[must_use]
    pub fn check_action(&self, key: u32, config: &KeybindingsConfig) -> Option<KeyAction> {
        let state = self.state.as_ref()?;
        let keycode = Keycode::new(key + 8);
        let sym = state.key_get_one_sym(keycode);
        let current_mods = self.modifiers();
        let canonical_sym = canonicalize_sym(sym);

        let get_action = |mods: Modifiers, s: xkb::Keysym| -> Option<KeyAction> {
            if !self.resolved_bindings.is_empty() {
                self.resolved_bindings.get(&(mods, s)).cloned()
            } else {
                let map = config.resolve_bindings_map();
                map.get(&(mods, s)).cloned()
            }
        };

        if let Some(action) = get_action(current_mods, canonical_sym) {
            return Some(action);
        }

        if canonical_sym != sym
            && let Some(action) = get_action(current_mods, sym)
        {
            return Some(action);
        }

        if current_mods.shift
            && (sym == xkb::Keysym::new(keysyms::KEY_plus)
                || canonical_sym == xkb::Keysym::new(keysyms::KEY_plus))
        {
            let mut unshifted = current_mods;
            unshifted.shift = false;
            if let Some(action) = get_action(unshifted, xkb::Keysym::new(keysyms::KEY_plus)) {
                return Some(action);
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

// --- IME ---

/// Active pre-edit text and cursor range from the input method.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Preedit {
    pub text: String,
    pub cursor_begin: i32,
    pub cursor_end: i32,
}

/// Double-buffered pending events for `text-input-v3` batches committed upon `Done`.
#[derive(Debug, Clone, Default)]
pub struct PendingImeEvents {
    pub delete_surrounding: Option<(u32, u32)>,
    pub commit_text: Option<String>,
    pub preedit: Option<Option<Preedit>>,
}

/// Tracks the active IME session state, pre-edit text, and double-buffered batches.
#[derive(Debug, Clone, Default)]
pub struct ImeState {
    pub active: bool,
    pub preedit: Option<Preedit>,
    pub pending: PendingImeEvents,
}

impl ImeState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stages a surrounding text deletion event into the pending batch.
    pub fn stage_delete(&mut self, before_length: u32, after_length: u32) {
        self.pending.delete_surrounding = Some((before_length, after_length));
    }

    /// Stages committed text into the pending batch.
    pub fn stage_commit(&mut self, text: Option<String>) {
        self.pending.commit_text = text;
    }

    /// Stages pre-edit string updates into the pending batch.
    pub fn stage_preedit(&mut self, text: Option<String>, cursor_begin: i32, cursor_end: i32) {
        match text {
            Some(t) if !t.is_empty() => {
                self.pending.preedit = Some(Some(Preedit {
                    text: t,
                    cursor_begin,
                    cursor_end,
                }));
            }
            _ => {
                self.pending.preedit = Some(None);
            }
        }
    }

    /// Atomically applies the pending batch upon `zwp_text_input_v3.done`.
    ///
    /// Returns `(delete_surrounding, commit_text)` ordered so deletion precedes commit.
    pub fn apply_done(&mut self) -> (Option<(u32, u32)>, Option<String>) {
        let delete = self.pending.delete_surrounding.take();
        let commit = self.pending.commit_text.take();

        if let Some(preedit_update) = self.pending.preedit.take() {
            self.preedit = preedit_update;
        } else if commit.is_some() {
            self.preedit = None;
        }

        (delete, commit)
    }

    /// Clears any active composition and pending batches upon focus loss or reset.
    pub fn clear(&mut self) {
        self.active = false;
        self.preedit = None;
        self.pending = PendingImeEvents::default();
    }
}

/// Computes the pixel-accurate bounding rectangle of the terminal cursor for IME popup positioning.
///
/// Returns `(x, y, width, height)` in surface-local pixels.
#[must_use]
pub fn calculate_cursor_rect(
    grid: &Grid,
    metrics: CellMetrics,
    padding: [u16; 2],
) -> (i32, i32, i32, i32) {
    let cw = metrics.cell_width as i32;
    let ch = metrics.cell_height as i32;
    let pad_x = i32::from(padding[0]);
    let pad_y = i32::from(padding[1]);

    let row = grid.cursor.row.min(grid.rows.saturating_sub(1));
    let mut col = grid.cursor.col.min(grid.cols.saturating_sub(1));

    // If placed on a wide character spacer, anchor to the leading wide character cell
    if row < grid.lines.len() {
        let line = &grid.lines[row];
        if col > 0
            && col < line.cells.len()
            && line.cells[col].flags.contains(CellFlags::WIDE_CHAR_SPACER)
        {
            col -= 1;
        }
    }

    let is_wide = if row < grid.lines.len() {
        let line = &grid.lines[row];
        if col < line.cells.len() {
            line.cells[col].flags.contains(CellFlags::WIDE_CHAR)
        } else {
            false
        }
    } else {
        false
    };

    let width = if is_wide {
        2.min(grid.cols.saturating_sub(col)) as i32 * cw
    } else {
        cw
    };
    let x = pad_x + (col as i32) * cw;
    let y = pad_y + (row as i32) * ch;

    (x, y, width, ch)
}

// --- Mouse ---

/// DECSET/DECRST private mode controlling which pointer events are reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseTracking {
    /// `?1000` is off: pointer events stay local to the terminal for text selection.
    #[default]
    Disabled,
    /// `?1000`: report button press and release only.
    Click,
    /// `?1002`: report press/release plus motion while a button is held.
    Drag,
    /// `?1003`: report every pointer motion.
    Motion,
}

/// Wire encoding used for mouse reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseEncoding {
    /// Legacy `CSI M` followed by three offset bytes (X10).
    #[default]
    X10,
    /// `?1005` UTF-8 extended coordinates.
    Utf8,
    /// `?1015` `CSI b;x;y M` with decimal coordinates.
    Urxvt,
    /// `?1006` SGR `CSI <b;x;y M/m`, which has no coordinate limit.
    Sgr,
}

/// Modifier bits carried inside the mouse report button field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MouseModifiers {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

/// Active mouse protocol as negotiated by the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MouseState {
    pub tracking: MouseTracking,
    pub encoding: MouseEncoding,
    pub utf8_mode: bool,
    pub sgr_mode: bool,
    pub urxvt_mode: bool,
}

impl MouseState {
    fn update_encoding(&mut self) {
        self.encoding = if self.sgr_mode {
            MouseEncoding::Sgr
        } else if self.urxvt_mode {
            MouseEncoding::Urxvt
        } else if self.utf8_mode {
            MouseEncoding::Utf8
        } else {
            MouseEncoding::X10
        };
    }

    /// Applies a DECSET (`enabled`) or DECRST private mode. Returns `true` when the
    /// mode is a mouse mode owned by this state.
    pub fn apply_private_mode(&mut self, mode: u16, enabled: bool) -> bool {
        match mode {
            1000 => {
                if enabled {
                    self.tracking = MouseTracking::Click;
                } else if self.tracking == MouseTracking::Click {
                    self.tracking = MouseTracking::Disabled;
                }
            }
            1002 => {
                if enabled {
                    self.tracking = MouseTracking::Drag;
                } else if self.tracking == MouseTracking::Drag {
                    self.tracking = MouseTracking::Disabled;
                }
            }
            1003 => {
                if enabled {
                    self.tracking = MouseTracking::Motion;
                } else if self.tracking == MouseTracking::Motion {
                    self.tracking = MouseTracking::Disabled;
                }
            }
            1005 => {
                self.utf8_mode = enabled;
                self.update_encoding();
            }
            1006 => {
                self.sgr_mode = enabled;
                self.update_encoding();
            }
            1015 => {
                self.urxvt_mode = enabled;
                self.update_encoding();
            }
            _ => return false,
        }
        true
    }

    /// Whether pointer events must be forwarded to the application.
    #[must_use]
    pub fn is_reporting(&self) -> bool {
        self.tracking != MouseTracking::Disabled
    }

    /// Whether motion is reported given whether a button is currently held down.
    #[must_use]
    pub fn reports_motion(&self, button_held: bool) -> bool {
        match self.tracking {
            MouseTracking::Disabled | MouseTracking::Click => false,
            MouseTracking::Drag => button_held,
            MouseTracking::Motion => true,
        }
    }
}

/// Encodes a mouse report for a terminal application.
///
/// `button` is the X11 button number (`0` left, `1` middle, `2` right, `64` wheel up,
/// `65` wheel down), `col`/`row` are zero-based cell coordinates and `motion` marks a
/// pure motion event. Returns `None` when the requested coordinates cannot be encoded.
#[must_use]
pub fn encode_mouse_event(
    encoding: MouseEncoding,
    button: u8,
    col: usize,
    row: usize,
    pressed: bool,
    motion: bool,
    modifiers: MouseModifiers,
) -> Option<Vec<u8>> {
    let mut code = u16::from(button);
    if motion {
        code |= 32;
    }
    if modifiers.shift {
        code |= 4;
    }
    if modifiers.alt {
        code |= 8;
    }
    if modifiers.ctrl {
        code |= 16;
    }
    let x = u16::try_from(col).ok()?.saturating_add(1);
    let y = u16::try_from(row).ok()?.saturating_add(1);

    match encoding {
        MouseEncoding::Sgr => {
            let terminator = if pressed { 'M' } else { 'm' };
            Some(format!("\x1b[<{code};{x};{y}{terminator}").into_bytes())
        }
        MouseEncoding::Urxvt => {
            let code = if pressed || motion {
                code
            } else {
                (code & !3) | 3
            };
            Some(format!("\x1b[{};{x};{y}M", code + 32).into_bytes())
        }
        MouseEncoding::Utf8 => {
            if col > 2015 || row > 2015 {
                return None;
            }
            let code = if pressed || motion {
                code
            } else {
                (code & !3) | 3
            };
            let mut out = b"\x1b[M".to_vec();
            for value in [code + 32, x + 32, y + 32] {
                push_utf8(&mut out, value);
            }
            Some(out)
        }
        MouseEncoding::X10 => {
            // The legacy encoding has no way to say which button was released.
            let code = if pressed || motion {
                code
            } else {
                (code & !3) | 3
            };
            let values = [code + 32, x + 32, y + 32];
            if values.iter().any(|value| *value > u16::from(u8::MAX)) {
                return None;
            }
            let mut out = b"\x1b[M".to_vec();
            out.extend(values.iter().map(|value| *value as u8));
            Some(out)
        }
    }
}

fn push_utf8(out: &mut Vec<u8>, value: u16) {
    match char::from_u32(u32::from(value)) {
        Some(c) => {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
        None => out.push(b'?'),
    }
}

// --- Selection ---

/// A point in the terminal grid history or active screen buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SelectionPoint {
    pub line: usize,
    pub col: usize,
}

impl SelectionPoint {
    #[must_use]
    pub const fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

/// The mode of mouse selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectionType {
    #[default]
    Simple,
    Word,
    Line,
}

/// Represents an active or completed text selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub start: SelectionPoint,
    pub end: SelectionPoint,
    pub kind: SelectionType,
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            start: SelectionPoint::new(0, 0),
            end: SelectionPoint::new(0, 0),
            kind: SelectionType::Simple,
        }
    }
}

impl Selection {
    #[must_use]
    pub fn new(start: SelectionPoint, end: SelectionPoint, kind: SelectionType) -> Self {
        Self { start, end, kind }
    }

    /// Normalizes the selection range into `(start, end)` where `start <= end` in reading order.
    #[must_use]
    pub fn normalized(&self) -> (SelectionPoint, SelectionPoint) {
        if self.start <= self.end {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }

    /// Clears the selection by resetting start and end to (0, 0).
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Checks whether the selection spans zero characters.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.start == self.end && self.kind == SelectionType::Simple
    }

    /// Returns `true` if the cell at absolute line and column is contained within the selection.
    #[must_use]
    pub fn contains(&self, line: usize, col: usize) -> bool {
        if self.is_empty() {
            return false;
        }
        let (start, end) = self.normalized();
        let point = SelectionPoint::new(line, col);

        match self.kind {
            SelectionType::Simple | SelectionType::Word => point >= start && point <= end,
            SelectionType::Line => line >= start.line && line <= end.line,
        }
    }

    /// Returns true if this selection spans across the given line.
    #[must_use]
    pub fn spans_line(&self, line: usize) -> bool {
        if self.is_empty() {
            return false;
        }
        let (start, end) = self.normalized();
        line >= start.line && line <= end.line
    }

    /// Returns the column span `Some((start_col, end_col))` selected on the given line, if any.
    #[must_use]
    pub fn line_span(&self, line: usize, cols: usize) -> Option<(usize, usize)> {
        if self.is_empty() || cols == 0 {
            return None;
        }
        let (start, end) = self.normalized();
        if line < start.line || line > end.line {
            return None;
        }
        let max_col = cols.saturating_sub(1);
        let start_col = if line == start.line && self.kind != SelectionType::Line {
            if start.col > max_col {
                return None;
            }
            start.col
        } else {
            0
        };
        let end_col = if line == end.line && self.kind != SelectionType::Line {
            end.col.min(max_col)
        } else {
            max_col
        };
        if start_col <= end_col {
            Some((start_col, end_col))
        } else {
            None
        }
    }

    /// Extracts clean UTF-8 text from the grid within this selection range.
    ///
    /// Respects wrapped lines (omits newline) and trims trailing spaces from rows.
    #[must_use]
    pub fn extract_text(&self, grid: &Grid) -> String {
        if self.is_empty() {
            return String::new();
        }

        let (start, end) = self.normalized();
        let total_lines = grid.scrollback.len() + grid.lines.len();
        let mut result = String::new();
        let mut line_str = String::with_capacity(grid.cols);

        let get_row = |idx: usize| -> Option<&Row> {
            if idx < grid.scrollback.len() {
                grid.scrollback.get(idx)
            } else {
                grid.lines.get(idx - grid.scrollback.len())
            }
        };

        for line_idx in start.line..=end.line.min(total_lines.saturating_sub(1)) {
            let Some(row) = get_row(line_idx) else {
                break;
            };

            let start_col = if line_idx == start.line && self.kind != SelectionType::Line {
                start.col.min(row.cells.len())
            } else {
                0
            };

            let end_col = if line_idx == end.line && self.kind != SelectionType::Line {
                (end.col + 1).min(row.cells.len())
            } else {
                row.cells.len()
            };

            if start_col >= end_col {
                continue;
            }

            line_str.clear();
            for cell in &row.cells[start_col..end_col] {
                if !cell
                    .flags
                    .intersects(CellFlags::WIDE_CHAR_SPACER | CellFlags::WRAP_SPACER)
                {
                    line_str.push(cell.c);
                }
            }

            // Only trim trailing whitespace if this is the last line or an unwrapped line
            if line_idx == end.line || !row.wrapped {
                let trimmed_len = line_str.trim_end_matches(' ').len();
                line_str.truncate(trimmed_len);
            }

            result.push_str(&line_str);

            // Add newline if unwrapped and not the final line
            if !row.wrapped && line_idx < end.line {
                result.push('\n');
            }
        }

        result
    }
}

/// Identifies word boundaries around a given column index in a row.
#[must_use]
pub fn find_word_boundaries(row: &Row, col: usize) -> (usize, usize) {
    if row.cells.is_empty() {
        return (0, 0);
    }
    let col = col.min(row.cells.len().saturating_sub(1));
    let target_char = row.cells[col].c;

    let is_word_char = |c: char| c.is_alphanumeric() || c == '_';
    let target_is_word = is_word_char(target_char);

    // Expand left
    let mut start = col;
    while start > 0 {
        let prev = row.cells[start - 1].c;
        if is_word_char(prev) == target_is_word && !prev.is_whitespace() {
            start -= 1;
        } else {
            break;
        }
    }

    // Expand right
    let mut end = col;
    while end + 1 < row.cells.len() {
        let next = row.cells[end + 1].c;
        if is_word_char(next) == target_is_word && !next.is_whitespace() {
            end += 1;
        } else {
            break;
        }
    }

    (start, end)
}

pub mod ime {
    pub use super::{ImeState, Preedit, calculate_cursor_rect};
}

pub mod mouse {
    pub use super::{MouseEncoding, MouseModifiers, MouseState, MouseTracking, encode_mouse_event};
}

pub mod selection {
    pub use super::{Selection, SelectionPoint, SelectionType, find_word_boundaries};
}
