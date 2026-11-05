//! Mouse protocol modes and wire encodings for pointer reports.

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
