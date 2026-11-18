//! Translates key events into VT escape sequences, kitty keyboard protocol, and UTF-8 bytes.

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::OwnedFd;

use xkbcommon::xkb::{self, Context, KEYMAP_FORMAT_TEXT_V1, Keycode, Keymap, State, keysyms};

use crate::config::KeybindingsConfig;

use super::keys::{KeyAction, KittyKeyboardFlags, Modifiers, canonicalize_sym, is_modifier_key};

/// Handles key events and produces VT escape sequences or UTF-8 byte streams.
pub struct KeyboardHandler {
    pub context: Context,
    pub keymap: Option<Keymap>,
    pub state: Option<State>,
    pub kitty_flags: u8,
    pub kitty_stack: Vec<u8>,
    pub current_group: u32,
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

/// Maps functional keysyms to their Kitty key encodings. One keysym per row;
/// the `KEY_F13..=KEY_F35` range and the Unicode fallback layer on top.
static KEY_FORMAT_TABLE: &[(u32, KittyKey)] = &[
    (keysyms::KEY_Return, KittyKey::Unicode(13)),
    (keysyms::KEY_KP_Enter, KittyKey::Unicode(13)),
    (keysyms::KEY_Tab, KittyKey::Unicode(9)),
    (keysyms::KEY_ISO_Left_Tab, KittyKey::Unicode(9)),
    (keysyms::KEY_BackSpace, KittyKey::Unicode(127)),
    (keysyms::KEY_Escape, KittyKey::Unicode(27)),
    (keysyms::KEY_Up, KittyKey::Letter('A')),
    (keysyms::KEY_Down, KittyKey::Letter('B')),
    (keysyms::KEY_Right, KittyKey::Letter('C')),
    (keysyms::KEY_Left, KittyKey::Letter('D')),
    (keysyms::KEY_Home, KittyKey::Letter('H')),
    (keysyms::KEY_End, KittyKey::Letter('F')),
    (keysyms::KEY_F1, KittyKey::Letter('P')),
    (keysyms::KEY_F2, KittyKey::Letter('Q')),
    (keysyms::KEY_F3, KittyKey::Tilde(13)),
    (keysyms::KEY_F4, KittyKey::Letter('S')),
    (keysyms::KEY_Insert, KittyKey::Tilde(2)),
    (keysyms::KEY_Delete, KittyKey::Tilde(3)),
    (keysyms::KEY_Page_Up, KittyKey::Tilde(5)),
    (keysyms::KEY_Page_Down, KittyKey::Tilde(6)),
    (keysyms::KEY_F5, KittyKey::Tilde(15)),
    (keysyms::KEY_F6, KittyKey::Tilde(17)),
    (keysyms::KEY_F7, KittyKey::Tilde(18)),
    (keysyms::KEY_F8, KittyKey::Tilde(19)),
    (keysyms::KEY_F9, KittyKey::Tilde(20)),
    (keysyms::KEY_F10, KittyKey::Tilde(21)),
    (keysyms::KEY_F11, KittyKey::Tilde(23)),
    (keysyms::KEY_F12, KittyKey::Tilde(24)),
    (keysyms::KEY_Caps_Lock, KittyKey::Unicode(57358)),
    (keysyms::KEY_Scroll_Lock, KittyKey::Unicode(57359)),
    (keysyms::KEY_Num_Lock, KittyKey::Unicode(57360)),
    (keysyms::KEY_Print, KittyKey::Unicode(57361)),
    (keysyms::KEY_Pause, KittyKey::Unicode(57362)),
    (keysyms::KEY_Menu, KittyKey::Unicode(57363)),
    (keysyms::KEY_Shift_L, KittyKey::Unicode(57441)),
    (keysyms::KEY_Control_L, KittyKey::Unicode(57442)),
    (keysyms::KEY_Alt_L, KittyKey::Unicode(57443)),
    (keysyms::KEY_Super_L, KittyKey::Unicode(57444)),
    (keysyms::KEY_Hyper_L, KittyKey::Unicode(57445)),
    (keysyms::KEY_Meta_L, KittyKey::Unicode(57446)),
    (keysyms::KEY_Shift_R, KittyKey::Unicode(57447)),
    (keysyms::KEY_Control_R, KittyKey::Unicode(57448)),
    (keysyms::KEY_Alt_R, KittyKey::Unicode(57449)),
    (keysyms::KEY_Super_R, KittyKey::Unicode(57450)),
    (keysyms::KEY_Hyper_R, KittyKey::Unicode(57451)),
    (keysyms::KEY_Meta_R, KittyKey::Unicode(57452)),
    (keysyms::KEY_ISO_Level3_Shift, KittyKey::Unicode(57453)),
    (keysyms::KEY_ISO_Level5_Shift, KittyKey::Unicode(57454)),
];

/// Resolves a keysym to its Kitty encoding: exact table hit, the F13..F35 run,
/// or the keysym's own Unicode value (None when the keysym carries no text).
fn kitty_key_format(sym: u32) -> Option<KittyKey> {
    if let Some(k) = KEY_FORMAT_TABLE
        .iter()
        .find(|&&(s, _)| s == sym)
        .map(|&(_, k)| k)
    {
        return Some(k);
    }
    if (keysyms::KEY_F13..=keysyms::KEY_F35).contains(&sym) {
        return Some(KittyKey::Unicode(57376 + (sym - keysyms::KEY_F13)));
    }
    let cp = xkb::keysym_to_utf32(xkb::Keysym::new(sym));
    (cp != 0).then_some(KittyKey::Unicode(cp))
}

/// Collapses release/repeat event types when `REPORT_EVENT_TYPES` is off:
/// releases are dropped, repeats become plain presses. `None` means "drop".
fn effective_event_type(report_types: bool, event_type: u8) -> Option<u8> {
    if report_types {
        return Some(event_type);
    }
    if event_type == 3 {
        return None;
    }
    Some(1)
}

/// Decides whether a key event is reported as a CSI u sequence per the Kitty
/// progressive-enhancement spec: flag 8 covers everything, functional/special
/// keys always qualify, and DISAMBIGUATE additionally covers plain keys with a
/// non-shift modifier.
fn is_csi_key(flags: u8, key_format: KittyKey, mods: Modifiers) -> bool {
    let all_keys = flags & KittyKeyboardFlags::REPORT_ALL_KEYS_AS_ESC != 0;
    let disambiguate = flags & KittyKeyboardFlags::DISAMBIGUATE != 0;
    let is_functional = matches!(key_format, KittyKey::Letter(_) | KittyKey::Tilde(_));
    let is_special_disambiguated = matches!(
        key_format,
        KittyKey::Unicode(13 | 9 | 127 | 27 | 57361..=57363 | 57376..=57440)
    );
    all_keys
        || is_functional
        || (disambiguate && (is_special_disambiguated || mods.ctrl || mods.alt || mods.logo))
}

/// Kitty modifier field: 1 + one bit per active modifier group.
fn modifier_value(mods: Modifiers) -> u32 {
    1 + u32::from(mods.shift)
        + 2 * u32::from(mods.alt)
        + 4 * u32::from(mods.ctrl)
        + 8 * u32::from(mods.logo)
}

/// Letter-type CSI sequences: `ESC [ 1;mod:event Ch` / `ESC [ 1;mod Ch` / `ESC [ Ch`.
fn format_letter(ch: char, mod_val: u32, effective_event_type: u8, report_types: bool) -> String {
    if report_types && effective_event_type != 1 {
        format!("\x1b[1;{mod_val}:{effective_event_type}{ch}")
    } else if mod_val > 1 {
        format!("\x1b[1;{mod_val}{ch}")
    } else {
        format!("\x1b[{ch}")
    }
}

/// Tilde-type CSI sequences: `ESC [ num;mod:event ~` / `ESC [ num;mod ~` / `ESC [ num ~`.
fn format_tilde(num: u32, mod_val: u32, effective_event_type: u8, report_types: bool) -> String {
    if report_types && effective_event_type != 1 {
        format!("\x1b[{num};{mod_val}:{effective_event_type}~")
    } else if mod_val > 1 {
        format!("\x1b[{num};{mod_val}~")
    } else {
        format!("\x1b[{num}~")
    }
}

/// Unicode/`u`-terminated CSI sequences, mixing the optional `base:shifted`
/// first field, event types, and the associated-text field.
fn format_unicode(
    first: String,
    mod_val: u32,
    effective_event_type: u8,
    report_types: bool,
    text_codepoints: Option<&str>,
) -> String {
    // Associated text (REPORT_ASSOCIATED_TEXT) is appended as its own field.
    if let Some(text) = text_codepoints {
        return if report_types {
            format!("\x1b[{first};{mod_val}:{effective_event_type};{text}u")
        } else {
            format!("\x1b[{first};{mod_val};{text}u")
        };
    }

    // With event types: plain presses omit `:event`, all other types keep it.
    if report_types {
        if mod_val == 1 && effective_event_type == 1 {
            return format!("\x1b[{first}u");
        }
        if effective_event_type == 1 {
            return format!("\x1b[{first};{mod_val}u");
        }
        return format!("\x1b[{first};{mod_val}:{effective_event_type}u");
    }

    if mod_val == 1 {
        format!("\x1b[{first}u")
    } else {
        format!("\x1b[{first};{mod_val}u")
    }
}

/// Legacy (non-kitty) escape sequences for functional keys, keyed by keysym.
/// `KEY_Tab` is handled separately because it emits `ESC [ Z` when shifted.
static LEGACY_SEQ_TABLE: &[(u32, &[u8])] = &[
    (keysyms::KEY_Return, b"\r"),
    (keysyms::KEY_KP_Enter, b"\r"),
    (keysyms::KEY_BackSpace, b"\x7f"),
    (keysyms::KEY_ISO_Left_Tab, b"\x1b[Z"),
    (keysyms::KEY_Escape, b"\x1b"),
    (keysyms::KEY_Up, b"\x1b[A"),
    (keysyms::KEY_Down, b"\x1b[B"),
    (keysyms::KEY_Right, b"\x1b[C"),
    (keysyms::KEY_Left, b"\x1b[D"),
    (keysyms::KEY_Home, b"\x1b[H"),
    (keysyms::KEY_End, b"\x1b[F"),
    (keysyms::KEY_Insert, b"\x1b[2~"),
    (keysyms::KEY_Delete, b"\x1b[3~"),
    (keysyms::KEY_Page_Up, b"\x1b[5~"),
    (keysyms::KEY_Page_Down, b"\x1b[6~"),
    (keysyms::KEY_F1, b"\x1bOP"),
    (keysyms::KEY_F2, b"\x1bOQ"),
    (keysyms::KEY_F3, b"\x1bOR"),
    (keysyms::KEY_F4, b"\x1bOS"),
    (keysyms::KEY_F5, b"\x1b[15~"),
    (keysyms::KEY_F6, b"\x1b[17~"),
    (keysyms::KEY_F7, b"\x1b[18~"),
    (keysyms::KEY_F8, b"\x1b[19~"),
    (keysyms::KEY_F9, b"\x1b[20~"),
    (keysyms::KEY_F10, b"\x1b[21~"),
    (keysyms::KEY_F11, b"\x1b[23~"),
    (keysyms::KEY_F12, b"\x1b[24~"),
];

/// Legacy functional-key sequence for a keysym, with the shifted-tab special case.
fn legacy_functional_seq(sym: u32, shift: bool) -> Option<&'static [u8]> {
    if sym == keysyms::KEY_Tab {
        return Some(if shift { b"\x1b[Z" } else { b"\t" });
    }
    LEGACY_SEQ_TABLE
        .iter()
        .find(|&&(s, _)| s == sym)
        .map(|&(_, seq)| seq)
}

/// Legacy control-character byte (Ctrl+letter and friends), or `None`.
fn ctrl_sequence(sym: u32) -> Option<Vec<u8>> {
    if (keysyms::KEY_a..=keysyms::KEY_z).contains(&sym) {
        return Some(vec![(sym - keysyms::KEY_a + 1) as u8]);
    }
    if (keysyms::KEY_A..=keysyms::KEY_Z).contains(&sym) {
        return Some(vec![(sym - keysyms::KEY_A + 1) as u8]);
    }
    Some(match sym {
        keysyms::KEY_space | keysyms::KEY_at => vec![0x00],
        keysyms::KEY_bracketleft => vec![0x1b],
        keysyms::KEY_backslash => vec![0x1c],
        keysyms::KEY_bracketright => vec![0x1d],
        keysyms::KEY_asciicircum => vec![0x1e],
        keysyms::KEY_underscore => vec![0x1f],
        _ => return None,
    })
}

/// Prepends the Alt escape prefix when the composer is active.
fn with_alt_prefix(mut bytes: Vec<u8>, alt: bool) -> Vec<u8> {
    if alt {
        bytes.insert(0, 0x1b);
    }
    bytes
}

/// CSI u rendering context derived once per key event.
struct CsiContext {
    mod_val: u32,
    effective_event_type: u8,
    report_types: bool,
    /// Colon-joined associated text (REPORT_ASSOCIATED_TEXT), if any.
    text_codepoints: Option<String>,
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
            current_group: 0,
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
        self.current_group = group;
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

    /// Encodes a key event per the Kitty keyboard protocol.
    pub fn encode_kitty_key(
        &self,
        sym: u32,
        keycode: Keycode,
        mods: Modifiers,
        event_type: u8,
    ) -> Option<Vec<u8>> {
        let key_format = kitty_key_format(sym)?;

        let flags = self.kitty_flags;
        // Standalone modifier key events must only be reported when
        // REPORT_ALL_KEYS_AS_ESC (flag 8) is active; mirrors Kitty's
        // `encode_glfw_key_event` early return.
        let all_keys = flags & KittyKeyboardFlags::REPORT_ALL_KEYS_AS_ESC != 0;
        if !all_keys && is_modifier_key(sym) {
            return None;
        }

        let report_types = flags & KittyKeyboardFlags::REPORT_EVENT_TYPES != 0;
        let effective_event_type = effective_event_type(report_types, event_type)?;

        if !is_csi_key(flags, key_format, mods) {
            return None;
        }

        let seq = self.format_kitty_sequence(
            key_format,
            &CsiContext {
                mod_val: modifier_value(mods),
                effective_event_type,
                report_types,
                text_codepoints: self.associated_text_codepoints(
                    keycode,
                    flags,
                    effective_event_type,
                ),
            },
            keycode,
            flags,
            mods,
        );
        Some(seq.into_bytes())
    }

    /// Renders the CSI u sequence for a resolved key format.
    fn format_kitty_sequence(
        &self,
        key_format: KittyKey,
        ctx: &CsiContext,
        keycode: Keycode,
        flags: u8,
        mods: Modifiers,
    ) -> String {
        match key_format {
            KittyKey::Letter(ch) => {
                format_letter(ch, ctx.mod_val, ctx.effective_event_type, ctx.report_types)
            }
            KittyKey::Tilde(num) => {
                format_tilde(num, ctx.mod_val, ctx.effective_event_type, ctx.report_types)
            }
            KittyKey::Unicode(codepoint) => {
                let first = self.alternate_key_field(keycode, codepoint, flags, mods);
                format_unicode(
                    first,
                    ctx.mod_val,
                    ctx.effective_event_type,
                    ctx.report_types,
                    ctx.text_codepoints.as_deref(),
                )
            }
        }
    }

    /// First CSI u field: plain codepoint, or `base:shifted` under
    /// REPORT_ALTERNATE_KEYS so the receiver can recover the shifted glyph.
    fn alternate_key_field(
        &self,
        keycode: Keycode,
        codepoint: u32,
        flags: u8,
        mods: Modifiers,
    ) -> String {
        let alternate = flags & KittyKeyboardFlags::REPORT_ALTERNATE_KEYS != 0;
        if alternate
            && mods.shift
            && let Some(base) = self.unshifted_codepoint(keycode)
            && base != codepoint
        {
            return format!("{base}:{codepoint}");
        }
        format!("{codepoint}")
    }

    /// REPORT_ASSOCIATED_TEXT (16) payload: press-associated text as a
    /// colon-joined list of codepoints for the current key, filtered to
    /// printable characters. `None` when the flag is off or no text exists.
    fn associated_text_codepoints(
        &self,
        keycode: Keycode,
        flags: u8,
        effective_event_type: u8,
    ) -> Option<String> {
        let report_text = flags & KittyKeyboardFlags::REPORT_ASSOCIATED_TEXT != 0;
        if !report_text || effective_event_type != 1 {
            return None;
        }
        let state = self.state.as_ref()?;
        let utf8 = state.key_get_utf8(keycode);
        let cps: Vec<String> = utf8
            .chars()
            .filter(|&c| c >= ' ' && c != '\x7f')
            .map(|c| (c as u32).to_string())
            .collect();
        if cps.is_empty() {
            None
        } else {
            Some(cps.join(":"))
        }
    }

    /// Returns the Unicode codepoint a key produces with no modifiers active,
    /// used to populate the `REPORT_ALTERNATE_KEYS` base field for shifted keys.
    ///
    /// Builds a throwaway `State` from the active keymap with an empty modifier
    /// mask, so the returned value reflects the key's base (unshifted) level.
    fn unshifted_codepoint(&self, keycode: Keycode) -> Option<u32> {
        let keymap = self.keymap.as_ref()?;
        let mut state = State::new(keymap);
        // Empty modifiers mask while preserving the active layout group.
        state.update_mask(0, 0, 0, 0, 0, self.current_group);
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
        let sym = state.key_get_one_sym(keycode).raw();

        let ctrl = state.mod_name_is_active(&xkb::MOD_NAME_CTRL, xkb::STATE_MODS_EFFECTIVE);
        let alt = state.mod_name_is_active(&xkb::MOD_NAME_ALT, xkb::STATE_MODS_EFFECTIVE);
        let shift = state.mod_name_is_active(&xkb::MOD_NAME_SHIFT, xkb::STATE_MODS_EFFECTIVE);

        // Functional navigation / control keys
        if let Some(bytes) = legacy_functional_seq(sym, shift) {
            return Some(with_alt_prefix(bytes.to_vec(), alt));
        }

        // Control key combinations
        if ctrl && let Some(bytes) = ctrl_sequence(sym) {
            return Some(with_alt_prefix(bytes, alt));
        }

        // Regular character output from xkb state
        let utf8 = state.key_get_utf8(keycode);
        if !utf8.is_empty() {
            return Some(with_alt_prefix(utf8.as_bytes().to_vec(), alt));
        }

        None
    }
}
