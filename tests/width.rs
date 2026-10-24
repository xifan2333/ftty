#![allow(clippy::unwrap_used, clippy::expect_used)]

use ftty::width::{AMBIGUOUS_NARROW, AMBIGUOUS_WIDE, WidthAction, WidthState, wcswidth, wcwidth};

#[test]
fn single_codepoint_widths_match_python_wcwidth() {
    let cases = [
        ('\0', 0),
        ('A', 1),
        ('\u{7f}', -1),
        ('\u{0301}', 0),
        ('\u{093e}', 0),
        ('\u{00ad}', 1),
        ('\u{115f}', 2),
        ('\u{1160}', 0),
        ('\u{4e2d}', 2),
        ('\u{2764}', 1),
        ('\u{1f600}', 2),
        ('\u{1f1e6}', 2),
        ('\u{1f3fd}', 2),
        ('\u{302e}', 0),
        ('\u{3248}', 1),
        ('\u{17d8}', 1),
        ('\u{2d7f}', 0),
        ('\u{10eeee}', 1),
    ];
    for (c, expected) in cases {
        assert_eq!(wcwidth(c, AMBIGUOUS_NARROW), expected, "U+{:04X}", c as u32);
    }
    assert_eq!(wcwidth('\u{3248}', AMBIGUOUS_WIDE), 2);
    assert_eq!(wcwidth('\u{10eeee}', AMBIGUOUS_WIDE), 2);
}

#[test]
fn sequence_widths_match_python_wcswidth() {
    let cases = [
        ("A", 1),
        ("e\u{0301}", 1),
        ("\u{2764}", 1),
        ("\u{2764}\u{fe0f}", 2),
        ("\u{2764}\u{fe0e}", 1),
        ("\u{1f600}", 2),
        ("\u{1f600}\u{fe0e}", 2),
        ("\u{1f1e6}", 2),
        ("\u{1f1e6}\u{1f1e8}", 2),
        ("\u{1f1e6}\u{1f1e8}\u{1f1e6}", 4),
        ("\u{1f44d}", 2),
        ("\u{1f44d}\u{1f3fd}", 2),
        ("\u{1f3fd}", 2),
        (
            "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}",
            2,
        ),
        ("1\u{fe0f}\u{20e3}", 2),
        ("\u{0915}\u{093e}", 2),
        ("\u{0915}\u{093f}", 2),
        ("\u{0915}\u{094d}\u{0928}", 2),
        (
            "\u{0915}\u{094d}\u{0928}\u{094d}\u{0928}\u{094d}\u{0928}",
            2,
        ),
        // The reference implementation resets virama context on an intervening
        // nukta (Mn), so this three-consonant sequence measures three cells.
        (
            "\u{0915}\u{094d}\u{093c}\u{0928}\u{094d}\u{0928}\u{094d}\u{0928}",
            3,
        ),
        ("\u{4e2d}\u{6587}", 4),
        ("\u{00ad}", 1),
        ("\u{17d8}", 1),
        ("\u{2d7f}", 0),
        ("\u{302e}", 0),
    ];
    for (text, expected) in cases {
        assert_eq!(
            wcswidth(text, AMBIGUOUS_NARROW),
            expected,
            "{}",
            text.escape_unicode()
        );
    }
    assert_eq!(wcswidth("\u{3248}", AMBIGUOUS_WIDE), 2);
}

#[test]
fn controls_return_minus_one_except_after_zwj() {
    assert_eq!(wcswidth("a\nb", AMBIGUOUS_NARROW), -1);
    assert_eq!(wcswidth("\u{1b}[31m", AMBIGUOUS_NARROW), -1);
    assert_eq!(wcswidth("\u{200d}\n", AMBIGUOUS_NARROW), 0);
}

#[test]
fn width_state_tracks_cluster_boundaries() {
    let mut state = WidthState::new();
    assert_eq!(state.step('\u{2764}', 1), WidthAction::Start { width: 1 });
    assert_eq!(state.step('\u{fe0f}', 1), WidthAction::Attach { width: 2 });
    assert_eq!(state.step('\u{1f44d}', 1), WidthAction::Start { width: 2 });
    assert_eq!(state.step('\u{1f3fd}', 1), WidthAction::Attach { width: 2 });
    state.reset();
    assert_eq!(state.step('\u{0915}', 1), WidthAction::Start { width: 1 });
    assert_eq!(state.step('\u{093e}', 1), WidthAction::Attach { width: 2 });
}

fn hex_to_string(hex: &str) -> String {
    let bytes: Vec<u8> = hex
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let text = std::str::from_utf8(pair).unwrap();
            u8::from_str_radix(text, 16).unwrap()
        })
        .collect();
    String::from_utf8(bytes).unwrap()
}

#[test]
fn generated_reference_corpus_matches_python_wcswidth() {
    let corpus = include_str!("data/width_cases.txt");
    let mut checked = 0;
    for line in corpus.lines() {
        let (hex, expected) = line.split_once('\t').unwrap();
        let text = hex_to_string(hex);
        let expected: i32 = expected.parse().unwrap();
        assert_eq!(
            wcswidth(&text, AMBIGUOUS_NARROW),
            expected,
            "{}",
            text.escape_unicode()
        );
        checked += 1;
    }
    assert!(checked > 4000, "corpus unexpectedly small: {checked}");
}
