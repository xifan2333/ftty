//! Unicode display width using standard `unicode-width` and `unicode-segmentation` crates.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const AMBIGUOUS_NARROW: u8 = 1;
pub const AMBIGUOUS_WIDE: u8 = 2;
pub const KITTY_PLACEHOLDER: char = '\u{10EEEE}';

/// Width of a single character in terminal columns.
#[must_use]
pub fn wcwidth(c: char, ambiguous_width: u8) -> i32 {
    let ucs = c as u32;
    if ucs == 0 {
        return 0;
    }
    if ucs < 0x20 || (0x7F..0xA0).contains(&ucs) {
        return -1;
    }
    if c == KITTY_PLACEHOLDER {
        return 1;
    }
    let width = if ambiguous_width == AMBIGUOUS_WIDE {
        c.width_cjk()
    } else {
        c.width()
    };
    width.map(|w| w as i32).unwrap_or(-1)
}

/// Width of a string in terminal columns.
#[must_use]
pub fn wcswidth(text: &str, ambiguous_width: u8) -> i32 {
    if text.is_ascii() && text.bytes().all(|b| (0x20..0x7F).contains(&b)) {
        return text.len() as i32;
    }
    for c in text.chars() {
        let ucs = c as u32;
        if ucs != 0 && (ucs < 0x20 || (0x7F..0xA0).contains(&ucs)) && ucs != 0x200D {
            return -1;
        }
    }
    let width = if ambiguous_width == AMBIGUOUS_WIDE {
        text.width_cjk()
    } else {
        text.width()
    };
    width as i32
}

/// Returns `true` if appending `c` to `prev_cluster` continues the same extended grapheme cluster.
#[must_use]
pub fn continues_grapheme(prev_cluster: &str, c: char, scratch: &mut String) -> bool {
    scratch.clear();
    scratch.push_str(prev_cluster);
    scratch.push(c);
    scratch.graphemes(true).count() == 1
}

/// Computes the display width of a complete grapheme cluster in terminal cells (clamped to 1..=2).
#[must_use]
pub fn grapheme_display_width(cluster: &str, ambiguous_width: u8) -> usize {
    let width = if ambiguous_width == AMBIGUOUS_WIDE {
        cluster.width_cjk()
    } else {
        cluster.width()
    };
    width.clamp(1, 2)
}
