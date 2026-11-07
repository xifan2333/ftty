//! Plaintext URL detection and grid coordinate extraction.
//!
//! Aligned with industry-standard terminal implementations (Foot, Alacritty, Ghostty),
//! URL detection uses a declarative regex matching arbitrary hierarchical schemes (`<scheme>://`)
//! including developer DeepLinks, while enforcing strict RFC 3986 ASCII character boundaries
//! and balanced delimiter pairing.

use regex::Regex;
use std::sync::LazyLock;

use crate::grid::{CellFlags, Grid};

/// Declarative regular expression for RFC 3986 hierarchical URIs.
///
/// 1. `(?:^|[^a-zA-Z0-9+.-])`: Preceding boundary must be start-of-text or non-scheme char.
/// 2. `(?P<url>...)`: Named capture group containing the actual URL.
/// 3. `[a-zA-Z][a-zA-Z0-9+.-]*://`: Any valid RFC 3986 scheme (e.g. `https://`, `vscode://`).
/// 4. Body: RFC 3986 permitted characters or balanced parentheses `\([^()\s]*\)` / brackets `\[[^\[\]\s]*\]`.
/// 5. Tail: Excludes trailing punctuation (`.`, `,`, `:`, `;`, `!`, `?`, `'`, `"`) and unbalanced brackets.
static URL_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    match Regex::new(
        r"(?:^|[^a-zA-Z0-9+.-])(?P<url>[a-zA-Z][a-zA-Z0-9+.-]*://(?:[0-9a-zA-Z:/?#@!$&*+,;=.~_%^\-]+|\([^()\s]*\)|\[[^\[\]\s]*\])*(?:[0-9a-zA-Z/#@$&*+=~_%^\-]|\([^()\s]*\)|\[[^\[\]\s]*\]))",
    ) {
        Ok(re) => re,
        Err(_) => panic!("valid URL regex pattern"),
    }
});

/// Extracts a plaintext URL across wrapped lines in `grid` containing `(target_line, target_screen_row, target_col)`.
///
/// Returns `(start_col, end_col, url)` where `start_col..=end_col` is the span on `target_line`.
#[must_use]
pub fn find_url_in_grid(
    grid: &Grid,
    target_line: usize,
    target_screen_row: usize,
    target_col: usize,
) -> Option<(usize, usize, String)> {
    let mut start_row = target_screen_row;
    while start_row > 0 {
        if grid.visible_line(start_row - 1).wrapped {
            start_row -= 1;
        } else {
            break;
        }
    }
    let mut end_row = target_screen_row;
    while end_row + 1 < grid.rows {
        if grid.visible_line(end_row).wrapped {
            end_row += 1;
        } else {
            break;
        }
    }

    let mut text = String::new();
    let mut coords: Vec<(usize, usize, usize)> = Vec::new(); // (abs_line, col, width)

    for r in start_row..=end_row {
        let line = grid.visible_line(r);
        let abs_line = grid.scrollback.len() + r - grid.viewport_offset;
        for (c_idx, cell) in line.cells.iter().enumerate() {
            if cell.flags.contains(CellFlags::HIDDEN) {
                text.push(' ');
                coords.push((abs_line, c_idx, 1));
            } else if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                // Skip wide character continuation spacers
            } else {
                let width = if cell.flags.contains(CellFlags::WIDE_CHAR) {
                    2
                } else {
                    1
                };
                if let Some(cluster) = line.cluster_text(c_idx) {
                    for ch in cluster.chars() {
                        text.push(ch);
                        coords.push((abs_line, c_idx, width));
                    }
                } else {
                    text.push(cell.c);
                    coords.push((abs_line, c_idx, width));
                }
            }
        }
    }

    for cap in URL_REGEX.captures_iter(&text) {
        let Some(m) = cap.name("url") else {
            continue;
        };
        let start = m.start();
        let end = m.end();

        let start_char_idx = text[..start].chars().count();
        let end_char_idx = text[..end].chars().count();

        if start_char_idx < coords.len() && end_char_idx <= coords.len() {
            let url_coords = &coords[start_char_idx..end_char_idx];
            let is_hit = url_coords
                .iter()
                .any(|&(line, c, w)| line == target_line && target_col >= c && target_col < c + w);

            if is_hit {
                let mut min_col = usize::MAX;
                let mut max_col = 0;
                for &(line, c, w) in url_coords {
                    if line == target_line {
                        min_col = min_col.min(c);
                        max_col = max_col.max(c + w - 1);
                    }
                }
                if min_col <= max_col {
                    return Some((min_col, max_col, m.as_str().to_string()));
                }
            }
        }
    }

    None
}

/// Extracts a plaintext URL and its column span on a given row if `col` falls within it.
#[must_use]
#[cfg(test)]
pub fn find_url_at_col(row: &crate::grid::Row, col: usize) -> Option<(usize, usize, String)> {
    let mut grid = Grid::new(row.cells.len(), 1, 0);
    grid.lines[0] = row.clone();
    find_url_in_grid(&grid, 0, 0, col)
}
