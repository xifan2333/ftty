//! Mouse tracking modes negotiated over DECSET/DECRST and mouse report encoding.

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
}

impl MouseState {
    /// Applies a DECSET (`enabled`) or DECRST private mode. Returns `true` when the
    /// mode is a mouse mode owned by this state.
    pub fn apply_private_mode(&mut self, mode: u16, enabled: bool) -> bool {
        match mode {
            1000 => {
                self.tracking = if enabled {
                    MouseTracking::Click
                } else {
                    MouseTracking::Disabled
                }
            }
            1002 => {
                self.tracking = if enabled {
                    MouseTracking::Drag
                } else {
                    MouseTracking::Disabled
                }
            }
            1003 => {
                self.tracking = if enabled {
                    MouseTracking::Motion
                } else {
                    MouseTracking::Disabled
                }
            }
            1005 => {
                self.encoding = if enabled {
                    MouseEncoding::Utf8
                } else {
                    MouseEncoding::X10
                }
            }
            1006 => {
                self.encoding = if enabled {
                    MouseEncoding::Sgr
                } else {
                    MouseEncoding::X10
                }
            }
            1015 => {
                self.encoding = if enabled {
                    MouseEncoding::Urxvt
                } else {
                    MouseEncoding::X10
                }
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
        MouseEncoding::Urxvt => Some(format!("\x1b[{};{x};{y}M", code + 32).into_bytes()),
        MouseEncoding::Utf8 => {
            let mut out = b"\x1b[M".to_vec();
            for value in [code + 32, x + 32, y + 32] {
                push_utf8(&mut out, value);
            }
            Some(out)
        }
        MouseEncoding::X10 => {
            // The legacy encoding has no way to say which button was released.
            let code = if pressed || motion { code } else { 3 };
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

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
