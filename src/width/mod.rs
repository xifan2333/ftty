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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_codepoint_widths() {
        let cases = [
            ('\0', 0),
            ('A', 1),
            ('\u{7f}', -1),
            ('\u{0301}', 0),
            ('\u{115f}', 2),
            ('\u{1160}', 0),
            ('\u{4e2d}', 2),
            ('\u{2764}', 1),
            ('\u{1f600}', 2),
            ('\u{1f1e6}', 1),
            ('\u{1f3fd}', 2),
            ('\u{10eeee}', 1),
        ];
        for (c, expected) in cases {
            assert_eq!(wcwidth(c, AMBIGUOUS_NARROW), expected, "U+{:04X}", c as u32);
        }
        assert_eq!(wcwidth('\u{4e2d}', AMBIGUOUS_WIDE), 2);
        assert_eq!(wcwidth('\u{10eeee}', AMBIGUOUS_WIDE), 1);
    }

    #[test]
    fn sequence_widths_match_standard_unicode() {
        let cases = [
            ("A", 1),
            ("e\u{0301}", 1),
            ("\u{2764}", 1),
            ("\u{2764}\u{fe0f}", 2),
            ("\u{1f600}", 2),
            ("\u{1f1e6}\u{1f1e8}", 2),
            ("\u{1f44d}", 2),
            ("\u{1f44d}\u{1f3fd}", 2),
            (
                "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}",
                2,
            ),
            ("1\u{fe0f}\u{20e3}", 2),
            ("\u{0915}\u{093e}", 2),
            ("\u{4e2d}\u{6587}", 4),
        ];
        for (text, expected) in cases {
            assert_eq!(
                wcswidth(text, AMBIGUOUS_NARROW),
                expected,
                "{}",
                text.escape_unicode()
            );
        }
    }

    #[test]
    fn controls_return_minus_one() {
        assert_eq!(wcswidth("a\nb", AMBIGUOUS_NARROW), -1);
        assert_eq!(wcswidth("\u{1b}[31m", AMBIGUOUS_NARROW), -1);
    }

    #[test]
    fn grapheme_continuity_and_cluster_display_width() {
        let mut scratch = String::new();
        assert!(continues_grapheme("e", '\u{0301}', &mut scratch));
        assert_eq!(grapheme_display_width("e\u{0301}", AMBIGUOUS_NARROW), 1);

        assert!(continues_grapheme("❤", '\u{fe0f}', &mut scratch));
        assert_eq!(grapheme_display_width("❤\u{fe0f}", AMBIGUOUS_NARROW), 2);

        assert!(continues_grapheme("👨", '\u{200d}', &mut scratch));
        let mut family = String::from("👨\u{200d}");
        assert!(continues_grapheme(&family, '👩', &mut scratch));
        family.push('👩');
        assert_eq!(grapheme_display_width(&family, AMBIGUOUS_NARROW), 2);

        assert!(continues_grapheme("🇺", '🇸', &mut scratch));
        assert_eq!(grapheme_display_width("🇺🇸", AMBIGUOUS_NARROW), 2);

        assert!(continues_grapheme("👍", '\u{1f3fd}', &mut scratch));
        assert_eq!(grapheme_display_width("👍\u{1f3fd}", AMBIGUOUS_NARROW), 2);
    }
}
