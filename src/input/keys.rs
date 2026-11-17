//! Key actions, modifier state, and xkb keysym/combination parsing.

use xkbcommon::xkb::{self, keysyms};

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

const NAMED_KEYS: &[(&[&str], u32)] = &[
    (&["pageup", "page_up"], keysyms::KEY_Page_Up),
    (&["pagedown", "page_down"], keysyms::KEY_Page_Down),
    (&["kp_pageup", "kp_page_up"], keysyms::KEY_KP_Page_Up),
    (&["kp_pagedown", "kp_page_down"], keysyms::KEY_KP_Page_Down),
    (&["home"], keysyms::KEY_Home),
    (&["end"], keysyms::KEY_End),
    (&["up"], keysyms::KEY_Up),
    (&["down"], keysyms::KEY_Down),
    (&["left"], keysyms::KEY_Left),
    (&["right"], keysyms::KEY_Right),
    (&["insert"], keysyms::KEY_Insert),
    (&["+", "plus", "kp_add"], keysyms::KEY_plus),
    (&["=", "equal"], keysyms::KEY_equal),
    (&["-", "minus", "kp_subtract"], keysyms::KEY_minus),
    (&["0", "kp_0"], keysyms::KEY_0),
];

fn lookup_well_known_key(name: &str) -> Option<xkb::Keysym> {
    for &(aliases, sym) in NAMED_KEYS {
        if aliases.contains(&name) {
            return Some(xkb::Keysym::new(sym));
        }
    }
    None
}

fn split_key_combo(s: &str) -> (&str, &str) {
    if let Some(prefix) = s.strip_suffix("++") {
        (prefix, "+")
    } else {
        match s.rfind('+') {
            Some(idx) => (&s[..idx], &s[idx + 1..]),
            None => ("", s),
        }
    }
}

fn parse_modifiers(mod_part: &str) -> Option<Modifiers> {
    if mod_part.is_empty() {
        return Some(Modifiers::default());
    }

    let mut mods = Modifiers::default();
    for part in mod_part.split('+') {
        match part.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods.ctrl = true,
            "alt" | "mod1" => mods.alt = true,
            "shift" => mods.shift = true,
            "super" | "mod4" | "logo" => mods.logo = true,
            _ => return None,
        }
    }

    Some(mods)
}

fn parse_keysym(key_str: &str) -> Option<xkb::Keysym> {
    let lower = key_str.trim().to_ascii_lowercase();
    if let Some(sym) = lookup_well_known_key(&lower) {
        return Some(sym);
    }

    let sym = if lower.len() == 1 {
        let ch = lower.chars().next()?;
        xkb::utf32_to_keysym(ch as u32)
    } else {
        xkb::keysym_from_name(&lower, xkb::KEYSYM_CASE_INSENSITIVE)
    };

    if sym == xkb::Keysym::new(keysyms::KEY_NoSymbol) {
        None
    } else {
        Some(sym)
    }
}

/// Parses a key combination string (e.g. `"Shift+PageUp"` or `"Ctrl+Shift+C"`).
#[must_use]
pub fn parse_key_combo(s: &str) -> Option<(Modifiers, xkb::Keysym)> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("none") {
        return None;
    }

    let (mod_part, key_str) = split_key_combo(s);
    let modifiers = parse_modifiers(mod_part)?;
    let sym = parse_keysym(key_str)?;

    Some((modifiers, sym))
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

/// Checks if a keysym corresponds to a modifier key (Shift, Ctrl, Alt, Super, Hyper, Meta, ISO Level Shift, Locks).
/// Aligns with Kitty `is_modifier_key` in `kitty/keys.c`.
#[must_use]
pub fn is_modifier_key(sym: u32) -> bool {
    matches!(
        sym,
        keysyms::KEY_Shift_L
            | keysyms::KEY_Shift_R
            | keysyms::KEY_Control_L
            | keysyms::KEY_Control_R
            | keysyms::KEY_Alt_L
            | keysyms::KEY_Alt_R
            | keysyms::KEY_Super_L
            | keysyms::KEY_Super_R
            | keysyms::KEY_Hyper_L
            | keysyms::KEY_Hyper_R
            | keysyms::KEY_Meta_L
            | keysyms::KEY_Meta_R
            | keysyms::KEY_ISO_Level3_Shift
            | keysyms::KEY_ISO_Level5_Shift
            | keysyms::KEY_Caps_Lock
            | keysyms::KEY_Scroll_Lock
            | keysyms::KEY_Num_Lock
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_key_combo_modifiers() {
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

        let (mods, sym) = parse_key_combo("Ctrl++").unwrap();
        assert!(mods.ctrl);
        assert_eq!(sym, xkb::Keysym::new(keysyms::KEY_plus));

        let (mods, sym) = parse_key_combo("Alt+Mod4+Return").unwrap();
        assert!(mods.alt);
        assert!(mods.logo);
        assert_eq!(sym, xkb::Keysym::new(keysyms::KEY_Return));

        let (mods, sym) = parse_key_combo("Super+Mod1+Space").unwrap();
        assert!(mods.alt);
        assert!(mods.logo);
        assert_eq!(sym, xkb::Keysym::new(keysyms::KEY_space));

        assert!(parse_key_combo("none").is_none());
        assert!(parse_key_combo("").is_none());
        assert!(parse_key_combo("UnknownMod+C").is_none());
    }
}
