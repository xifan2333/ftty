use std::collections::HashMap;

use crate::color::Color;
use crate::grid::diacritics::KITTY_PLACEHOLDER;
use crate::grid::row::MAX_ROW_OVERFLOW;
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
fn shrink_on_alternate_screen_shifts_placements() {
    let mut grid = Grid::new(10, 4, 100);
    grid.enter_alt_screen();
    fill_rows(&mut grid, 4);
    grid.cursor.row = 3;

    // The alternate screen has no history, so its content shifts up on shrink.
    grid.add_placement(ImagePlacement {
        image_id: 1,
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
        "the placement must not be dropped"
    );
    assert_eq!(
        grid.placements[0].line,
        grid.scrollback.len() + 1,
        "it follows its row up the alternate screen"
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
fn test_horizontal_shrink_and_grow_preserves_overflow_cells() {
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

    // Shrink horizontally to 5 columns
    grid.resize(5, 2);
    assert_eq!(grid.cols, 5);
    assert_eq!(grid.visible_line(0).cells[0].c, 'H');
    assert_eq!(grid.visible_line(0).cells[4].c, 'o');

    // Grow horizontally back to 20 columns: overflow cells restored!
    grid.resize(20, 2);
    assert_eq!(grid.cols, 20);
    let restored: String = grid.visible_line(0).cells[..17]
        .iter()
        .map(|c| c.c)
        .collect();
    assert_eq!(restored, "Hello World 12345");
}

#[test]
fn test_shrink_and_grow_restores_placeholder_coordinates_and_bounds_overflow() {
    let mut row = Row::new(300);
    row.placeholders = Some(HashMap::from([(50, (1, 2, 3))]));

    // Shrink to 40 columns (excess = 260)
    row.resize(40);
    assert_eq!(row.cells.len(), 40);
    // Placeholder at 50 is beyond 40, so it's stashed in overflow_placeholders at offset 10
    assert!(row.placeholders.as_ref().unwrap().is_empty());
    assert_eq!(row.overflow.len(), MAX_ROW_OVERFLOW); // Bounded to 256!
    assert_eq!(row.overflow_placeholders, vec![(10, (1, 2, 3))]);

    // Grow back to 60 columns: restores 20 overflow cells
    row.resize(60);
    assert_eq!(row.cells.len(), 60);
    // Offset 10 is restored to column 40 + 10 = 50!
    assert_eq!(
        row.placeholders.as_ref().unwrap().get(&50),
        Some(&(1, 2, 3))
    );
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
