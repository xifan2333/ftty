//! Unicode display widths following the python-wcwidth `wcswidth()` specification.
//!
//! python-wcwidth deliberately extends POSIX `wcswidth(3)` with grapheme-aware rules for
//! emoji ZWJ sequences, variation selectors, regional indicators, Fitzpatrick modifiers,
//! spacing marks, and virama conjuncts. `ucs-detect` scores terminals against this model.

mod tables;

use tables::{
    AMBIGUOUS_EASTASIAN, CATEGORY_MC, EXTENDED_PICTOGRAPHIC, VIRAMA, VS15_WIDE_TO_NARROW,
    VS16_NARROW_TO_WIDE, WIDE_EASTASIAN, ZERO_WIDTH,
};

/// Width of East Asian Ambiguous characters in a Western context.
pub const AMBIGUOUS_NARROW: u8 = 1;
/// Width of East Asian Ambiguous characters in a CJK context.
pub const AMBIGUOUS_WIDE: u8 = 2;

const ZWJ: u32 = 0x200D;
const VS16: u32 = 0xFE0F;
const VS15: u32 = 0xFE0E;
const FITZPATRICK_START: u32 = 0x1F3FB;
const FITZPATRICK_END: u32 = 0x1F3FF;
const REGIONAL_INDICATOR_START: u32 = 0x1F1E6;
const REGIONAL_INDICATOR_END: u32 = 0x1F1FF;
/// Kitty's Unicode placeholder is protocol-defined as a single cell even in CJK mode.
pub const KITTY_PLACEHOLDER: u32 = 0x10EEEE;

fn in_table(ucs: u32, table: &[(u32, u32)]) -> bool {
    let mut lo = 0;
    let mut hi = table.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        let (start, end) = table[mid];
        if ucs < start {
            hi = mid;
        } else if ucs > end {
            lo = mid + 1;
        } else {
            return true;
        }
    }
    false
}

fn is_regional_indicator(ucs: u32) -> bool {
    (REGIONAL_INDICATOR_START..=REGIONAL_INDICATOR_END).contains(&ucs)
}

fn is_fitzpatrick(ucs: u32) -> bool {
    (FITZPATRICK_START..=FITZPATRICK_END).contains(&ucs)
}

fn is_emoji_zwj_base(ucs: u32) -> bool {
    is_regional_indicator(ucs) || in_table(ucs, EXTENDED_PICTOGRAPHIC)
}

fn is_virama(ucs: u32) -> bool {
    in_table(ucs, VIRAMA)
}

fn is_mc(ucs: u32) -> bool {
    in_table(ucs, CATEGORY_MC)
}

/// Width of a single code point, following the python-wcwidth `wcwidth()` rules.
///
/// Returns `-1` for C0/C1 controls, `0` for zero-width code points, and `1` or `2`
/// for printable cells. `ambiguous_width` is 1 by default or 2 for CJK contexts.
#[must_use]
pub fn wcwidth(c: char, ambiguous_width: u8) -> i32 {
    let ucs = c as u32;
    if (0x20..0x7f).contains(&ucs) {
        return 1;
    }
    if ucs != 0 && (ucs < 0x20 || (0x7f..0xa0).contains(&ucs)) {
        return -1;
    }
    if in_table(ucs, ZERO_WIDTH) {
        return 0;
    }
    if in_table(ucs, WIDE_EASTASIAN) {
        return 2;
    }
    if ambiguous_width == AMBIGUOUS_WIDE && in_table(ucs, AMBIGUOUS_EASTASIAN) {
        return 2;
    }
    1
}

/// Width of a string, following the python-wcwidth `wcswidth()` sequence rules.
///
/// Returns `-1` if a C0/C1 control appears outside a ZWJ-consumed position.
#[must_use]
pub fn wcswidth(text: &str, ambiguous_width: u8) -> i32 {
    if text.is_ascii() && text.bytes().all(|byte| (0x20..0x7f).contains(&byte)) {
        return text.len() as i32;
    }

    let chars: Vec<char> = text.chars().collect();
    let end = chars.len();
    let mut total = 0;
    let mut idx = 0;
    let mut last_measured_valid = false;
    let mut last_measured_ucs = 0;
    let mut last_measured_width = 0;
    let mut prev_was_virama = false;
    let mut cluster_width = 0;

    while idx < end {
        let ucs = chars[idx] as u32;

        if ucs == ZWJ {
            if prev_was_virama {
                idx += 1;
            } else if idx + 1 < end {
                last_measured_width = 0;
                prev_was_virama = false;
                idx += 2;
            } else {
                prev_was_virama = false;
                idx += 1;
            }
            continue;
        }

        if ucs == VS16 && last_measured_valid {
            if in_table(last_measured_ucs, VS16_NARROW_TO_WIDE) {
                cluster_width = 2;
            }
            last_measured_valid = false;
            idx += 1;
            continue;
        }

        if ucs == VS15 && last_measured_valid {
            if in_table(last_measured_ucs, VS15_WIDE_TO_NARROW) && last_measured_width == 2 {
                total -= 1;
            }
            last_measured_valid = false;
            idx += 1;
            continue;
        }

        if ucs > 0xFFFF {
            if is_regional_indicator(ucs) {
                let mut ri_before = 0;
                let mut j = idx;
                while j > 0 {
                    j -= 1;
                    if is_regional_indicator(chars[j] as u32) {
                        ri_before += 1;
                    } else {
                        break;
                    }
                }
                if ri_before % 2 == 1 {
                    last_measured_ucs = ucs;
                    idx += 1;
                    continue;
                }
            } else if is_fitzpatrick(ucs) && is_emoji_zwj_base(last_measured_ucs) {
                idx += 1;
                continue;
            }
        }

        let width = wcwidth(chars[idx], ambiguous_width);
        if width < 0 {
            return -1;
        }
        if width > 0 {
            if prev_was_virama {
                cluster_width = 2;
            } else if cluster_width > 0 {
                total += cluster_width;
                cluster_width = width;
            } else {
                cluster_width = width;
            }
            last_measured_valid = true;
            last_measured_ucs = ucs;
            last_measured_width = width;
            prev_was_virama = false;
        } else if is_virama(ucs) {
            prev_was_virama = true;
        } else if last_measured_valid && is_mc(ucs) {
            cluster_width = 2;
            last_measured_valid = false;
            prev_was_virama = false;
        } else {
            prev_was_virama = false;
        }
        idx += 1;
    }

    if cluster_width > 0 {
        total += cluster_width;
    }
    total
}

/// Action requested by [`WidthState::step`] for the next printed code point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidthAction {
    /// Start a new grapheme cluster with this cell width.
    Start { width: u32 },
    /// Join the current cluster (or start one) and set its width.
    StartOrJoin { width: u32 },
    /// Append to the current cluster without starting a new cell; width may change.
    Attach { width: u32 },
    /// Nothing visible should be emitted.
    Drop,
    /// The code point is a C0/C1 control.
    Control,
}

/// Incremental version of the python-wcwidth `wcswidth()` loop.
///
/// Terminal output arrives one code point at a time; this state machine exposes the
/// same cluster boundaries and width mutations so the grid can update the active cell
/// without buffering the whole string.
#[derive(Debug, Clone, Default)]
pub struct WidthState {
    cluster_width: i32,
    last_measured_valid: bool,
    last_measured_ucs: u32,
    last_measured_width: i32,
    prev_was_virama: bool,
    skip_next: bool,
    ri_run: usize,
}

impl WidthState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    #[must_use]
    pub fn skip_next(&self) -> bool {
        self.skip_next
    }

    #[must_use]
    pub fn prev_was_virama(&self) -> bool {
        self.prev_was_virama
    }

    /// Consume the code point immediately following a ZWJ.
    pub fn consume_skipped(&mut self) -> WidthAction {
        if !self.skip_next {
            return WidthAction::Drop;
        }
        self.skip_next = false;
        self.ri_run = 0;
        WidthAction::Attach {
            width: self.cluster_width.max(0) as u32,
        }
    }

    /// Observed a run of printable ASCII that bypassed per-character stepping.
    pub fn observe_ascii_run(&mut self, last: char) {
        self.cluster_width = 1;
        self.last_measured_valid = true;
        self.last_measured_ucs = last as u32;
        self.last_measured_width = 1;
        self.prev_was_virama = false;
        self.skip_next = false;
        self.ri_run = 0;
    }

    /// Process one printed code point and return the cluster operation to perform.
    pub fn step(&mut self, c: char, ambiguous_width: u8) -> WidthAction {
        let ucs = c as u32;
        let ri_before = self.ri_run;

        let action = if self.skip_next {
            self.skip_next = false;
            WidthAction::Attach {
                width: self.cluster_width.max(0) as u32,
            }
        } else if ucs == ZWJ {
            if self.prev_was_virama {
                WidthAction::Attach {
                    width: self.cluster_width.max(0) as u32,
                }
            } else {
                self.last_measured_width = 0;
                self.prev_was_virama = false;
                self.skip_next = true;
                WidthAction::Attach {
                    width: self.cluster_width.max(0) as u32,
                }
            }
        } else if ucs == VS16 && self.last_measured_valid {
            if in_table(self.last_measured_ucs, VS16_NARROW_TO_WIDE) {
                self.cluster_width = 2;
            }
            self.last_measured_valid = false;
            WidthAction::Attach {
                width: self.cluster_width.max(0) as u32,
            }
        } else if ucs == VS15 && self.last_measured_valid {
            if in_table(self.last_measured_ucs, VS15_WIDE_TO_NARROW)
                && self.last_measured_width == 2
            {
                self.cluster_width = 1;
            }
            self.last_measured_valid = false;
            WidthAction::Attach {
                width: self.cluster_width.max(0) as u32,
            }
        } else if is_regional_indicator(ucs) && ri_before % 2 == 1 {
            self.last_measured_ucs = ucs;
            WidthAction::Attach {
                width: self.cluster_width.max(0) as u32,
            }
        } else if is_fitzpatrick(ucs) && is_emoji_zwj_base(self.last_measured_ucs) {
            WidthAction::Attach {
                width: self.cluster_width.max(0) as u32,
            }
        } else {
            let width = if ucs == KITTY_PLACEHOLDER {
                1
            } else {
                wcwidth(c, ambiguous_width)
            };
            if width < 0 {
                self.reset();
                WidthAction::Control
            } else if width > 0 {
                if self.prev_was_virama {
                    self.cluster_width = 2;
                    self.last_measured_valid = true;
                    self.last_measured_ucs = ucs;
                    self.last_measured_width = width;
                    self.prev_was_virama = false;
                    WidthAction::StartOrJoin { width: 2 }
                } else {
                    self.cluster_width = width;
                    self.last_measured_valid = true;
                    self.last_measured_ucs = ucs;
                    self.last_measured_width = width;
                    self.prev_was_virama = false;
                    WidthAction::Start {
                        width: width as u32,
                    }
                }
            } else if is_virama(ucs) {
                self.prev_was_virama = true;
                WidthAction::Attach {
                    width: self.cluster_width.max(0) as u32,
                }
            } else if self.last_measured_valid && is_mc(ucs) {
                self.cluster_width = 2;
                self.last_measured_valid = false;
                self.prev_was_virama = false;
                WidthAction::Attach { width: 2 }
            } else {
                self.prev_was_virama = false;
                WidthAction::Attach {
                    width: self.cluster_width.max(0) as u32,
                }
            }
        };

        if is_regional_indicator(ucs) {
            self.ri_run += 1;
        } else {
            self.ri_run = 0;
        }
        action
    }
}
