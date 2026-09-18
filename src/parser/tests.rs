use std::path::Path;

use crate::color::{Color, Rgb};
use crate::grid::CellFlags;
use crate::parser::{MAX_HYPERLINKS, ProgressState, ShellIntegrationState, Terminal};

#[test]
fn test_print_and_cursor_movement() {
    let mut term = Terminal::new(80, 24, 100);
    term.advance_bytes(b"Hello, World!\r\nSecond line");

    assert_eq!(term.grid.lines[0].cells[0].c, 'H');
    assert_eq!(term.grid.lines[0].cells[12].c, '!');
    assert_eq!(term.grid.lines[1].cells[0].c, 'S');
    assert_eq!(term.grid.cursor.row, 1);
    assert_eq!(term.grid.cursor.col, 11);
}

#[test]
fn test_sgr_formatting() {
    let mut term = Terminal::new(80, 24, 100);
    // Set bold, red fg (31), truecolor bg (48;2;10;20;30)
    term.advance_bytes(b"\x1b[1;31;48;2;10;20;30mX\x1b[0m");

    let cell = term.grid.lines[0].cells[0];
    assert_eq!(cell.c, 'X');
    assert!(cell.flags.contains(CellFlags::BOLD));
    assert_eq!(cell.fg, Color::Indexed(1));
    assert_eq!(cell.bg, Color::Rgb(10, 20, 30));

    // After reset
    assert_eq!(term.active_fg, Color::DefaultForeground);
    assert_eq!(term.active_bg, Color::DefaultBackground);
    assert_eq!(term.active_flags, CellFlags::empty());
}

#[test]
fn test_osc_title() {
    let mut term = Terminal::new(80, 24, 100);
    term.advance_bytes(b"\x1b]0;ftty terminal\x07");
    assert_eq!(term.title, "ftty terminal");
}

#[test]
fn window_geometry_queries_are_answered_in_height_width_order() {
    let mut term = Terminal::new(80, 24, 100);
    term.set_geometry([9, 18], [720, 432]);
    term.advance_bytes(b"\x1b[14t\x1b[16t\x1b[18t");

    let responses = term.take_responses();
    assert_eq!(
        responses,
        vec![
            b"\x1b[4;432;720t".to_vec(),
            b"\x1b[6;18;9t".to_vec(),
            b"\x1b[8;24;80t".to_vec(),
        ]
    );
    // Draining resets the queue so replies are written exactly once.
    assert!(term.take_responses().is_empty());

    // Unrelated window manipulations stay silent.
    term.advance_bytes(b"\x1b[22;0t\x1b[23;0t");
    assert!(term.take_responses().is_empty());
}

#[test]
fn test_mouse_tracking_modes() {
    use crate::input::mouse::{MouseEncoding, MouseTracking};

    let mut term = Terminal::new(80, 24, 100);
    assert!(!term.mouse.is_reporting());

    term.advance_bytes(b"\x1b[?1002h\x1b[?1006h");
    assert_eq!(term.mouse.tracking, MouseTracking::Drag);
    assert_eq!(term.mouse.encoding, MouseEncoding::Sgr);
    assert!(term.mouse.is_reporting());

    // Bundled private modes must all be applied.
    term.advance_bytes(b"\x1b[?1002l\x1b[?1006l");
    assert!(!term.mouse.is_reporting());
    assert_eq!(term.mouse.encoding, MouseEncoding::X10);

    term.advance_bytes(b"\x1b[?1000;1006h");
    assert_eq!(term.mouse.tracking, MouseTracking::Click);
    assert_eq!(term.mouse.encoding, MouseEncoding::Sgr);

    // A full reset returns to local selection.
    term.advance_bytes(b"\x1bc");
    assert!(!term.mouse.is_reporting());
    assert_eq!(term.mouse.encoding, MouseEncoding::X10);
}

#[test]
fn test_cursor_visibility_does_not_disturb_mouse_modes() {
    let mut term = Terminal::new(80, 24, 100);
    term.advance_bytes(b"\x1b[?1000h\x1b[?25l");
    assert!(term.mouse.is_reporting());
    assert!(!term.grid.cursor.visible);
}

#[test]
fn test_clear_screen_csi() {
    let mut term = Terminal::new(80, 24, 100);
    term.advance_bytes(b"Testing\x1b[2J");
    assert_eq!(term.grid.lines[0].cells[0].c, ' ');
}

#[test]
fn test_device_status_and_attributes_queries() {
    let mut term = Terminal::new(80, 24, 100);

    // CSI 5 n -> Operating status (DSR)
    term.advance_bytes(b"\x1b[5n");
    assert_eq!(term.take_responses(), vec![b"\x1b[0n".to_vec()]);

    // CSI 6 n -> Cursor position report (CPR)
    term.advance_bytes(b"\x1b[10;20H\x1b[6n");
    assert_eq!(term.take_responses(), vec![b"\x1b[10;20R".to_vec()]);

    // Primary DA: CSI c
    term.advance_bytes(b"\x1b[c");
    assert_eq!(term.take_responses(), vec![b"\x1b[?62;c".to_vec()]);

    // Secondary DA: CSI > c
    term.advance_bytes(b"\x1b[>c");
    assert_eq!(term.take_responses(), vec![b"\x1b[>0;10;1c".to_vec()]);
}

#[test]
fn test_osc_color_queries_use_configured_defaults() {
    let mut term = Terminal::new(80, 24, 100);
    term.set_default_colors(Rgb::new(255, 128, 0), Rgb::new(10, 20, 30));

    term.advance_bytes(b"\x1b]10;?\x07");
    assert_eq!(
        term.take_responses(),
        vec![b"\x1b]10;rgb:ffff/8080/0000\x1b\\".to_vec()]
    );

    term.advance_bytes(b"\x1b]11;?\x07");
    assert_eq!(
        term.take_responses(),
        vec![b"\x1b]11;rgb:0a0a/1414/1e1e\x1b\\".to_vec()]
    );
}

#[test]
fn test_bracketed_paste_mode_toggle() {
    let mut term = Terminal::new(80, 24, 100);
    assert!(!term.bracketed_paste);

    term.advance_bytes(b"\x1b[?2004h");
    assert!(term.bracketed_paste);

    term.advance_bytes(b"\x1b[?2004l");
    assert!(!term.bracketed_paste);

    // Bundled private modes
    term.advance_bytes(b"\x1b[?25;2004h");
    assert!(term.bracketed_paste);
    assert!(term.grid.cursor.visible);

    // Full reset disables bracketed paste
    term.advance_bytes(b"\x1bc");
    assert!(!term.bracketed_paste);
}

#[test]
fn test_focus_reporting_mode_toggle() {
    let mut term = Terminal::new(80, 24, 100);
    assert!(!term.focus_reporting);

    term.advance_bytes(b"\x1b[?1004h");
    assert!(term.focus_reporting);

    term.advance_bytes(b"\x1b[?1004l");
    assert!(!term.focus_reporting);

    // Full reset disables focus reporting
    term.advance_bytes(b"\x1b[?1004h\x1bc");
    assert!(!term.focus_reporting);
}

#[test]
fn test_synchronized_output_and_decrqm_queries() {
    let mut term = Terminal::new(80, 24, 100);
    assert!(!term.synchronized_output);

    // Query mode 2026 before setting: disabled (2)
    term.advance_bytes(b"\x1b[?2026$p");
    assert_eq!(term.take_responses(), vec![b"\x1b[?2026;2$y".to_vec()]);

    // Enable mode 2026
    term.advance_bytes(b"\x1b[?2026h");
    assert!(term.synchronized_output);

    // Repeated enable must increment generation to refresh timeout
    let gen1 = term.sync_output_gen;
    term.advance_bytes(b"\x1b[?2026h");
    assert_eq!(term.sync_output_gen, gen1.wrapping_add(1));

    // Query mode 2026 after setting: enabled (1)
    term.advance_bytes(b"\x1b[?2026$p");
    assert_eq!(term.take_responses(), vec![b"\x1b[?2026;1$y".to_vec()]);

    // Disable mode 2026
    term.advance_bytes(b"\x1b[?2026l");
    assert!(!term.synchronized_output);

    // Query unrecognized mode -> 0
    term.advance_bytes(b"\x1b[?9999$p");
    assert_eq!(term.take_responses(), vec![b"\x1b[?9999;0$y".to_vec()]);
}

#[test]
fn test_osc_52_clipboard_read_and_write() {
    let mut term = Terminal::new(80, 24, 100);

    // Query empty clipboard
    term.advance_bytes(b"\x1b]52;c;?\x07");
    assert_eq!(term.take_responses(), vec![b"\x1b]52;c;\x1b\\".to_vec()]);

    // Write "hello world" (aGVsbG8gd29ybGQ=)
    term.advance_bytes(b"\x1b]52;c;aGVsbG8gd29ybGQ=\x07");
    assert_eq!(term.clipboard_content.as_deref(), Some("hello world"));
    assert_eq!(
        term.take_pending_clipboard(),
        Some(Some("hello world".to_string()))
    );

    // Query populated clipboard
    term.advance_bytes(b"\x1b]52;c;?\x1b\\");
    assert_eq!(
        term.take_responses(),
        vec![b"\x1b]52;c;aGVsbG8gd29ybGQ=\x1b\\".to_vec()]
    );

    // Clear clipboard
    term.advance_bytes(b"\x1b]52;c;\x07");
    assert_eq!(term.clipboard_content, None);
    assert_eq!(term.take_pending_clipboard(), Some(None));
}

#[test]
fn test_osc_7_current_working_directory() {
    let mut term = Terminal::new(80, 24, 100);
    assert_eq!(term.current_dir, None);

    // Typical file URI with hostname
    term.advance_bytes(b"\x1b]7;file://archlinux/home/user/projects\x07");
    assert_eq!(
        term.current_dir.as_deref(),
        Some(Path::new("/home/user/projects"))
    );

    // Percent-encoded spaces and UTF-8
    term.advance_bytes(b"\x1b]7;file:///home/user/my%20documents\x1b\\");
    assert_eq!(
        term.current_dir.as_deref(),
        Some(Path::new("/home/user/my documents"))
    );

    // Full reset clears current dir
    term.advance_bytes(b"\x1bc");
    assert_eq!(term.current_dir, None);
}

#[test]
fn test_osc_133_shell_integration() {
    let mut term = Terminal::new(80, 24, 100);
    assert_eq!(term.shell_integration, None);

    term.advance_bytes(b"\x1b]133;A\x07");
    assert_eq!(
        term.shell_integration,
        Some(ShellIntegrationState::PromptStart)
    );
    assert!(term.grid.prompt_marks.contains(&0));

    term.advance_bytes(b"\x1b]133;B\x07");
    assert_eq!(
        term.shell_integration,
        Some(ShellIntegrationState::CommandStart)
    );

    term.advance_bytes(b"\x1b]133;C\x07");
    assert_eq!(
        term.shell_integration,
        Some(ShellIntegrationState::OutputStart)
    );

    term.advance_bytes(b"\x1b]133;D;0\x07");
    assert_eq!(
        term.shell_integration,
        Some(ShellIntegrationState::CommandFinished(Some(0)))
    );

    term.advance_bytes(b"\x1b]133;D;127\x1b\\");
    assert_eq!(
        term.shell_integration,
        Some(ShellIntegrationState::CommandFinished(Some(127)))
    );

    // Full reset clears shell integration
    term.advance_bytes(b"\x1bc");
    assert_eq!(term.shell_integration, None);
}

#[test]
fn test_mode_2031_color_scheme_report() {
    let mut term = Terminal::new(80, 24, 100);
    // Default bg is (24, 24, 24) which is dark -> 1
    term.advance_bytes(b"\x1b[?2031h");
    assert!(term.report_color_scheme);
    assert_eq!(term.take_responses(), vec![b"\x1b[?2031;1$y".to_vec()]);

    // Switching to light background reports 2
    term.set_default_colors(Rgb::new(0, 0, 0), Rgb::new(240, 240, 240));
    assert_eq!(term.take_responses(), vec![b"\x1b[?2031;2$y".to_vec()]);

    // DECRQM query
    term.advance_bytes(b"\x1b[?2031$p");
    assert_eq!(term.take_responses(), vec![b"\x1b[?2031;1$y".to_vec()]);

    term.advance_bytes(b"\x1b[?2031l");
    assert!(!term.report_color_scheme);
}

#[test]
fn test_mode_2048_window_size_notifications() {
    let mut term = Terminal::new(80, 24, 100);
    term.set_geometry([10, 20], [800, 600]);

    term.advance_bytes(b"\x1b[?2048h");
    assert!(term.report_window_size);
    assert_eq!(
        term.take_responses(),
        vec![b"\x1b[48;24;80;600;800t".to_vec()]
    );

    // Resizing geometry emits notification
    term.set_geometry([10, 20], [1024, 768]);
    assert_eq!(
        term.take_responses(),
        vec![b"\x1b[48;24;80;768;1024t".to_vec()]
    );

    // Grid dimension change (e.g. font zoom) with constant viewport pixels emits updated character counts
    term.grid.resize(100, 30);
    term.set_geometry([8, 16], [1024, 768]);
    assert_eq!(
        term.take_responses(),
        vec![b"\x1b[48;30;100;768;1024t".to_vec()]
    );

    term.advance_bytes(b"\x1b[?2048l");
    assert!(!term.report_window_size);
}

#[test]
fn test_osc_9_4_progress_reporting() {
    let mut term = Terminal::new(80, 24, 100);
    assert_eq!(term.progress, None);

    // State 1: normal, 45%
    term.advance_bytes(b"\x1b]9;4;1;45\x07");
    assert_eq!(term.progress, Some(ProgressState::Normal(45)));

    // State 2: error, 75%
    term.advance_bytes(b"\x1b]9;4;2;75\x1b\\");
    assert_eq!(term.progress, Some(ProgressState::Error(75)));

    // State 3: indeterminate
    term.advance_bytes(b"\x1b]9;4;3\x07");
    assert_eq!(term.progress, Some(ProgressState::Indeterminate));

    // State 4: warning, 90%
    term.advance_bytes(b"\x1b]9;4;4;90\x07");
    assert_eq!(term.progress, Some(ProgressState::Warning(90)));

    // State 0: clear
    term.advance_bytes(b"\x1b]9;4;0\x07");
    assert_eq!(term.progress, None);

    // Full reset clears progress
    term.advance_bytes(b"\x1b]9;4;1;50\x07\x1bc");
    assert_eq!(term.progress, None);
}

#[test]
fn test_styled_underlines_and_underline_color() {
    let mut term = Terminal::new(80, 24, 100);

    // Undercurl (4:3) with RGB underline color (58;2;255;0;128)
    term.advance_bytes(b"\x1b[4:3;58;2;255;0;128mU\x1b[0m");
    let cell = term.grid.lines[0].cells[0];
    assert_eq!(cell.c, 'U');
    assert!(cell.flags.contains(CellFlags::UNDERLINE));
    assert!(cell.flags.contains(CellFlags::UNDERLINE_CURLY));
    assert_eq!(cell.underline_color, Color::Rgb(255, 0, 128));

    // Double underline (4:2)
    term.advance_bytes(b"\x1b[4:2mD\x1b[0m");
    let cell_d = term.grid.lines[0].cells[1];
    assert_eq!(cell_d.c, 'D');
    assert!(cell_d.flags.contains(CellFlags::UNDERLINE_DOUBLE));

    // Reset underline (24) and reset color (59)
    term.advance_bytes(b"\x1b[4:4;58;5;12mX\x1b[24;59mY\x1b[0m");
    let cell_x = term.grid.lines[0].cells[2];
    assert!(cell_x.flags.contains(CellFlags::UNDERLINE_DOTTED));
    assert_eq!(cell_x.underline_color, Color::Indexed(12));

    let cell_y = term.grid.lines[0].cells[3];
    assert!(!cell_y.flags.contains(CellFlags::UNDERLINE));
    assert_eq!(cell_y.underline_color, Color::DefaultForeground);
}

#[test]
fn test_osc_8_hyperlinks() {
    let mut term = Terminal::new(80, 24, 100);

    // Emit text with hyperlink
    term.advance_bytes(b"\x1b]8;id=link1;https://example.com\x07Click\x1b]8;;\x07 here");

    // "Click" should have the hyperlink ID
    for i in 0..5 {
        let cell = term.grid.lines[0].cells[i];
        assert!(cell.hyperlink_id.is_some());
        let id = cell.hyperlink_id.unwrap().get();
        assert_eq!(term.hyperlink_url(id), Some("https://example.com"));
    }

    // " here" should have None
    for i in 5..10 {
        let cell = term.grid.lines[0].cells[i];
        assert_eq!(cell.hyperlink_id, None);
    }

    // Full reset clears active hyperlink, pool and grid references
    term.advance_bytes(b"\x1b]8;;https://another.com\x07\x1bc");
    assert_eq!(term.active_hyperlink, None);
    assert_eq!(term.grid.lines[0].cells[0].hyperlink_id, None);

    // Test hyperlink clearing across alternate screen switch
    let mut alt_term = Terminal::new(80, 24, 100);
    alt_term.advance_bytes(b"\x1b]8;;https://foo.bar\x07Link\x1b]8;;\x07");
    assert!(alt_term.grid.lines[0].cells[0].hyperlink_id.is_some());
    // Enter alt screen, execute RIS, exit alt screen
    alt_term.advance_bytes(b"\x1b[?1049h\x1bc\x1b[?1049l");
    assert_eq!(alt_term.grid.lines[0].cells[0].hyperlink_id, None);

    // When pool reaches MAX_HYPERLINKS, FIFO eviction ensures new URLs continue to be interned
    let mut full_term = Terminal::new(80, 24, 100);
    let first_id = full_term.get_or_intern_hyperlink("https://first.com".to_string());
    for i in 1..MAX_HYPERLINKS {
        let _ = full_term.get_or_intern_hyperlink(format!("https://unique-{i}.com"));
    }
    assert_eq!(full_term.hyperlink_url(first_id), Some("https://first.com"));

    // Overflow: 1025th URL evicts the oldest entry (first_id)
    full_term.advance_bytes(b"\x1b]8;;https://overflow.com\x07Overflow\x1b]8;;\x07");
    let cell = full_term.grid.lines[0].cells[0];
    assert!(cell.hyperlink_id.is_some());
    let overflow_id = cell.hyperlink_id.unwrap().get();
    assert_eq!(
        full_term.hyperlink_url(overflow_id),
        Some("https://overflow.com")
    );
    // The evicted URL now safely resolves to None without aliasing
    assert_eq!(full_term.hyperlink_url(first_id), None);
}

#[test]
fn test_kitty_keyboard_protocol_negotiation() {
    let mut term = Terminal::new(80, 24, 100);

    // Query initial flags: 0
    term.advance_bytes(b"\x1b[?u");
    assert_eq!(term.take_responses(), vec![b"\x1b[?0u".to_vec()]);

    // Push flags = 3 via CSI > 3 u
    term.advance_bytes(b"\x1b[>3u");
    assert_eq!(term.kitty_keyboard_flags, 3);
    assert_eq!(term.take_pending_kitty_keyboard(), Some((3, 1)));

    // Query flags again: 3
    term.advance_bytes(b"\x1b[?u");
    assert_eq!(term.take_responses(), vec![b"\x1b[?3u".to_vec()]);

    // Pop flags via CSI < 1 u
    term.advance_bytes(b"\x1b[<1u");
    assert_eq!(term.kitty_keyboard_flags, 0);
    assert_eq!(term.take_pending_kitty_keyboard(), Some((0, 1)));

    // Mode 2 (union) via CSI = 2 ; 2 u
    term.advance_bytes(b"\x1b[=2;2u");
    assert_eq!(term.kitty_keyboard_flags, 2);

    // Mode 3 (difference) via CSI = 2 ; 3 u
    term.advance_bytes(b"\x1b[=2;3u");
    assert_eq!(term.kitty_keyboard_flags, 0);

    // Popping empty stack resets to 0
    term.advance_bytes(b"\x1b[=1;1u\x1b[<5u");
    assert_eq!(term.kitty_keyboard_flags, 0);

    // Full reset clears flags and stack
    term.advance_bytes(b"\x1b[>3u\x1bc");
    assert_eq!(term.kitty_keyboard_flags, 0);
    assert_eq!(term.kitty_keyboard_stack.len(), 0);
}

#[test]
fn test_combined_underline_and_bold_does_not_corrupt() {
    let mut term = Terminal::new(80, 24, 100);
    // CSI 4;1m must set BOTH underline and bold (not treated as 4:1)
    term.advance_bytes(b"\x1b[4;1mB\x1b[0m");
    let cell = term.grid.lines[0].cells[0];
    assert!(cell.flags.contains(CellFlags::UNDERLINE));
    assert!(cell.flags.contains(CellFlags::BOLD));
    assert!(!cell.flags.contains(CellFlags::UNDERLINE_DOUBLE));
    assert!(!cell.flags.contains(CellFlags::UNDERLINE_CURLY));
}

#[test]
fn test_parser_instance_reuse_across_invocations() {
    let mut term = Terminal::new(80, 24, 100);
    term.advance_bytes(b"A");
    assert_eq!(term.grid.lines[0].cells[0].c, 'A');
    term.advance_bytes(b"B");
    assert_eq!(term.grid.lines[0].cells[1].c, 'B');
    term.advance_bytes(b"\r\nC");
    assert_eq!(term.grid.lines[1].cells[0].c, 'C');
}

#[test]
fn test_stack_allocated_parameter_parsing_dense_sgr() {
    let mut term = Terminal::new(80, 24, 100);
    // Sequence with many chained parameters
    term.advance_bytes(b"\x1b[0;1;3;4;31;48;2;10;20;30;24;58;2;40;50;60mZ\x1b[0m");
    let cell = term.grid.lines[0].cells[0];
    assert_eq!(cell.c, 'Z');
    assert!(cell.flags.contains(CellFlags::BOLD));
    assert!(cell.flags.contains(CellFlags::ITALIC));
    assert_eq!(cell.fg, Color::Indexed(1));
    assert_eq!(cell.bg, Color::Rgb(10, 20, 30));
    assert_eq!(cell.underline_color, Color::Rgb(40, 50, 60));
}

#[test]
fn test_simd_ascii_scanning_and_batch_wrap() {
    let mut term = Terminal::new(10, 5, 100);
    // Write 15 printable ASCII characters: 10 on line 0, wraps 5 to line 1
    term.advance_bytes(b"0123456789ABCDE\n");
    for (i, c) in "0123456789".chars().enumerate() {
        assert_eq!(term.grid.lines[0].cells[i].c, c);
    }
    assert!(term.grid.lines[0].wrapped);
    for (i, c) in "ABCDE".chars().enumerate() {
        assert_eq!(term.grid.lines[1].cells[i].c, c);
    }

    // Now test with escape sequence interleaved
    term.advance_bytes(b"\x1b[31mRED\x1b[0mNORMAL\n");
    assert_eq!(term.grid.lines[2].cells[0].c, 'R');
    assert_eq!(term.grid.lines[2].cells[0].fg, Color::Indexed(1));
    assert_eq!(term.grid.lines[2].cells[3].c, 'N');
    assert_eq!(term.grid.lines[2].cells[3].fg, Color::DefaultForeground);

    // Test with non-ASCII unicode
    term.advance_bytes("你好\n".as_bytes());
    assert_eq!(term.grid.lines[3].cells[0].c, '你');
}

#[test]
fn test_c1_control_split_sequence() {
    let mut term = Terminal::new(10, 5, 100);
    // Split CSI sequence across chunks
    term.advance_bytes(b"\x1b[");
    assert!(term.parser_in_escape);
    term.advance_bytes(b"31mX");
    assert!(!term.parser_in_escape);
    assert_eq!(term.grid.lines[0].cells[0].c, 'X');
    assert_eq!(term.grid.lines[0].cells[0].fg, Color::Indexed(1));
}

#[test]
fn test_alt_screen_prompt_markers_not_recorded_in_primary() {
    let mut term = Terminal::new(80, 24, 100);
    term.advance_bytes(b"\x1b[?1049h");
    assert!(term.grid.is_alt_screen());
    term.advance_bytes(b"\x1b]133;A\x07");
    assert_eq!(term.grid.prompt_marks.len(), 0);
    term.advance_bytes(b"\x1b[?1049l");
    assert!(!term.grid.is_alt_screen());
    assert_eq!(term.grid.prompt_marks.len(), 0);
}

#[test]
fn test_dynamic_palette_and_colors_osc() {
    let mut term = Terminal::new(80, 24, 100);
    let init_fg = term.default_fg;
    let init_bg = term.default_bg;
    let init_p1 = term.palette[1];

    // 1. Set foreground via OSC 10
    term.advance_bytes(b"\x1b]10;#123456\x07");
    assert_eq!(term.default_fg, Rgb::new(0x12, 0x34, 0x56));
    assert!(term.palette_dirty);
    term.palette_dirty = false;

    // 2. Set background via OSC 11
    term.advance_bytes(b"\x1b]11;rgb:10/20/30\x07");
    assert_eq!(term.default_bg, Rgb::new(0x10, 0x20, 0x30));
    assert!(term.palette_dirty);
    term.palette_dirty = false;

    // 3. Reset foreground via OSC 110
    term.advance_bytes(b"\x1b]110\x07");
    assert_eq!(term.default_fg, init_fg);
    assert!(term.palette_dirty);
    term.palette_dirty = false;

    // 4. Reset background via OSC 111
    term.advance_bytes(b"\x1b]111\x07");
    assert_eq!(term.default_bg, init_bg);
    assert!(term.palette_dirty);
    term.palette_dirty = false;

    // 5. Set palette color 1 via OSC 4
    term.advance_bytes(b"\x1b]4;1;#abcdef\x07");
    assert_eq!(term.palette[1], Rgb::new(0xab, 0xcd, 0xef));
    assert!(term.palette_dirty);
    term.palette_dirty = false;

    // 6. Query palette color 1 via OSC 4;1;?
    term.advance_bytes(b"\x1b]4;1;?\x07");
    assert_eq!(
        term.take_responses(),
        vec![b"\x1b]4;1;rgb:abab/cdcd/efef\x1b\\".to_vec()]
    );

    // 7. Reset palette color 1 via OSC 104;1
    term.advance_bytes(b"\x1b]104;1\x07");
    assert_eq!(term.palette[1], init_p1);
    assert!(term.palette_dirty);
    term.palette_dirty = false;

    // 8. Reset all palette via OSC 104
    term.advance_bytes(b"\x1b]4;2;#112233\x07");
    assert_eq!(term.palette[2], Rgb::new(0x11, 0x22, 0x33));
    term.advance_bytes(b"\x1b]104\x07");
    assert_eq!(term.palette[2], term.initial_palette[2]);
}
