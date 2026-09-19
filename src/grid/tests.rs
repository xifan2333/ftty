use crate::color::Color;
use crate::grid::diacritics::KITTY_PLACEHOLDER;
use crate::grid::{CellFlags, ClearMode, Grid, Row};
use crate::kitty::{ImageData, ImagePlacement};

#[test]
fn test_grid_initialization() {
    let grid = Grid::new(80, 24, 100);
    assert_eq!(grid.cols, 80);
    assert_eq!(grid.rows, 24);
    assert_eq!(grid.lines.len(), 24);
    assert_eq!(grid.lines[0].cells.len(), 80);
    assert_eq!(grid.cursor.row, 0);
    assert_eq!(grid.cursor.col, 0);
}

#[test]
fn test_write_ascii_and_wide_char() {
    let mut grid = Grid::new(10, 2, 10);
    grid.write_char(
        'A',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    assert_eq!(grid.lines[0].cells[0].c, 'A');
    assert_eq!(grid.cursor.col, 1);

    grid.write_char(
        '中',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    assert_eq!(grid.lines[0].cells[1].c, '中');
    assert!(grid.lines[0].cells[1].flags.contains(CellFlags::WIDE_CHAR));
    assert!(
        grid.lines[0].cells[2]
            .flags
            .contains(CellFlags::WIDE_CHAR_SPACER)
    );
    assert_eq!(grid.cursor.col, 3);
}

#[test]
fn test_scrolling_and_scrollback() {
    let mut grid = Grid::new(10, 2, 5);
    grid.write_char(
        '1',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    grid.newline();
    grid.carriage_return();
    grid.write_char(
        '2',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    grid.newline(); // This triggers scroll up

    assert_eq!(grid.scrollback.len(), 1);
    assert_eq!(grid.scrollback[0].cells[0].c, '1');
    assert_eq!(grid.lines[0].cells[0].c, '2');
}

#[test]
fn test_alt_screen() {
    let mut grid = Grid::new(10, 2, 10);
    grid.write_char(
        'P',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    assert_eq!(grid.lines[0].cells[0].c, 'P');

    grid.enter_alt_screen();
    assert!(grid.is_alt_screen());
    assert_eq!(grid.lines[0].cells[0].c, ' ');

    grid.write_char(
        'A',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    assert_eq!(grid.lines[0].cells[0].c, 'A');

    grid.exit_alt_screen();
    assert!(!grid.is_alt_screen());
    assert_eq!(grid.lines[0].cells[0].c, 'P');
}

#[test]
fn test_viewport_scrolling_and_visible_lines() {
    let mut grid = Grid::new(10, 2, 100);
    // Write line 1
    grid.write_char(
        '1',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    grid.carriage_return();
    grid.newline();
    // Write line 2
    grid.write_char(
        '2',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    grid.carriage_return();
    grid.newline();
    // Write line 3
    grid.write_char(
        '3',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    grid.carriage_return();
    grid.newline();
    // Write line 4
    grid.write_char(
        '4',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );

    assert_eq!(grid.scrollback.len(), 2);
    assert_eq!(grid.viewport_offset(), 0);
    assert_eq!(grid.visible_line(0).cells[0].c, '3');
    assert_eq!(grid.visible_line(1).cells[0].c, '4');

    // Scroll up by 1
    grid.scroll_viewport_up(1);
    assert_eq!(grid.viewport_offset(), 1);
    assert_eq!(grid.visible_line(0).cells[0].c, '2');
    assert_eq!(grid.visible_line(1).cells[0].c, '3');

    // Scroll to top
    grid.scroll_viewport_top();
    assert_eq!(grid.viewport_offset(), 2);
    assert_eq!(grid.visible_line(0).cells[0].c, '1');
    assert_eq!(grid.visible_line(1).cells[0].c, '2');

    // Scroll down and to bottom
    grid.scroll_viewport_down(1);
    assert_eq!(grid.viewport_offset(), 1);
    grid.scroll_viewport_bottom();
    assert_eq!(grid.viewport_offset(), 0);
    assert_eq!(grid.visible_line(1).cells[0].c, '4');
}

#[test]
fn test_history_anchoring_on_new_output() {
    let mut grid = Grid::new(10, 2, 5);
    for c in ['1', '2', '3', '4'] {
        grid.write_char(
            c,
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
        grid.carriage_return();
        grid.newline();
    }

    // Viewport viewing '3' and '4'
    grid.scroll_viewport_up(1);
    assert_eq!(grid.visible_line(0).cells[0].c, '3');

    // New output arrives while scrolled back
    grid.write_char(
        '5',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    grid.carriage_return();
    grid.newline();

    // History anchor must keep visible lines identical
    assert_eq!(grid.visible_line(0).cells[0].c, '3');
}

#[test]
fn test_clear_saved_scrollback_resets_viewport_offset() {
    let mut grid = Grid::new(10, 2, 10);
    for c in ['1', '2', '3'] {
        grid.write_char(
            c,
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
        grid.carriage_return();
        grid.newline();
    }

    grid.scroll_viewport_up(1);
    assert_eq!(grid.viewport_offset(), 1);

    // CSI 3 J clears saved lines
    grid.clear_screen(ClearMode::Saved);
    assert_eq!(grid.viewport_offset(), 0);
    assert!(grid.scrollback.is_empty());
    // Must not panic on subsequent visible_line access
    assert_eq!(grid.visible_line(0).cells.len(), 10);
}

#[test]
fn test_clear_screen_marks_affected_rows_dirty() {
    let mut grid = Grid::new(10, 5, 10);
    // Clear all initial dirty flags
    for row in &grid.lines {
        row.dirty.set(false);
    }

    // 1. Clear Below at row 2
    grid.cursor.row = 2;
    grid.cursor.col = 3;
    grid.clear_screen(ClearMode::Below);
    assert!(!grid.lines[0].dirty.get());
    assert!(!grid.lines[1].dirty.get());
    assert!(grid.lines[2].dirty.get());
    assert!(grid.lines[3].dirty.get());
    assert!(grid.lines[4].dirty.get());

    // Reset dirty flags
    for row in &grid.lines {
        row.dirty.set(false);
    }

    // 2. Clear Above at row 2
    grid.clear_screen(ClearMode::Above);
    assert!(grid.lines[0].dirty.get());
    assert!(grid.lines[1].dirty.get());
    assert!(grid.lines[2].dirty.get());
    assert!(!grid.lines[3].dirty.get());
    assert!(!grid.lines[4].dirty.get());

    // Reset dirty flags
    for row in &grid.lines {
        row.dirty.set(false);
    }

    // 3. Clear Saved
    grid.clear_screen(ClearMode::Saved);
    for row in &grid.lines {
        assert!(row.dirty.get());
    }
}

#[test]
fn virtual_placement_images_are_preserved_across_evictions() {
    let mut grid = Grid::new(80, 24, 100);
    grid.virtual_placements.insert(100, (10, 10));
    grid.write_char(
        KITTY_PLACEHOLDER,
        Color::Rgb(0, 0, 100),
        Color::DefaultBackground,
        CellFlags::empty(),
    );

    for id in 1..=260 {
        grid.add_image(ImageData {
            id,
            width: 1,
            height: 1,
            rgba: vec![0, 0, 0, 0],
        });
    }

    assert!(
        grid.images.contains_key(&100),
        "virtual image 100 must be preserved across eviction"
    );
}

/// Writes `count` rows labelled `R0`, `R1`, … and leaves the cursor on the last one.
fn fill_rows(grid: &mut Grid, count: usize) {
    for row in 0..count {
        for c in format!("R{row}").chars() {
            grid.write_char(
                c,
                Color::DefaultForeground,
                Color::DefaultBackground,
                CellFlags::empty(),
            );
        }
        grid.carriage_return();
        if row + 1 < count {
            grid.newline();
        }
    }
}

#[test]
fn shrink_below_cursor_discards_bottom_rows_and_keeps_content() {
    let mut grid = Grid::new(10, 10, 100);
    fill_rows(&mut grid, 3);
    grid.cursor.row = 2;
    grid.cursor.col = 0;

    grid.resize(10, 8);

    assert_eq!(grid.rows, 8);
    assert_eq!(grid.lines.len(), 8);
    assert_eq!(
        grid.cursor.row, 2,
        "cursor must not move when nothing above it is cut"
    );
    assert_eq!(grid.visible_line(0).cells[0].c, 'R');
    assert_eq!(grid.visible_line(0).cells[1].c, '0');
    assert_eq!(grid.visible_line(2).cells[1].c, '2');
    assert!(
        grid.scrollback.is_empty(),
        "top content must not scroll away"
    );
}

#[test]
fn shrink_above_cursor_scrolls_top_rows_and_keeps_cursor_line() {
    let mut grid = Grid::new(10, 4, 100);
    fill_rows(&mut grid, 4);
    grid.cursor.row = 3;
    grid.cursor.col = 1;

    grid.resize(10, 2);

    assert_eq!(grid.rows, 2);
    assert_eq!(grid.cursor.row, 1, "cursor must follow its content up");
    assert!(grid.cursor.row < grid.rows);
    // The cursor's own row (`R3`) is the last surviving line.
    assert_eq!(grid.visible_line(1).cells[1].c, '3');
    assert_eq!(grid.scrollback.len(), 2, "two top rows move into history");
    assert_eq!(grid.scrollback[0].cells[1].c, '0');
}

#[test]
fn shrink_without_scrollback_discards_top_rows() {
    let mut grid = Grid::new(10, 4, 0);
    fill_rows(&mut grid, 4);
    grid.cursor.row = 3;

    grid.resize(10, 2);

    assert_eq!(grid.rows, 2);
    assert_eq!(grid.cursor.row, 1);
    assert!(grid.scrollback.is_empty());
    assert_eq!(grid.visible_line(1).cells[1].c, '3');
}

#[test]
fn shrink_on_alternate_screen_keeps_cursor_visible() {
    let mut grid = Grid::new(10, 4, 100);
    fill_rows(&mut grid, 2);
    grid.enter_alt_screen();
    fill_rows(&mut grid, 4);
    grid.cursor.row = 3;

    grid.resize(10, 2);

    assert!(grid.is_alt_screen());
    assert_eq!(grid.rows, 2);
    assert_eq!(grid.cursor.row, 1);
    assert!(
        grid.scrollback.is_empty(),
        "the alternate screen has no history"
    );
    // The primary screen is restored with its own cursor intact.
    grid.exit_alt_screen();
    assert_eq!(grid.rows, 2);
    assert!(grid.cursor.row < grid.rows);
}

#[test]
fn shrink_shifts_saved_cursor() {
    let mut grid = Grid::new(10, 4, 100);
    fill_rows(&mut grid, 4);
    grid.cursor.row = 3;
    grid.cursor.col = 2;
    grid.save_cursor();

    grid.resize(10, 2);

    assert_eq!(grid.cursor.row, 1);
    assert_eq!(
        grid.saved_cursor.row, 1,
        "the saved cursor follows the screen"
    );
}

#[test]
fn shrink_evicts_placements_below_the_new_bottom() {
    let mut grid = Grid::new(10, 4, 100);
    fill_rows(&mut grid, 4);
    // Keep the cursor near the top so the bottom row is what gets discarded.
    grid.cursor.row = 1;

    let scrollback_len = grid.scrollback.len();
    grid.add_placement(ImagePlacement {
        image_id: 1,
        placement_id: 0,
        line: scrollback_len + 3,
        col: 0,
        cols: 1,
        rows: 1,
        offset_x: 0,
        offset_y: 0,
        z_index: 0,
    });

    grid.resize(10, 2);

    assert_eq!(grid.cursor.row, 1);
    assert!(
        grid.placements.is_empty(),
        "a placement anchored below the new bottom row is evicted"
    );
}

#[test]
fn shrink_on_alternate_screen_evicts_truncated_placements() {
    let mut grid = Grid::new(10, 4, 100);
    grid.enter_alt_screen();
    fill_rows(&mut grid, 4);

    // Placement on row 1 (survives) and row 3 (truncated)
    grid.add_placement(ImagePlacement {
        image_id: 1,
        placement_id: 0,
        line: grid.scrollback.len() + 1,
        col: 0,
        cols: 1,
        rows: 1,
        offset_x: 0,
        offset_y: 0,
        z_index: 0,
    });
    grid.add_placement(ImagePlacement {
        image_id: 2,
        placement_id: 0,
        line: grid.scrollback.len() + 3,
        col: 0,
        cols: 1,
        rows: 1,
        offset_x: 0,
        offset_y: 0,
        z_index: 0,
    });

    grid.resize(10, 2);

    assert_eq!(
        grid.placements.len(),
        1,
        "only the surviving row placement is retained"
    );
    assert_eq!(
        grid.placements[0].line,
        grid.scrollback.len() + 1,
        "row 1 maintains its absolute row placement"
    );
}

#[test]
fn saved_cursor_is_scoped_to_its_screen_across_resize() {
    let mut grid = Grid::new(10, 6, 100);
    grid.cursor.row = 1;
    grid.cursor.col = 3;
    grid.save_cursor();

    // The alternate screen trims more top rows than the hidden primary would.
    grid.enter_alt_screen();
    grid.cursor.row = 5;
    grid.resize(10, 2);
    grid.exit_alt_screen();

    assert_eq!(
        grid.saved_cursor.row, 1,
        "the primary keeps its own saved cursor"
    );
    assert_eq!(grid.saved_cursor.col, 3);
}

#[test]
fn primary_placements_are_parked_while_the_alternate_screen_is_active() {
    let mut grid = Grid::new(10, 4, 100);
    grid.add_placement(ImagePlacement {
        image_id: 7,
        placement_id: 0,
        line: 2,
        col: 0,
        cols: 1,
        rows: 1,
        offset_x: 0,
        offset_y: 0,
        z_index: 0,
    });
    assert_eq!(grid.placements.len(), 1);

    grid.enter_alt_screen();
    assert!(
        grid.placements.is_empty(),
        "primary placements must not bleed onto the alternate screen"
    );

    grid.exit_alt_screen();
    assert_eq!(grid.placements.len(), 1);
    assert_eq!(grid.placements[0].line, 2);
}

#[test]
fn hidden_primary_images_survive_cache_eviction() {
    let mut grid = Grid::new(10, 4, 100);
    let image = ImageData {
        id: 42,
        width: 1,
        height: 1,
        rgba: vec![0, 0, 0, 0],
    };
    grid.add_image(image);
    grid.add_placement(ImagePlacement {
        image_id: 42,
        placement_id: 0,
        line: 1,
        col: 0,
        cols: 1,
        rows: 1,
        offset_x: 0,
        offset_y: 0,
        z_index: 0,
    });

    // The primary's placement is parked while the alternate screen churns the cache.
    grid.enter_alt_screen();
    for id in 1000..1300 {
        grid.add_image(ImageData {
            id,
            width: 1,
            height: 1,
            rgba: vec![0, 0, 0, 0],
        });
    }
    grid.exit_alt_screen();

    assert!(
        grid.images.contains_key(&42),
        "an image referenced by the hidden primary must not be evicted"
    );
    assert_eq!(grid.placements.len(), 1);
}

#[test]
fn clear_saved_history_rebases_hidden_primary_placements() {
    let mut grid = Grid::new(10, 2, 10);
    fill_rows(&mut grid, 4);
    assert_eq!(grid.scrollback.len(), 2);

    grid.add_placement(ImagePlacement {
        image_id: 5,
        placement_id: 0,
        line: grid.scrollback.len() + 1,
        col: 0,
        cols: 1,
        rows: 1,
        offset_x: 0,
        offset_y: 0,
        z_index: 0,
    });

    grid.enter_alt_screen();
    grid.clear_screen(ClearMode::Saved);
    grid.exit_alt_screen();

    assert_eq!(grid.scrollback.len(), 0);
    assert_eq!(
        grid.placements[0].line, 1,
        "the parked placement follows the cleared history"
    );
}

#[test]
fn hidden_saved_cursor_is_clamped_on_shrink() {
    let mut grid = Grid::new(10, 6, 100);
    grid.cursor.row = 1;
    grid.saved_cursor.row = 5;
    grid.enter_alt_screen();

    // Shrinking drops the hidden primary's bottom rows, so row 5 no longer exists.
    grid.resize(10, 2);
    // Growing back must not resurrect the discarded coordinate.
    grid.resize(10, 6);
    grid.exit_alt_screen();

    assert_eq!(
        grid.saved_cursor.row, 1,
        "a restored saved cursor must be clamped to the surviving screen"
    );
}

#[test]
fn grow_appends_rows_and_keeps_cursor_and_scrollback() {
    let mut grid = Grid::new(10, 2, 100);
    fill_rows(&mut grid, 2);
    grid.cursor.row = 1;
    grid.cursor.col = 1;

    grid.resize(12, 5);

    assert_eq!(grid.rows, 5);
    assert_eq!(grid.cols, 12);
    assert_eq!(grid.lines.len(), 5);
    assert_eq!(grid.cursor.row, 1);
    assert_eq!(grid.visible_line(1).cells[1].c, '1');
    assert_eq!(grid.lines[1].cells.len(), 12);
    assert!(grid.scrollback.is_empty());
}

#[test]
fn test_shrink_and_grow_restores_scrollback_text() {
    let mut grid = Grid::new(10, 4, 100);
    fill_rows(&mut grid, 4);
    grid.cursor.row = 3;
    grid.cursor.col = 1;

    // Shrink from 4 to 2 rows: top 2 rows move to scrollback
    grid.resize(10, 2);
    assert_eq!(grid.rows, 2);
    assert_eq!(grid.cursor.row, 1);
    assert_eq!(grid.scrollback.len(), 2);

    // Grow back from 2 to 4 rows: top 2 rows must be pulled back from scrollback!
    grid.resize(10, 4);
    assert_eq!(grid.rows, 4);
    assert_eq!(grid.cursor.row, 3);
    assert_eq!(grid.scrollback.len(), 0);
    assert_eq!(grid.visible_line(0).cells[1].c, '0');
    assert_eq!(grid.visible_line(1).cells[1].c, '1');
    assert_eq!(grid.visible_line(2).cells[1].c, '2');
    assert_eq!(grid.visible_line(3).cells[1].c, '3');
}

#[test]
fn test_horizontal_shrink_and_grow_pads_with_default_cells() {
    let mut grid = Grid::new(20, 2, 100);
    let text = "Hello World 12345";
    for c in text.chars() {
        grid.write_char(
            c,
            crate::color::Color::DefaultForeground,
            crate::color::Color::DefaultBackground,
            CellFlags::empty(),
        );
    }
    assert_eq!(grid.visible_line(0).cells[16].c, '5');

    // Shrink horizontally to 5 columns: line reflows and wraps across rows
    grid.resize(5, 2);
    assert_eq!(grid.cols, 5);
    assert_eq!(grid.visible_line(0).cells.len(), 5);

    // Grow horizontally back to 20 columns: wrapped rows unwrap and restore the full string
    grid.resize(20, 2);
    assert_eq!(grid.cols, 20);
    assert_eq!(grid.visible_line(0).cells.len(), 20);
    let full: String = grid.visible_line(0).cells[..17]
        .iter()
        .map(|c| c.c)
        .collect();
    assert_eq!(full, "Hello World 12345");
    for cell in &grid.visible_line(0).cells[17..] {
        assert_eq!(cell.c, ' ');
    }
}

#[test]
fn test_horizontal_shrink_clears_split_wide_character() {
    let mut grid = Grid::new(10, 1, 100);
    grid.cursor.col = 4;
    // Write 2-column wide character '你' at col 4 and 5
    grid.write_char(
        '你',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    assert!(
        grid.visible_line(0).cells[4]
            .flags
            .contains(CellFlags::WIDE_CHAR)
    );
    assert!(
        grid.visible_line(0).cells[5]
            .flags
            .contains(CellFlags::WIDE_CHAR_SPACER)
    );

    // Shrink to 5 columns: wraps rather than splitting the 2-column wide character
    grid.resize(5, 1);
    assert_eq!(grid.cols, 5);

    // Grow back to 10 columns: unwraps and preserves the wide character intact
    grid.resize(10, 1);
    assert_eq!(grid.visible_line(0).cells[4].c, '你');
    assert!(
        grid.visible_line(0).cells[4]
            .flags
            .contains(CellFlags::WIDE_CHAR)
    );
    assert!(
        grid.visible_line(0).cells[5]
            .flags
            .contains(CellFlags::WIDE_CHAR_SPACER)
    );
}

#[test]
fn test_alt_screen_resize_preserves_absolute_row_coordinates() {
    let mut grid = Grid::new(20, 5, 100);
    grid.enter_alt_screen();

    for r in 0..5 {
        grid.cursor.row = r;
        grid.cursor.col = 0;
        let c = char::from_digit(r as u32, 10).unwrap();
        grid.write_char(
            c,
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
    }

    // Shrink to 3 rows on alternate screen: row 0, 1, 2 stay intact (never drained from top)
    grid.resize(20, 3);
    assert_eq!(grid.rows, 3);
    assert_eq!(grid.lines.len(), 3);
    assert_eq!(grid.visible_line(0).cells[0].c, '0');
    assert_eq!(grid.visible_line(1).cells[0].c, '1');
    assert_eq!(grid.visible_line(2).cells[0].c, '2');

    // Grow back to 5 rows on alternate screen: bottom rows appended as clean blanks
    grid.resize(20, 5);
    assert_eq!(grid.rows, 5);
    assert_eq!(grid.lines.len(), 5);
    assert_eq!(grid.visible_line(0).cells[0].c, '0');
    assert_eq!(grid.visible_line(1).cells[0].c, '1');
    assert_eq!(grid.visible_line(2).cells[0].c, '2');
    assert_eq!(grid.visible_line(3).cells[0].c, ' ');
    assert_eq!(grid.visible_line(4).cells[0].c, ' ');
}

#[test]
fn test_image_placements_pool_has_bounded_capacity() {
    let mut grid = Grid::new(80, 24, 100);
    for i in 0..1100 {
        grid.add_placement(ImagePlacement {
            image_id: i as u32,
            placement_id: 0,
            line: 0,
            col: 0,
            cols: 1,
            rows: 1,
            offset_x: 0,
            offset_y: 0,
            z_index: 0,
        });
    }
    assert_eq!(grid.placements.len(), 1024);
    // Oldest placements 0..76 were evicted by FIFO
    assert_eq!(grid.placements[0].image_id, 76);
}

#[test]
fn test_high_volume_scrolling_recycles_rows_and_bounds_dirty_scan() {
    let mut grid = Grid::new(80, 24, 100);
    // Write 5000 lines to exhaust scrollback and force repeated steady-state recycling
    for i in 0..5000 {
        grid.write_char(
            'X',
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
        grid.carriage_return();
        grid.newline();
        // Visible lines must remain within active screen bounds
        assert_eq!(grid.lines.len(), 24);
        assert!(grid.scrollback.len() <= 100);
        // Only active screen lines are marked dirty, not the entire scrollback
        assert!(grid.lines[23].dirty.get());
        if i % 500 == 0 {
            grid.mark_all_dirty();
        }
    }
    assert_eq!(grid.scrollback.len(), 100);
    assert_eq!(grid.lines.len(), 24);
}

#[test]
fn test_saturated_row_recycling_rebases_image_placements() {
    let mut grid = Grid::new(80, 2, 5);
    // Fill screen and scrollback to capacity (5 scrollback lines)
    for _ in 0..7 {
        grid.newline();
    }
    assert_eq!(grid.scrollback.len(), 5);

    // Add placement at line 0 (oldest line, should be evicted on next scroll)
    grid.add_placement(ImagePlacement {
        image_id: 1,
        placement_id: 0,
        line: 0,
        col: 0,
        cols: 1,
        rows: 1,
        offset_x: 0,
        offset_y: 0,
        z_index: 0,
    });

    // Add placement at line 3 (should be decremented to line 2 on next scroll)
    grid.add_placement(ImagePlacement {
        image_id: 2,
        placement_id: 0,
        line: 3,
        col: 0,
        cols: 1,
        rows: 1,
        offset_x: 0,
        offset_y: 0,
        z_index: 0,
    });

    // Trigger saturated row recycling
    grid.scroll_up(1);

    // Placement 1 on line 0 must be evicted
    assert!(!grid.placements.iter().any(|p| p.image_id == 1));

    // Placement 2 originally on line 3 must be decremented to line 2
    let p2 = grid.placements.iter().find(|p| p.image_id == 2).unwrap();
    assert_eq!(p2.line, 2);
}

#[test]
fn test_vectorized_clear_line_and_reset_clears_cells() {
    let mut grid = Grid::new(20, 4, 10);
    for r in 0..4 {
        for c in 0..20 {
            grid.cursor.row = r;
            grid.cursor.col = c;
            grid.write_char(
                'Z',
                Color::Indexed(1),
                Color::Indexed(2),
                CellFlags::BOLD | CellFlags::UNDERLINE,
            );
        }
    }

    // Clear Below on line 1 from col 5
    grid.cursor.row = 1;
    grid.cursor.col = 5;
    grid.clear_line(ClearMode::Below);
    assert_eq!(grid.lines[1].cells[4].c, 'Z');
    assert_eq!(grid.lines[1].cells[5].c, ' ');
    assert_eq!(grid.lines[1].cells[19].c, ' ');

    // Clear Above on line 2 up to col 10
    grid.cursor.row = 2;
    grid.cursor.col = 10;
    grid.clear_line(ClearMode::Above);
    assert_eq!(grid.lines[2].cells[0].c, ' ');
    assert_eq!(grid.lines[2].cells[10].c, ' ');
    assert_eq!(grid.lines[2].cells[11].c, 'Z');

    // Reset row 3
    grid.lines[3].reset();
    assert!(grid.lines[3].cells.iter().all(|c| c.c == ' '));
    assert!(grid.lines[3].dirty.get());
}

#[test]
fn test_auto_scroll_viewport_bottom_bounds_dirty_marking() {
    let mut grid = Grid::new(80, 24, 100);
    // Initially at bottom (viewport_offset == 0)
    for row in &grid.lines {
        row.dirty.set(false);
    }
    // Calling scroll_viewport_bottom when already at bottom must be a no-op and preserve clean state
    grid.scroll_viewport_bottom();
    assert_eq!(grid.viewport_offset(), 0);
    assert!(!grid.lines[0].dirty.get());

    // Scroll up into history
    grid.scrollback.push_back(Row::new(80));
    grid.scroll_viewport_up(1);
    assert_eq!(grid.viewport_offset(), 1);

    // Resetting viewport to bottom marks lines dirty
    grid.scroll_viewport_bottom();
    assert_eq!(grid.viewport_offset(), 0);
    assert!(grid.lines[0].dirty.get());
}

#[test]
fn test_ascii_fast_path_writing_and_wrapping() {
    let mut grid = Grid::new(10, 3, 10);
    let s = "0123456789ABC";
    for c in s.chars() {
        grid.write_char(c, Color::Indexed(7), Color::Indexed(0), CellFlags::empty());
    }
    // "0123456789" fits on row 0, then "ABC" wraps to row 1
    assert_eq!(grid.lines[0].cells[0].c, '0');
    assert_eq!(grid.lines[0].cells[9].c, '9');
    assert_eq!(grid.lines[1].cells[0].c, 'A');
    assert_eq!(grid.lines[1].cells[1].c, 'B');
    assert_eq!(grid.lines[1].cells[2].c, 'C');
    assert_eq!(grid.cursor.row, 1);
    assert_eq!(grid.cursor.col, 3);
}

#[test]
fn test_cell_memory_footprint() {
    assert!(std::mem::size_of::<crate::grid::Cell>() <= 24);
}

#[test]
fn test_prompt_marks_navigation_and_rebasing() {
    let mut grid = Grid::new(20, 5, 10);
    grid.add_prompt_mark(0);
    grid.add_prompt_mark(2);

    grid.scroll_up(3);
    assert_eq!(grid.scrollback.len(), 3);
    assert_eq!(grid.viewport_offset, 0);

    grid.scroll_to_prompt_prev();
    assert!(grid.viewport_offset > 0);

    grid.scroll_to_prompt_next();
    assert_eq!(grid.viewport_offset, 0);
}

#[test]
fn test_clear_screen_rebases_and_clears_prompt_marks() {
    let mut grid = Grid::new(20, 5, 10);
    grid.add_prompt_mark(0);
    grid.add_prompt_mark(5);
    grid.scroll_up(3);

    grid.clear_screen(ClearMode::All);
    assert!(grid.prompt_marks.iter().all(|&m| m < grid.scrollback.len()));

    let active_mark = grid.scrollback.len() + 1;
    grid.add_prompt_mark(active_mark);
    let old_sb = grid.scrollback.len();
    grid.clear_screen(ClearMode::Saved);
    assert_eq!(grid.scrollback.len(), 0);
    assert!(grid.prompt_marks.contains(&(active_mark - old_sb)));
}

#[test]
fn test_shrink_and_expand_preserves_long_lines_without_truncation() {
    let mut grid = Grid::new(80, 5, 100);
    let line1 = "Permissions Size User Date Modified Name";
    let line2 = "drwxr-xr-x     - xifan 14 Sep 02:31 Code";

    for c in line1.chars() {
        grid.write_char(
            c,
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
    }
    grid.newline();
    grid.cursor.col = 0;

    for c in line2.chars() {
        grid.write_char(
            c,
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
    }

    // Shrink horizontally to 25 columns (as in user screenshot)
    grid.resize(25, 10);
    assert_eq!(grid.cols, 25);

    // Expand back to 80 columns: text must unwrap and be 100% preserved without any truncation
    grid.resize(80, 5);
    assert_eq!(grid.cols, 80);

    let read_l1: String = grid.visible_line(0).cells[..line1.len()]
        .iter()
        .map(|c| c.c)
        .collect();
    assert_eq!(read_l1, line1);

    let read_l2: String = grid.visible_line(1).cells[..line2.len()]
        .iter()
        .map(|c| c.c)
        .collect();
    assert_eq!(read_l2, line2);
}

#[test]
fn test_reflow_shrink_to_one_column_preserves_wide_characters() {
    let mut grid = Grid::new(10, 2, 100);
    grid.write_char(
        '你',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    grid.write_char(
        '好',
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );

    // Shrink to 1 column: wide characters cannot fit on a single column, but must not be deleted
    grid.resize(1, 10);
    assert_eq!(grid.cols, 1);

    // Expand back to 10 columns: both wide characters must be fully restored
    grid.resize(10, 2);
    assert_eq!(grid.cols, 10);
    assert_eq!(grid.visible_line(0).cells[0].c, '你');
    assert_eq!(grid.visible_line(0).cells[2].c, '好');
}

#[test]
fn test_reflow_exact_width_cursor_tracking() {
    let mut grid = Grid::new(5, 2, 100);
    for c in "12345".chars() {
        grid.write_char(
            c,
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
    }
    // Cursor is right at the boundary (deferred wrap at col 5)
    assert_eq!(grid.cursor.col, 5);

    // Resize to 10 columns: cursor must follow the logical line ending rather than jumping
    grid.resize(10, 2);
    assert_eq!(grid.cols, 10);
    assert_eq!(grid.cursor.row, 0);
    assert_eq!(grid.cursor.col, 5);
}

#[test]
fn test_reflow_combined_resize_preserves_cursor_row_on_screen() {
    let mut grid = Grid::new(20, 10, 100);
    for r in 0..8 {
        grid.cursor.row = r;
        grid.cursor.col = 0;
        grid.write_char(
            'A',
            Color::DefaultForeground,
            Color::DefaultBackground,
            CellFlags::empty(),
        );
    }
    // Place cursor in middle of content (row 4)
    grid.cursor.row = 4;
    grid.cursor.col = 5;

    // Simultaneously shrink width and height: cursor row must remain on the visible screen
    grid.resize(10, 4);
    assert_eq!(grid.cols, 10);
    assert_eq!(grid.rows, 4);
    assert!(
        grid.cursor.row < 4,
        "cursor row {} must be within new screen height 4",
        grid.cursor.row
    );
}

#[test]
fn test_reflow_preserves_image_placeholders() {
    let mut grid = Grid::new(20, 2, 100);
    grid.write_char(
        KITTY_PLACEHOLDER,
        Color::DefaultForeground,
        Color::DefaultBackground,
        CellFlags::empty(),
    );
    grid.lines[0].placeholders = Some(std::collections::HashMap::from([(0, (42, 99, 1))]));

    // Resize narrower
    grid.resize(10, 2);
    assert_eq!(grid.visible_line(0).cells[0].c, KITTY_PLACEHOLDER);
    assert_eq!(
        grid.visible_line(0).placeholders.as_ref().unwrap().get(&0),
        Some(&(42, 99, 1))
    );

    // Resize wider
    grid.resize(25, 2);
    assert_eq!(grid.visible_line(0).cells[0].c, KITTY_PLACEHOLDER);
    assert_eq!(
        grid.visible_line(0).placeholders.as_ref().unwrap().get(&0),
        Some(&(42, 99, 1))
    );
}
